//! Entity store with typed handles.
//!
//! Components may own children directly, but a runtime-owned store is useful
//! for ownership and async work: entities live in the runtime, components
//! hold cheap typed handles, and task results are routed back by entity id.
//! A message delivered to a removed entity is dropped safely — the runtime
//! never keeps a destroyed component alive through a pending result.

use std::any::Any;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use ratatui::Frame;
use ratatui::layout::Rect;

use crate::component::{ActionStatus, Component, RenderContext};
use crate::context::Ctx;

fn next_counter() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Stable identity of a stored entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EntityId(u64);

impl EntityId {
    pub(crate) fn allocate() -> Self {
        Self(next_counter())
    }

    #[cfg(test)]
    pub(crate) fn from_u64(value: u64) -> Self {
        Self(value)
    }
}

impl fmt::Display for EntityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "entity-{}", self.0)
    }
}

/// Typed handle to component state owned by the runtime's entity store.
///
/// Cheap to copy and compare. The handle does not keep the entity alive;
/// operations on a removed entity report it as missing.
#[derive(Debug)]
pub struct Entity<T> {
    id: EntityId,
    _marker: std::marker::PhantomData<fn() -> T>,
}

impl<T> Clone for Entity<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Entity<T> {}

impl<T> PartialEq for Entity<T> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl<T> Eq for Entity<T> {}

impl<T> std::hash::Hash for Entity<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

impl<T> Entity<T> {
    pub(crate) fn from_id(id: EntityId) -> Self {
        Self {
            id,
            _marker: std::marker::PhantomData,
        }
    }

    /// The entity's stable id.
    pub fn id(&self) -> EntityId {
        self.id
    }
}

/// Type-erased callback surface stored per entity.
///
/// Implemented automatically for every `Component<A, M>`; the runtime calls
/// it through the entity store without knowing concrete component types.
pub(crate) trait ComponentSlot<A, M>: 'static {
    fn init(&mut self, cx: &mut Ctx<'_, A, M>);
    fn handle_action(&mut self, action: &A, cx: &mut Ctx<'_, A, M>) -> ActionStatus;
    fn handle_message(&mut self, message: M, cx: &mut Ctx<'_, A, M>);
    fn render(&self, frame: &mut Frame, area: Rect, cx: &RenderContext<'_, A, M>);
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

impl<S, A, M> ComponentSlot<A, M> for S
where
    S: Component<A, M>,
    A: 'static,
    M: 'static,
{
    fn init(&mut self, cx: &mut Ctx<'_, A, M>) {
        let mut typed = cx.typed::<S>();
        S::init(self, &mut typed)
    }
    fn handle_action(&mut self, action: &A, cx: &mut Ctx<'_, A, M>) -> ActionStatus {
        let mut typed = cx.typed::<S>();
        S::handle_action(self, action, &mut typed)
    }

    fn handle_message(&mut self, message: M, cx: &mut Ctx<'_, A, M>) {
        let mut typed = cx.typed::<S>();
        S::handle_message(self, message, &mut typed)
    }

    fn render(&self, frame: &mut Frame, area: Rect, cx: &RenderContext<'_, A, M>) {
        S::render(self, frame, area, cx)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// The inner value of one entity slot: a boxed, type-erased component.
type SlotValue<A, M> = Option<Box<dyn ComponentSlot<A, M>>>;

/// A single entity slot: the boxed component behind its own lock.
type Slot<A, M> = Mutex<SlotValue<A, M>>;

/// Runtime-owned entity storage.
///
/// Each slot holds one boxed component behind its own lock, so cross-entity
/// access from a context never blocks unrelated entities. Dispatch locks one
/// slot at a time; re-entrant access to the same entity from within its own
/// callback deadlocks and is rejected by design (the component already has
/// `&mut self`).
pub(crate) struct EntityStore<A, M> {
    slots: HashMap<EntityId, Slot<A, M>>,
    /// Entities whose `init` has already run. Interior-mutable so init can
    /// run through the shared-lock action dispatch path (like
    /// [`Self::dispatch_action`]).
    initialised: RefCell<HashSet<EntityId>>,
}

impl<A, M> Default for EntityStore<A, M> {
    fn default() -> Self {
        Self {
            slots: HashMap::new(),
            initialised: RefCell::new(HashSet::new()),
        }
    }
}

impl<A: 'static, M: 'static> EntityStore<A, M> {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Move a component into the store, returning a typed handle.
    pub(crate) fn insert<T: Component<A, M>>(&mut self, state: T) -> Entity<T> {
        let id = EntityId::allocate();
        self.slots
            .insert(id, Mutex::new(Some(Box::new(state) as _)));
        Entity::from_id(id)
    }

    /// Insert a slot reserved by [`EntityId::allocate`] (queued overlay open).
    pub(crate) fn insert_slot(&mut self, id: EntityId, slot: Box<dyn ComponentSlot<A, M>>) {
        self.slots.insert(id, Mutex::new(Some(slot)));
    }

    /// Run `init` on an entity exactly once, tracked by `initialised`.
    ///
    /// Queued inserts are flushed before this runs, so entities the
    /// component inserts during init are visible to it.
    pub(crate) fn init_if_needed(&self, id: EntityId, cx: &mut Ctx<'_, A, M>) {
        if self.initialised.borrow().contains(&id) {
            return;
        }
        let Some(mut guard) = self.lock(id) else {
            return;
        };
        let Some(component) = guard.as_mut() else {
            return;
        };
        self.initialised.borrow_mut().insert(id);
        component.init(cx);
    }

    #[cfg(test)]
    pub(crate) fn is_initialised(&self, id: EntityId) -> bool {
        self.initialised.borrow().contains(&id)
    }

    /// Remove an entity, returning whether it existed.
    ///
    /// Handles are non-owning, so all future operations on this id become
    /// safe no-ops.
    pub(crate) fn remove_entity(&mut self, id: EntityId) -> bool {
        let removed = self.slots.remove(&id).is_some();
        if removed {
            self.initialised.borrow_mut().remove(&id);
        }
        removed
    }

    #[cfg(test)]
    pub(crate) fn remove(&mut self, id: EntityId) -> bool {
        self.remove_entity(id)
    }

    /// Whether the entity still exists.
    #[cfg(test)]
    pub(crate) fn contains(&self, id: EntityId) -> bool {
        self.slots.contains_key(&id)
    }

    pub(crate) fn is_active_entity(&self, id: EntityId) -> bool {
        self.slots.contains_key(&id)
    }

    pub(crate) fn lock(&self, id: EntityId) -> Option<MutexGuard<'_, SlotValue<A, M>>> {
        self.slots.get(&id).and_then(|slot| slot.lock().ok())
    }

    /// Dispatch an action to the entity, returning its action status.
    pub(crate) fn dispatch_action(
        &self,
        id: EntityId,
        action: &A,
        cx: &mut Ctx<'_, A, M>,
    ) -> Option<ActionStatus> {
        let propagation = {
            let mut guard = self.lock(id)?;
            guard.as_mut()?.handle_action(action, cx)
        };
        Some(propagation)
    }

    /// Deliver a deferred message to the entity's message handler.
    pub(crate) fn deliver_message(&self, id: EntityId, message: M, cx: &mut Ctx<'_, A, M>) {
        if let Some(mut guard) = self.lock(id)
            && let Some(component) = guard.as_mut()
        {
            component.handle_message(message, cx);
        }
    }

    /// Render the entity into `area`.
    pub(crate) fn render_entity(
        &self,
        id: EntityId,
        frame: &mut Frame,
        area: Rect,
        cx: &RenderContext<'_, A, M>,
    ) {
        if let Some(guard) = self.lock(id)
            && let Some(component) = guard.as_ref()
        {
            component.render(frame, area, cx);
        }
    }

    /// Run `f` with exclusive access to the entity's state, typed as `E`.
    pub(crate) fn typed_update<E: 'static, R>(
        &self,
        id: EntityId,
        f: impl FnOnce(&mut E) -> R,
    ) -> Option<R> {
        let mut guard = self.lock(id)?;
        let state = guard.as_mut()?.as_any_mut().downcast_mut::<E>()?;
        Some(f(state))
    }

    /// Run `f` with read access to the entity's state, typed as `E`.
    pub(crate) fn typed_read<E: 'static, R>(
        &self,
        id: EntityId,
        f: impl FnOnce(&E) -> R,
    ) -> Option<R> {
        let guard = self.lock(id)?;
        let state = guard.as_ref()?.as_any().downcast_ref::<E>()?;
        Some(f(state))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{Ctx, RuntimeState};
    use crate::task::TokioExecutor;
    use std::sync::Arc;

    struct Counter {
        value: i32,
    }

    impl Component<(), ()> for Counter {
        fn render(&self, _frame: &mut Frame, _area: Rect, _cx: &RenderContext<'_, (), ()>) {}
    }

    struct Echo;

    impl Component<(), String> for Echo {
        fn handle_action(
            &mut self,
            _action: &(),
            _cx: &mut Context<'_, Self, (), String>,
        ) -> ActionStatus {
            ActionStatus::Handled
        }

        fn handle_message(&mut self, message: String, cx: &mut Context<'_, Self, (), String>) {
            cx.emit(format!("echo:{message}"));
        }

        fn render(&self, _frame: &mut Frame, _area: Rect, _cx: &RenderContext<'_, (), String>) {}
    }

    fn ctx<'a, A: 'static, M: 'static>(
        core: &'a mut RuntimeState<A, M>,
        store: &'a EntityStore<A, M>,
    ) -> Ctx<'a, A, M> {
        Ctx::new(core, store, EntityId::allocate())
    }

    use crate::context::Context;

    struct SelfAccess {
        read_was_none: bool,
        update_was_none: bool,
    }

    impl Component<(), ()> for SelfAccess {
        fn handle_action(
            &mut self,
            _action: &(),
            cx: &mut Context<'_, Self, (), ()>,
        ) -> ActionStatus {
            let entity = cx.entity();
            self.read_was_none = cx.read(entity, |_| ()).is_none();
            self.update_was_none = cx.update(entity, |_| ()).is_none();
            ActionStatus::Handled
        }

        fn render(&self, _frame: &mut Frame, _area: Rect, _cx: &RenderContext<'_, (), ()>) {}
    }

    #[test]
    fn context_self_access_is_rejected_without_deadlocking() {
        let mut store = EntityStore::new();
        let entity = store.insert(SelfAccess {
            read_was_none: false,
            update_was_none: false,
        });
        let mut core = core_for();
        let mut cx = ctx(&mut core, &store);
        assert_eq!(
            store.dispatch_action(entity.id(), &(), &mut cx),
            Some(ActionStatus::Handled)
        );
        let state = store
            .typed_read(entity.id(), |state: &SelfAccess| {
                (state.read_was_none, state.update_was_none)
            })
            .unwrap();
        assert_eq!(state, (true, true));
    }

    fn core_for<A: 'static, M: Send + 'static>() -> RuntimeState<A, M> {
        let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
        RuntimeState::new(sender, Arc::new(TokioExecutor))
    }

    #[test]
    fn insert_update_read_remove() {
        let mut store = EntityStore::new();
        let entity = store.insert(Counter { value: 1 });
        store
            .typed_update(entity.id(), |c: &mut Counter| c.value += 1)
            .unwrap();
        assert_eq!(
            store
                .typed_read(entity.id(), |c: &Counter| c.value)
                .unwrap(),
            2
        );
        assert!(store.contains(entity.id()));
        assert!(store.remove(entity.id()));
        assert!(!store.remove(entity.id()));
        assert!(
            store
                .typed_update(entity.id(), |c: &mut Counter| c.value)
                .is_none()
        );
    }

    #[test]
    fn typed_access_rejects_wrong_type() {
        let mut store = EntityStore::new();
        let entity = store.insert(Counter { value: 0 });
        assert!(
            store
                .typed_update::<String, _>(entity.id(), |_s: &mut String| ())
                .is_none()
        );
        assert!(
            store
                .typed_read::<String, _>(entity.id(), |_s: &String| ())
                .is_none()
        );
    }

    #[test]
    fn message_dispatch_emits_through_context() {
        let mut store = EntityStore::new();
        let echo = store.insert(Echo);
        let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut core = RuntimeState::new(sender, Arc::new(TokioExecutor));
        let mut cx = ctx(&mut core, &store);
        store.deliver_message(echo.id(), "hi".to_owned(), &mut cx);
        let queued = core.take_messages();
        assert_eq!(queued.len(), 1);
        assert!(matches!(
            queued.front(),
            Some(crate::context::QueuedMessage {
                target: crate::context::MessageTarget::Root,
                message
            }) if message == "echo:hi"
        ));
    }

    #[test]
    fn action_dispatch_returns_propagation() {
        let mut store = EntityStore::new();
        let echo = store.insert(Echo);
        let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut core = RuntimeState::new(sender, Arc::new(TokioExecutor));
        let mut cx = ctx(&mut core, &store);
        assert_eq!(
            store.dispatch_action(echo.id(), &(), &mut cx),
            Some(ActionStatus::Handled)
        );
    }

    #[test]
    fn missing_entity_is_a_safe_no_op() {
        let store = EntityStore::new();
        let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut core = RuntimeState::new(sender, Arc::new(TokioExecutor));
        let missing = EntityId::allocate();
        {
            let mut cx = ctx(&mut core, &store);
            store.deliver_message(missing, "gone".to_owned(), &mut cx);
            assert_eq!(store.dispatch_action(missing, &(), &mut cx), None);
        }
        assert!(core.take_messages().is_empty());
    }
}
