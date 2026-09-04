//! Capabilities available during component callbacks.
//!
//! `dispatch` and `update` are direct, synchronous operations on a known
//! entity. `notify` invalidates the current entity and `observe` reacts to
//! that invalidation later. `emit`/`subscribe` are deferred typed entity
//! events; `subscribe_stream` consumes an external stream; `spawn` delivers a
//! one-shot typed result. Deferred callbacks are never re-entrant.

use std::any::{Any, TypeId};
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use tokio::sync::mpsc::UnboundedSender;

use crate::component::{ActionStatus, Component};
use crate::entity::{ComponentSlot, Entity, EntityId, EntityStore};
use crate::focus::FocusManager;
use crate::overlay::OverlayStack;
use crate::subscription::{
    self, EventDelivery, RuntimeDelivery, Subscription, SubscriptionId, SubscriptionRecord,
};
use crate::task::{TaskDelivery, TaskError, TaskExecutor, TaskHandle, TaskId};

/// Runtime state shared by the event loop and component contexts.
pub(crate) struct RuntimeState<A> {
    pub(crate) sender: UnboundedSender<RuntimeDelivery>,
    pub(crate) executor: Arc<dyn TaskExecutor>,
    pub(crate) subscriptions: HashMap<SubscriptionId, SubscriptionRecord<A>>,
    pub(crate) task_handlers: HashMap<TaskId, Box<dyn TaskHandler<A>>>,
    pub(crate) deliveries: VecDeque<RuntimeDelivery>,
    pub(crate) invalidated: HashSet<EntityId>,
    pub(crate) focus: FocusManager,
    pub(crate) parent_map: HashMap<EntityId, EntityId>,
    pub(crate) overlays: OverlayStack,
    pub(crate) pending_overlays: VecDeque<(EntityId, Box<dyn ComponentSlot<A>>)>,
    pub(crate) pending_inserts: VecDeque<(EntityId, Box<dyn ComponentSlot<A>>)>,
    pub(crate) pending_inits: VecDeque<EntityId>,
    pub(crate) pending_closes: usize,
    pub(crate) dirty: bool,
    pub(crate) quit: bool,
}

impl<A: 'static> RuntimeState<A> {
    pub(crate) fn new(
        sender: UnboundedSender<RuntimeDelivery>,
        executor: Arc<dyn TaskExecutor>,
    ) -> Self {
        Self {
            sender,
            executor,
            subscriptions: HashMap::new(),
            task_handlers: HashMap::new(),
            deliveries: VecDeque::new(),
            invalidated: HashSet::new(),
            focus: FocusManager::default(),
            parent_map: HashMap::new(),
            overlays: OverlayStack::new(),
            pending_overlays: VecDeque::new(),
            pending_inserts: VecDeque::new(),
            pending_inits: VecDeque::new(),
            pending_closes: 0,
            dirty: true,
            quit: false,
        }
    }

    pub(crate) fn cleanup_subscriptions(&mut self, store: &EntityStore<A>) {
        let stale: Vec<_> = self
            .subscriptions
            .iter()
            .filter_map(|(id, record)| {
                let (active, source, target) = match record {
                    SubscriptionRecord::Stream(s) => (&s.active, None, s.target),
                    SubscriptionRecord::Event(s) => (&s.active, Some(s.source), s.target),
                    SubscriptionRecord::Observation(s) => (&s.active, Some(s.source), s.target),
                };
                (!active.load(std::sync::atomic::Ordering::Acquire)
                    || !store.is_active_entity(target)
                    || source.is_some_and(|id| !store.is_active_entity(id)))
                .then_some(*id)
            })
            .collect();
        for id in stale {
            self.remove_subscription(id);
        }
        self.task_handlers
            .retain(|_, handler| handler.is_active() && store.is_active_entity(handler.target()));
        self.invalidated.retain(|id| store.is_active_entity(*id));
    }

    pub(crate) fn remove_subscription(&mut self, id: SubscriptionId) {
        if let Some(record) = self.subscriptions.remove(&id) {
            let active = match &record {
                SubscriptionRecord::Stream(s) => &s.active,
                SubscriptionRecord::Event(s) => &s.active,
                SubscriptionRecord::Observation(s) => &s.active,
            };
            active.store(false, std::sync::atomic::Ordering::Release);
            let cancellation = match record {
                SubscriptionRecord::Stream(s) => s.cancellation,
                SubscriptionRecord::Event(s) => s.cancellation,
                SubscriptionRecord::Observation(s) => s.cancellation,
            };
            let _ = cancellation.send(true);
        }
    }

    pub(crate) fn take_invalidated(&mut self) -> Vec<EntityId> {
        self.invalidated.drain().collect()
    }

    pub(crate) fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    pub(crate) fn should_quit(&self) -> bool {
        self.quit
    }

    pub(crate) fn input_target(&self) -> Option<EntityId> {
        self.overlays.top().or_else(|| self.focus.current())
    }
}

pub(crate) trait TaskHandler<A>: 'static {
    fn target(&self) -> EntityId;

    fn is_active(&self) -> bool;

    fn invoke(
        self: Box<Self>,
        result: Box<dyn Any + Send>,
        state: &mut RuntimeState<A>,
        store: &EntityStore<A>,
    );
}

struct TypedTaskHandler<T, R, H> {
    target: EntityId,
    handler: H,
    active: Arc<AtomicBool>,
    marker: PhantomData<fn(T, R)>,
}

impl<T, R, H, A> TaskHandler<A> for TypedTaskHandler<T, R, H>
where
    T: Component<A>,
    R: Send + 'static,
    A: 'static,
    H: for<'a> FnOnce(Result<R, TaskError>, &'a mut T, &'a mut Context<'a, T, A>) + Send + 'static,
{
    fn target(&self) -> EntityId {
        self.target
    }

    fn is_active(&self) -> bool {
        self.active.load(std::sync::atomic::Ordering::Acquire)
    }

    fn invoke(
        self: Box<Self>,
        result: Box<dyn Any + Send>,
        state: &mut RuntimeState<A>,
        store: &EntityStore<A>,
    ) {
        let Ok(result) = result.downcast::<Result<R, TaskError>>() else {
            return;
        };
        let Some(mut slot) = store.lock(self.target) else {
            return;
        };
        let Some(component) = slot
            .as_mut()
            .and_then(|v| v.as_any_mut().downcast_mut::<T>())
        else {
            return;
        };
        let mut cx = Context::new(state, store, self.target);
        (self.handler)(*result, component, &mut cx);
    }
}

/// Type-specific callback context for one component.
pub struct Context<'a, T, A> {
    pub(crate) runtime_state: &'a mut RuntimeState<A>,
    pub(crate) store: &'a EntityStore<A>,
    entity: Entity<T>,
    _marker: PhantomData<fn() -> T>,
}

impl<'a, T: Component<A>, A: 'static> Context<'a, T, A> {
    pub(crate) fn new(
        state: &'a mut RuntimeState<A>,
        store: &'a EntityStore<A>,
        entity: EntityId,
    ) -> Self {
        Self {
            runtime_state: state,
            store,
            entity: Entity::from_id(entity),
            _marker: PhantomData,
        }
    }

    pub fn entity(&self) -> Entity<T> {
        self.entity
    }

    /// Queue a typed event from the current entity for later delivery.
    pub fn emit<Ev: Send + 'static>(&mut self, event: Ev) {
        self.runtime_state
            .deliveries
            .push_back(RuntimeDelivery::Event(EventDelivery {
                source: self.entity.id(),
                event_type: TypeId::of::<Ev>(),
                event: Box::new(event),
            }));
    }

    /// Observe state invalidation from a specific source. The callback reads
    /// the source with `read`; it receives no source lock or payload.
    pub fn observe<Source, F>(&mut self, source: Entity<Source>, callback: F) -> Subscription
    where
        Source: Component<A>,
        F: for<'b> FnMut(&'b mut T, Entity<Source>, &'b mut Context<'b, T, A>) + 'static,
    {
        let (active, cancellation, _) = subscription::cancellation();
        let id = SubscriptionId::allocate();
        self.runtime_state.subscriptions.insert(
            id,
            SubscriptionRecord::Observation(subscription::ObservationSubscription {
                source: source.id(),
                target: self.entity.id(),
                active: active.clone(),
                cancellation: cancellation.clone(),
                handler: subscription::observation_handler(callback),
            }),
        );
        Subscription::new(active, cancellation)
    }

    /// Subscribe to a typed event emitted by one specific source entity.
    pub fn subscribe<Ev, Source, F>(&mut self, source: Entity<Source>, callback: F) -> Subscription
    where
        Ev: Send + 'static,
        Source: Component<A>,
        F: for<'b> FnMut(&Ev, &'b mut T, Entity<Source>, &'b mut Context<'b, T, A>) + 'static,
    {
        let (active, cancellation, _) = subscription::cancellation();
        let id = SubscriptionId::allocate();
        self.runtime_state.subscriptions.insert(
            id,
            SubscriptionRecord::Event(subscription::EventSubscription {
                source: source.id(),
                target: self.entity.id(),
                active: active.clone(),
                cancellation: cancellation.clone(),
                handler: subscription::event_handler(callback),
            }),
        );
        Subscription::new(active, cancellation)
    }

    /// Subscribe to an external asynchronous stream.
    pub fn subscribe_stream<S, Item, F>(&mut self, stream: S, callback: F) -> Subscription
    where
        T: Component<A>,
        S: futures_util::Stream<Item = Item> + Send + 'static,
        Item: Send + 'static,
        F: for<'b> FnMut(
                subscription::SubscriptionEvent<Item>,
                &'b mut T,
                &'b mut Context<'b, T, A>,
            ) + 'static,
    {
        let id = SubscriptionId::allocate();
        let (active, cancellation, receiver) = subscription::cancellation();
        self.runtime_state.subscriptions.insert(
            id,
            SubscriptionRecord::Stream(subscription::StreamSubscription {
                target: self.entity.id(),
                active: active.clone(),
                cancellation: cancellation.clone(),
                handler: subscription::stream_handler(callback),
            }),
        );
        let worker = subscription::worker(
            stream,
            id,
            active.clone(),
            receiver,
            self.runtime_state.sender.clone(),
        );
        self.runtime_state
            .executor
            .spawn_subscription(Box::pin(worker));
        Subscription::new(active, cancellation)
    }

    /// Start one-shot work; its typed result is delivered to this entity.
    pub fn spawn<F, R, H>(&mut self, future: F, handler: H) -> TaskHandle
    where
        F: Future<Output = Result<R, TaskError>> + Send + 'static,
        R: Send + 'static,
        H: for<'b> FnOnce(Result<R, TaskError>, &'b mut T, &'b mut Context<'b, T, A>)
            + Send
            + 'static,
    {
        let id = TaskId::allocate();
        let target = self.entity.id();
        let cancellation = Arc::new(std::sync::atomic::AtomicBool::new(true));
        self.runtime_state.task_handlers.insert(
            id,
            Box::new(TypedTaskHandler {
                target,
                handler,
                active: Arc::clone(&cancellation),
                marker: PhantomData,
            }),
        );
        let delivery = async move {
            TaskDelivery {
                id,
                target,
                result: Box::new(future.await),
            }
        };
        let handle = self
            .runtime_state
            .executor
            .spawn(Box::pin(delivery), self.runtime_state.sender.clone());
        handle.with_cancel_cleanup(move || {
            cancellation.store(false, std::sync::atomic::Ordering::Release);
        })
    }

    /// Invalidate this entity; observers run in a later deferred phase.
    pub fn notify(&mut self) {
        self.runtime_state.invalidated.insert(self.entity.id());
        self.runtime_state.dirty = true;
    }

    pub fn quit(&mut self) {
        self.runtime_state.quit = true;
    }

    pub fn focus_entity<E>(&mut self, target: Entity<E>) {
        self.runtime_state.focus.focus(target.id());
        self.runtime_state.dirty = true;
    }

    pub fn focus_order<I: IntoIterator<Item = EntityId>>(&mut self, order: I) {
        self.runtime_state
            .focus
            .register_order(self.entity.id(), order.into_iter().collect());
    }

    pub fn is_focused(&self) -> bool {
        self.runtime_state.focus.current() == Some(self.entity.id())
    }

    pub fn focus_next(&mut self) {
        self.runtime_state.focus.focus_next();
        self.runtime_state.dirty = true;
    }

    pub fn focus_prev(&mut self) {
        self.runtime_state.focus.focus_prev();
        self.runtime_state.dirty = true;
    }

    pub fn open_overlay<O: Component<A>>(&mut self, overlay: O) -> Entity<O> {
        let id = EntityId::allocate();
        self.runtime_state
            .pending_overlays
            .push_back((id, Box::new(overlay)));
        self.runtime_state.parent_map.insert(id, self.entity.id());
        self.runtime_state.dirty = true;
        Entity::from_id(id)
    }

    pub fn close_overlay(&mut self) {
        self.runtime_state.pending_closes += 1;
        self.runtime_state.dirty = true;
    }

    /// Mutate another entity synchronously. A successful update invalidates
    /// the target and marks it dirty; missing or wrong-typed entities are no-ops.
    pub fn update<E: 'static, R>(
        &mut self,
        target: Entity<E>,
        f: impl FnOnce(&mut E) -> R,
    ) -> Option<R> {
        if target.id() == self.entity.id() {
            return None;
        }
        let result = self.store.typed_update(target.id(), f);
        if result.is_some() {
            self.runtime_state.invalidated.insert(target.id());
            self.runtime_state.dirty = true;
        }
        result
    }

    pub fn read<E: 'static, R>(&self, target: Entity<E>, f: impl FnOnce(&E) -> R) -> Option<R> {
        if target.id() == self.entity.id() {
            None
        } else {
            self.store.typed_read(target.id(), f)
        }
    }

    pub fn insert<E: Component<A>>(&mut self, state: E) -> Entity<E> {
        let id = EntityId::allocate();
        self.runtime_state
            .pending_inserts
            .push_back((id, Box::new(state)));
        self.runtime_state.parent_map.insert(id, self.entity.id());
        self.runtime_state.pending_inits.push_back(id);
        Entity::from_id(id)
    }
    /// Dispatch a synchronous action to a known entity. Self-dispatch is a
    /// safe `Continue` no-op because the current slot is already locked.
    pub fn dispatch<E: Component<A>>(&mut self, target: Entity<E>, action: &A) -> ActionStatus {
        if target.id() == self.entity.id() {
            return ActionStatus::Continue;
        }
        let mut cx = Ctx::new(self.runtime_state, self.store, target.id());
        self.store
            .dispatch_action(target.id(), action, &mut cx)
            .unwrap_or(ActionStatus::Continue)
    }
}

pub(crate) struct Ctx<'a, A> {
    runtime_state: &'a mut RuntimeState<A>,
    store: &'a EntityStore<A>,
    entity: EntityId,
}

impl<'a, A: 'static> Ctx<'a, A> {
    pub(crate) fn new(
        state: &'a mut RuntimeState<A>,
        store: &'a EntityStore<A>,
        entity: EntityId,
    ) -> Self {
        Self {
            runtime_state: state,
            store,
            entity,
        }
    }

    pub(crate) fn typed<T: Component<A>>(&mut self) -> Context<'_, T, A> {
        Context::new(self.runtime_state, self.store, self.entity)
    }
}
