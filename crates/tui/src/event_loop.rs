//! The event loop.
//!
//! Owns the runtime loop mechanics:

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::EventStream;
use futures_util::StreamExt;
use tokio::sync::mpsc;

use crate::component::ActionStatus;
use crate::context::{Ctx, MessageTarget, RuntimeState};
use crate::entity::{EntityId, EntityStore};
use crate::error::RuntimeError;
use crate::keymap::{InputContext, KeyMapper};
use crate::render;
use crate::task::{TaskDelivery, TaskError};
use crate::terminal::TerminalGuard;

/// Dispatch one action through the routing priority: the topmost overlay is
/// a modal boundary; otherwise the focused entity bubbles through its parents
/// to the root. An action is never broadcast to siblings.
fn dispatch<A: 'static, M: 'static>(
    action: &A,
    core: &mut RuntimeState<A, M>,
    store: &EntityStore<A, M>,
    root: EntityId,
) {
    let initial_target = core.input_target();

    if core.overlays.is_active() {
        if let Some(overlay) = initial_target {
            let mut cx = Ctx::new(core, store, overlay);
            let _ = store.dispatch_action(overlay, action, &mut cx);
        }
        return;
    }

    let mut target = initial_target;
    while let Some(id) = target {
        let mut cx = Ctx::new(core, store, id);
        if store.dispatch_action(id, action, &mut cx) != Some(ActionStatus::Continue) {
            return;
        }
        if id == root {
            return;
        }
        target = core.parent_map.get(&id).copied();
    }

    let mut cx = Ctx::new(core, store, root);
    let _ = store.dispatch_action(root, action, &mut cx);
}

/// Apply queued requests until initialization and entity creation reach a
/// fixed point, then close overlays that were requested during callbacks.
fn flush_requests<A: 'static, M: 'static>(
    core: &mut RuntimeState<A, M>,
    store: &mut EntityStore<A, M>,
) {
    loop {
        while let Some((id, slot)) = core.pending_inserts.pop_front() {
            store.insert_slot(id, slot);
        }
        let pending_overlays = std::mem::take(&mut core.pending_overlays);
        for (id, slot) in pending_overlays {
            store.insert_slot(id, slot);
            // Save the underlying focus before initialization can focus the
            // overlay or queue a nested overlay.
            core.focus.save();
            let mut cx = Ctx::new(core, store, id);
            store.init_if_needed(id, &mut cx);
            core.overlays.push(id);
        }
        let pending_inits = std::mem::take(&mut core.pending_inits);
        for id in pending_inits {
            let mut cx = Ctx::new(core, store, id);
            store.init_if_needed(id, &mut cx);
        }
        if core.pending_inserts.is_empty()
            && core.pending_overlays.is_empty()
            && core.pending_inits.is_empty()
        {
            break;
        }
    }

    while core.pending_closes > 0 {
        core.pending_closes -= 1;
        let Some(id) = core.overlays.pop() else {
            continue;
        };
        core.focus.restore();
        remove_entity_tree(core, store, id);
    }
}

fn remove_entity_tree<A: 'static, M: 'static>(
    core: &mut RuntimeState<A, M>,
    store: &mut EntityStore<A, M>,
    root: EntityId,
) {
    let mut removed = vec![root];
    let mut index = 0;
    while index < removed.len() {
        let parent = removed[index];
        let children: Vec<_> = core
            .parent_map
            .iter()
            .filter_map(|(child, owner)| (*owner == parent).then_some(*child))
            .collect();
        removed.extend(children);
        index += 1;
    }

    for id in removed {
        core.focus.remove_entity(id);
        core.focus_map.retain(|_, owner| *owner != id);
        core.parent_map.remove(&id);
        store.remove_entity(id);
    }
}

/// Deliver one batch of queued messages to their destinations. Messages
/// queued by a handler are left for the next batch, preventing re-entry.
fn deliver_messages<A: 'static, M: 'static>(
    core: &mut RuntimeState<A, M>,
    store: &EntityStore<A, M>,
    root: EntityId,
) {
    for queued in core.take_messages() {
        let target = match queued.target {
            MessageTarget::Root => root,
            MessageTarget::Entity(id) => id,
        };
        let mut cx = Ctx::new(core, store, target);
        store.deliver_message(target, queued.message, &mut cx);
    }
}

/// Deliver a completed task result to its spawning entity.
fn deliver_task<A: 'static, M: 'static>(
    delivery: TaskDelivery<M>,
    core: &mut RuntimeState<A, M>,
    store: &EntityStore<A, M>,
) -> Result<(), RuntimeError> {
    let TaskDelivery { target, result } = delivery;
    match result {
        Ok(message) => {
            let mut cx = Ctx::new(core, store, target);
            store.deliver_message(target, message, &mut cx);
            Ok(())
        }
        Err(TaskError(error)) => Err(RuntimeError::Task(error)),
    }
}

/// Run the loop until the application quits.
pub(crate) async fn event_loop<A, M>(
    mut guard: TerminalGuard,
    root: EntityId,
    entity_store: Arc<Mutex<EntityStore<A, M>>>,
    mut runtime_state: RuntimeState<A, M>,
    key_mapper: Arc<dyn KeyMapper<A>>,
    mut task_rx: mpsc::UnboundedReceiver<TaskDelivery<M>>,
    tick_rate: Duration,
) -> Result<(), RuntimeError>
where
    A: Send + 'static,
    M: Send + 'static,
{
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(tick_rate);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        {
            let mut store = entity_store.lock().expect("entity store poisoned");
            flush_requests(&mut runtime_state, &mut store);
            deliver_messages(&mut runtime_state, &store, root);
            flush_requests(&mut runtime_state, &mut store);

            if runtime_state.take_dirty() {
                guard
                    .terminal()
                    .draw(|frame| render::draw(root, &runtime_state.overlays, &store, frame))?;
            }
        }

        let input_context = InputContext {
            overlay_active: runtime_state.overlays.is_active(),
            focus_active: runtime_state.focus.current().is_some(),
        };
        tokio::select! {
            maybe_event = events.next() => {
                let Some(result) = maybe_event else {
                    break;
                };
                let event = result?;
                if let Some(action) = key_mapper.map(&event, &input_context) {
                    let store = entity_store.lock().expect("entity store poisoned");
                    dispatch(&action, &mut runtime_state, &store, root);
                }
            }
            _ = tick.tick() => {
                runtime_state.mark_dirty();
            }
            delivery = task_rx.recv() => {
                let Some(delivery) = delivery else {
                    break;
                };
                let store = entity_store.lock().expect("entity store poisoned");
                deliver_task(delivery, &mut runtime_state, &store)?;
            }
        }

        if runtime_state.should_quit() {
            break;
        }
    }

    // Final flush so quit-time messages and overlay closes still take effect.
    {
        let mut store = entity_store.lock().expect("entity store poisoned");
        flush_requests(&mut runtime_state, &mut store);
        deliver_messages(&mut runtime_state, &store, root);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::component::{Component, RenderContext};
    use crate::context::Context;
    use ratatui::Frame;
    use ratatui::layout::Rect;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone, Copy)]
    struct TestAction;

    #[derive(Debug)]
    enum TestMessage {
        First,
        Second,
    }

    struct MessageProbe {
        log: Arc<Mutex<Vec<&'static str>>>,
    }

    impl Component<TestAction, TestMessage> for MessageProbe {
        fn handle_message(
            &mut self,
            message: TestMessage,
            cx: &mut Context<'_, Self, TestAction, TestMessage>,
        ) {
            match message {
                TestMessage::First => {
                    self.log.lock().unwrap().push("first");
                    cx.send(cx.entity(), TestMessage::Second);
                }
                TestMessage::Second => self.log.lock().unwrap().push("second"),
            }
        }

        fn render(
            &self,
            _frame: &mut Frame,
            _area: Rect,
            _cx: &RenderContext<'_, TestAction, TestMessage>,
        ) {
        }
    }

    #[test]
    fn targeted_messages_are_fifo_and_not_reentrant() {
        let mut store = EntityStore::new();
        let log = Arc::new(Mutex::new(Vec::new()));
        let root = store.insert(MessageProbe {
            log: Arc::clone(&log),
        });
        let child = store.insert(MessageProbe {
            log: Arc::clone(&log),
        });
        let (sender, _receiver) = mpsc::unbounded_channel();
        let mut core = RuntimeState::new(sender, Arc::new(crate::task::TokioExecutor));

        {
            let mut cx = Ctx::new(&mut core, &store, root.id());
            cx.typed::<MessageProbe>().send(child, TestMessage::First);
        }
        deliver_messages(&mut core, &store, root.id());
        assert_eq!(&*log.lock().unwrap(), &["first"]);
        deliver_messages(&mut core, &store, root.id());
        assert_eq!(&*log.lock().unwrap(), &["first", "second"]);
    }

    struct RouteProbe {
        label: &'static str,
        calls: Arc<Mutex<Vec<&'static str>>>,
        propagation: ActionStatus,
    }

    impl Component<TestAction, ()> for RouteProbe {
        fn handle_action(
            &mut self,
            _action: &TestAction,
            _cx: &mut Context<'_, Self, TestAction, ()>,
        ) -> ActionStatus {
            self.calls.lock().unwrap().push(self.label);
            self.propagation
        }

        fn render(&self, _frame: &mut Frame, _area: Rect, _cx: &RenderContext<'_, TestAction, ()>) {
        }
    }

    #[test]
    fn action_bubbles_to_root_without_broadcasting_to_siblings() {
        let mut store = EntityStore::new();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let root = store.insert(RouteProbe {
            label: "root",
            calls: Arc::clone(&calls),
            propagation: ActionStatus::Handled,
        });
        let focused = store.insert(RouteProbe {
            label: "focused",
            calls: Arc::clone(&calls),
            propagation: ActionStatus::Continue,
        });
        let sibling = store.insert(RouteProbe {
            label: "sibling",
            calls: Arc::clone(&calls),
            propagation: ActionStatus::Handled,
        });
        let handle = crate::FocusHandle::new();
        let (sender, _receiver) = mpsc::unbounded_channel();
        let mut core = RuntimeState::new(sender, Arc::new(crate::task::TokioExecutor));
        core.parent_map.insert(focused.id(), root.id());
        core.parent_map.insert(sibling.id(), root.id());
        core.focus_map.insert(handle.id(), focused.id());
        core.focus.focus(handle);

        dispatch(&TestAction, &mut core, &store, root.id());

        assert_eq!(&*calls.lock().unwrap(), &["focused", "root"]);
    }

    struct InitProbe {
        count: Arc<AtomicUsize>,
        child: Option<Arc<AtomicUsize>>,
        grandchild: Option<Arc<AtomicUsize>>,
    }

    impl Component<TestAction, ()> for InitProbe {
        fn init(&mut self, cx: &mut Context<'_, Self, TestAction, ()>) {
            self.count.fetch_add(1, Ordering::SeqCst);
            if let Some(count) = self.child.take() {
                cx.insert(InitProbe {
                    count,
                    child: self.grandchild.take(),
                    grandchild: None,
                });
            }
        }

        fn render(&self, _frame: &mut Frame, _area: Rect, _cx: &RenderContext<'_, TestAction, ()>) {
        }
    }

    fn core_for<A: 'static, M: Send + 'static>() -> RuntimeState<A, M> {
        let (sender, _receiver) = mpsc::unbounded_channel();
        RuntimeState::new(sender, Arc::new(crate::task::TokioExecutor))
    }

    #[test]
    fn initialization_drains_descendants_to_a_fixed_point() {
        let mut store = EntityStore::new();
        let root_count = Arc::new(AtomicUsize::new(0));
        let child_count = Arc::new(AtomicUsize::new(0));
        let grandchild_count = Arc::new(AtomicUsize::new(0));
        let root = store.insert(InitProbe {
            count: Arc::clone(&root_count),
            child: Some(Arc::clone(&child_count)),
            grandchild: Some(Arc::clone(&grandchild_count)),
        });
        let mut core = core_for();
        core.pending_inits.push_back(root.id());
        flush_requests(&mut core, &mut store);
        assert_eq!(root_count.load(Ordering::SeqCst), 1);
        assert_eq!(child_count.load(Ordering::SeqCst), 1);
        assert_eq!(grandchild_count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn root_init_child_is_ready_before_first_render() {
        let mut store = EntityStore::new();
        let child_count = Arc::new(AtomicUsize::new(0));
        let root = store.insert(InitProbe {
            count: Arc::new(AtomicUsize::new(0)),
            child: Some(Arc::clone(&child_count)),
            grandchild: None,
        });
        let mut core = core_for();
        core.pending_inits.push_back(root.id());
        flush_requests(&mut core, &mut store);
        assert_eq!(child_count.load(Ordering::SeqCst), 1);
        assert!(core.pending_inserts.is_empty());
        assert!(core.pending_inits.is_empty());
    }

    #[test]
    fn missing_initialization_does_not_poison_entity_id() {
        let mut store = EntityStore::new();
        let mut core = core_for();
        let missing = EntityId::allocate();
        {
            let mut cx = Ctx::new(&mut core, &store, missing);
            store.init_if_needed(missing, &mut cx);
        }
        assert!(!store.is_initialised(missing));
        let count = Arc::new(AtomicUsize::new(0));
        store.insert_slot(
            missing,
            Box::new(InitProbe {
                count: Arc::clone(&count),
                child: None,
                grandchild: None,
            }),
        );
        {
            let mut cx = Ctx::new(&mut core, &store, missing);
            store.init_if_needed(missing, &mut cx);
        }
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    struct OverlayProbe {
        initialized: Arc<AtomicUsize>,
        handle: crate::FocusHandle,
        scope: crate::FocusScope,
        opens_child: bool,
    }

    impl OverlayProbe {
        fn new(initialized: Arc<AtomicUsize>, opens_child: bool) -> Self {
            let mut scope = crate::FocusScope::new();
            let handle = scope.handle();
            Self {
                initialized,
                handle,
                scope,
                opens_child,
            }
        }
    }

    impl Component<TestAction, ()> for OverlayProbe {
        fn init(&mut self, cx: &mut Context<'_, Self, TestAction, ()>) {
            self.initialized.fetch_add(1, Ordering::SeqCst);
            cx.register_scope(self.scope.clone());
            cx.bind_focus(self.handle);
            cx.focus(self.handle);
            if self.opens_child {
                cx.open_overlay(Self::new(Arc::clone(&self.initialized), false));
            }
        }

        fn render(&self, _frame: &mut Frame, _area: Rect, _cx: &RenderContext<'_, TestAction, ()>) {
        }
    }

    #[test]
    fn overlays_initialize_nested_and_are_removed_on_close() {
        let mut store = EntityStore::new();
        let initialized = Arc::new(AtomicUsize::new(0));
        let outer_id = EntityId::allocate();
        let outer = OverlayProbe::new(Arc::clone(&initialized), true);
        let outer_handle = outer.handle;
        let mut core = core_for();
        core.pending_overlays.push_back((
            outer_id,
            Box::new(outer) as Box<dyn crate::entity::ComponentSlot<TestAction, ()>>,
        ));

        flush_requests(&mut core, &mut store);

        assert_eq!(initialized.load(Ordering::SeqCst), 2);
        assert_eq!(core.overlays.overlays().len(), 2);
        let inner_id = core.overlays.top().expect("nested overlay is active");
        assert!(store.contains(outer_id));
        assert!(store.contains(inner_id));
        assert_ne!(core.focus.current(), Some(outer_handle));

        core.pending_closes = 1;
        flush_requests(&mut core, &mut store);
        assert_eq!(core.focus.current(), Some(outer_handle));
        assert!(store.contains(outer_id));
        assert!(!store.contains(inner_id));

        core.pending_closes = 1;
        flush_requests(&mut core, &mut store);
        assert!(core.overlays.overlays().is_empty());
        assert!(!store.contains(outer_id));
        assert!(!store.contains(inner_id));
        assert!(core.focus_map.is_empty());
        assert!(core.focus.current().is_none());
    }

    #[test]
    fn closing_without_an_overlay_does_not_restore_focus() {
        let mut core = core_for::<TestAction, ()>();
        let handle = crate::FocusHandle::new();
        core.focus.focus(handle);
        core.pending_closes = 1;
        let mut store = EntityStore::new();
        flush_requests(&mut core, &mut store);
        assert_eq!(core.focus.current(), Some(handle));
    }

    #[test]
    fn removed_target_drops_late_task_delivery() {
        let mut store = EntityStore::new();
        let log = Arc::new(Mutex::new(Vec::new()));
        let target = store.insert(MessageProbe {
            log: Arc::clone(&log),
        });
        let mut core = core_for();
        assert!(store.remove_entity(target.id()));
        deliver_task(
            TaskDelivery {
                target: target.id(),
                result: Ok(TestMessage::First),
            },
            &mut core,
            &store,
        )
        .unwrap();
        assert!(log.lock().unwrap().is_empty());
    }
}
