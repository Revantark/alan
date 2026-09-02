//! Context capabilities available during component callbacks.
//!
//! Mutating operations (overlay open/close and message delivery) are deferred
//! and applied after the current callback completes. `emit` targets the root;
//! `send` targets one entity. Entity reads and writes apply immediately and
//! reject self-access, avoiding re-entry into the currently locked entity.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::marker::PhantomData;
use std::sync::Arc;

use tokio::sync::mpsc::UnboundedSender;

use crate::component::{ActionStatus, Component};
use crate::entity::{ComponentSlot, Entity, EntityId, EntityStore};
use crate::focus::{FocusHandle, FocusId, FocusManager, FocusScope};
use crate::overlay::OverlayStack;
use crate::subscription::{
    RuntimeDelivery, Subscription, SubscriptionEvent, SubscriptionId, SubscriptionRecord, handler,
};
use crate::task::{TaskDelivery, TaskError, TaskExecutor};

/// Destination for a deferred component message.
#[derive(Debug)]
pub(crate) enum MessageTarget {
    Root,
    Entity(EntityId),
}

/// A message and the entity that should receive it.
#[derive(Debug)]
pub(crate) struct QueuedMessage<M> {
    pub(crate) target: MessageTarget,
    pub(crate) message: M,
}

/// Framework state shared between the event loop and component contexts.
pub(crate) struct RuntimeState<A, M> {
    pub(crate) sender: UnboundedSender<RuntimeDelivery<M>>,
    pub(crate) executor: Arc<dyn TaskExecutor<M>>,
    pub(crate) subscriptions: HashMap<SubscriptionId, SubscriptionRecord<A, M>>,
    pub(crate) deliveries: VecDeque<RuntimeDelivery<M>>,
    pub(crate) focus: FocusManager,
    pub(crate) focus_map: HashMap<FocusId, EntityId>,
    pub(crate) parent_map: HashMap<EntityId, EntityId>,
    pub(crate) overlays: OverlayStack,
    pub(crate) pending_overlays: VecDeque<(EntityId, Box<dyn ComponentSlot<A, M>>)>,
    pub(crate) pending_inserts: VecDeque<(EntityId, Box<dyn ComponentSlot<A, M>>)>,
    /// Entities awaiting their one-time `init` call.
    pub(crate) pending_inits: VecDeque<EntityId>,
    pub(crate) pending_closes: usize,
    /// One FIFO queue preserves ordering between root and targeted messages.
    pub(crate) messages: VecDeque<QueuedMessage<M>>,
    pub(crate) dirty: bool,
    pub(crate) quit: bool,
}

impl<A: 'static, M: 'static> RuntimeState<A, M> {
    pub(crate) fn new(
        sender: UnboundedSender<RuntimeDelivery<M>>,
        executor: Arc<dyn TaskExecutor<M>>,
    ) -> Self {
        Self {
            sender,
            executor,
            subscriptions: HashMap::new(),
            deliveries: VecDeque::new(),
            focus: FocusManager::default(),
            focus_map: HashMap::new(),
            parent_map: HashMap::new(),
            overlays: OverlayStack::new(),
            pending_overlays: VecDeque::new(),
            pending_inserts: VecDeque::new(),
            pending_inits: VecDeque::new(),
            pending_closes: 0,
            messages: VecDeque::new(),
            dirty: true,
            quit: false,
        }
    }

    /// Drain one batch of queued component messages.
    pub(crate) fn take_messages(&mut self) -> VecDeque<QueuedMessage<M>> {
        std::mem::take(&mut self.messages)
    }

    pub(crate) fn take_delivery(&mut self) -> Option<RuntimeDelivery<M>> {
        self.deliveries.pop_front()
    }

    pub(crate) fn cleanup_subscriptions(&mut self, store: &EntityStore<A, M>) {
        let stale: Vec<_> = self
            .subscriptions
            .iter()
            .filter_map(|(id, record)| {
                (!record.active.load(std::sync::atomic::Ordering::Acquire)
                    || !store.is_active_entity(record.target))
                .then_some(*id)
            })
            .collect();
        for id in stale {
            if let Some(record) = self.subscriptions.remove(&id) {
                record
                    .active
                    .store(false, std::sync::atomic::Ordering::Release);
                let _ = record.cancellation.send(true);
            }
        }
    }

    /// Take and reset the dirty flag.
    pub(crate) fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// Mark the UI dirty, scheduling a redraw.
    pub(crate) fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Whether shutdown was requested.
    pub(crate) fn should_quit(&self) -> bool {
        self.quit
    }

    /// The entity currently targeted for input, if any: the topmost overlay,
    /// otherwise the focused entity.
    pub(crate) fn input_target(&self) -> Option<EntityId> {
        if let Some(top) = self.overlays.top() {
            return Some(top);
        }
        self.focus
            .current()
            .and_then(|handle| self.focus_map.get(&handle.id()).copied())
    }
}

/// Type-erased context handed to the entity store for dispatch.
///
/// The store converts it to a typed [`Context`] for the concrete component.
pub(crate) struct Ctx<'a, A, M> {
    pub(crate) runtime_state: &'a mut RuntimeState<A, M>,
    pub(crate) store: &'a EntityStore<A, M>,
    pub(crate) entity: EntityId,
}

impl<'a, A, M> Ctx<'a, A, M> {
    pub(crate) fn new(
        runtime_state: &'a mut RuntimeState<A, M>,
        store: &'a EntityStore<A, M>,
        entity: EntityId,
    ) -> Self {
        Self {
            runtime_state,
            store,
            entity,
        }
    }

    pub(crate) fn typed<T>(&mut self) -> Context<'_, T, A, M> {
        Context {
            runtime_state: self.runtime_state,
            store: self.store,
            entity: Entity::from_id(self.entity),
            _marker: PhantomData,
        }
    }
}

/// Callback capabilities available during `init`, action handling, and
/// message handling.
///
/// Typed with the component's own type `T`; `A` is the application action
/// type and `M` the application message type.
pub struct Context<'a, T, A, M = ()> {
    runtime_state: &'a mut RuntimeState<A, M>,
    store: &'a EntityStore<A, M>,
    entity: Entity<T>,
    _marker: PhantomData<fn() -> T>,
}

impl<'a, T, A: 'static, M: 'static> Context<'a, T, A, M> {
    pub(crate) fn new(
        runtime_state: &'a mut RuntimeState<A, M>,
        store: &'a EntityStore<A, M>,
        entity: EntityId,
    ) -> Self {
        Self {
            runtime_state,
            store,
            entity: Entity::from_id(entity),
            _marker: PhantomData,
        }
    }
    /// Typed handle to the component currently executing a callback.
    ///
    /// Lets a component learn its own identity (e.g. during `init`) so it
    /// can refer to itself in messages.
    pub fn entity(&self) -> Entity<T> {
        self.entity
    }

    /// Queue a message for the root after the current callback returns.
    ///
    /// `emit` reports an event upward. Use [`Context::send`] for a command
    /// addressed to one particular entity. Neither operation delivers
    /// re-entrantly.
    pub fn emit(&mut self, message: M) {
        self.runtime_state.messages.push_back(QueuedMessage {
            target: MessageTarget::Root,
            message,
        });
    }

    /// Queue a message for one entity after the current callback returns.
    ///
    /// Sending to an entity removed before delivery is a safe no-op. This is
    /// separate from [`Context::emit`], which always targets the root.
    pub fn send<E>(&mut self, target: Entity<E>, message: M)
    where
        E: Component<A, M>,
    {
        self.runtime_state.messages.push_back(QueuedMessage {
            target: MessageTarget::Entity(target.id()),
            message,
        });
    }

    /// Mark the UI dirty, scheduling a redraw.
    pub fn notify(&mut self) {
        self.runtime_state.dirty = true;
    }

    /// Request runtime shutdown after the current callback completes.
    pub fn quit(&mut self) {
        self.runtime_state.quit = true;
    }

    /// Set the current focus.
    pub fn focus(&mut self, handle: FocusHandle) {
        self.runtime_state.focus.focus(handle);
        self.runtime_state.dirty = true;
    }

    /// Focus the next handle in the active focus scope.
    pub fn focus_next(&mut self) {
        self.runtime_state.focus.focus_next();
        self.runtime_state.dirty = true;
    }

    /// Focus the previous handle in the active focus scope.
    pub fn focus_prev(&mut self) {
        self.runtime_state.focus.focus_prev();
        self.runtime_state.dirty = true;
    }

    /// Bind a focus handle to this component so routed input reaches it.
    ///
    /// Call once when the component first handles an action; rebinding is
    /// idempotent.
    pub fn bind_focus(&mut self, handle: FocusHandle) {
        self.runtime_state
            .focus_map
            .insert(handle.id(), self.entity.id());
    }

    /// Register a focus scope for next/previous cycling.
    ///
    /// Create all handles up front (e.g. at component construction), then
    /// register the scope once.
    pub fn register_scope(&mut self, scope: FocusScope) {
        self.runtime_state.focus.register(self.entity.id(), scope);
    }

    /// Queue opening an overlay: the component is stored, the current focus
    /// path saved, and input captured by the overlay once the current
    /// callback completes.
    pub fn open_overlay<O>(&mut self, overlay: O) -> Entity<O>
    where
        O: crate::component::Component<A, M>,
        A: 'static,
        M: 'static,
    {
        let id = EntityId::allocate();
        self.runtime_state
            .pending_overlays
            .push_back((id, Box::new(overlay) as Box<dyn ComponentSlot<A, M>>));
        self.runtime_state.parent_map.insert(id, self.entity.id());
        self.runtime_state.dirty = true;
        Entity::from_id(id)
    }

    /// Queue closing the topmost overlay; the previous focus path is
    /// restored when the current callback completes. If no overlay is open,
    /// this is a harmless no-op.
    pub fn close_overlay(&mut self) {
        self.runtime_state.pending_closes += 1;
        self.runtime_state.dirty = true;
    }

    /// Run `f` with exclusive access to another entity's state.
    ///
    /// This is a direct state-access escape hatch, not an action dispatch or
    /// message delivery operation. Returns `None` if the entity was removed,
    /// the type does not match, or `target` is the entity currently executing
    /// a callback.
    pub fn update<E, R>(&mut self, target: Entity<E>, f: impl FnOnce(&mut E) -> R) -> Option<R>
    where
        E: 'static,
    {
        if target.id() == self.entity.id() {
            return None;
        }
        self.store.typed_update(target.id(), f)
    }

    /// Run `f` with read access to another entity's state.
    /// Returns `None` when the entity is missing, has another type, or is the
    /// entity currently executing a callback.
    pub fn read<E, R>(&self, target: Entity<E>, f: impl FnOnce(&E) -> R) -> Option<R>
    where
        E: 'static,
    {
        if target.id() == self.entity.id() {
            return None;
        }
        self.store.typed_read(target.id(), f)
    }

    /// Register a new component with the runtime, returning its handle.
    ///
    /// This is how a parent creates child entities: insert the child state,
    /// keep the handle, and later route actions to it with
    /// [`Context::dispatch`] and render it with
    /// [`RenderContext::render_entity`](crate::RenderContext::render_entity).
    /// The component becomes available (routable and renderable) once the
    /// current callback completes.
    pub fn insert<E>(&mut self, state: E) -> Entity<E>
    where
        E: Component<A, M>,
    {
        let id = EntityId::allocate();
        self.runtime_state
            .pending_inserts
            .push_back((id, Box::new(state) as Box<dyn ComponentSlot<A, M>>));
        self.runtime_state.parent_map.insert(id, self.entity.id());
        self.runtime_state.pending_inits.push_back(id);
        Entity::from_id(id)
    }

    /// Dispatch an action directly to a child entity.
    ///
    /// This is the parent-routing primitive: a parent decides which child is
    /// active and delegates, then handles the action itself if the child
    /// continued. Note the child's queued requests (messages, overlays,
    /// focus) are applied like the parent's, after the child callback
    /// completes.
    pub fn dispatch<E>(&mut self, target: Entity<E>, action: &A) -> ActionStatus
    where
        E: Component<A, M>,
    {
        if target.id() == self.entity.id() {
            // Dispatching to the entity currently executing a callback would
            // deadlock on its slot lock; the component has &mut self already.
            return ActionStatus::Continue;
        }
        let mut cx = Ctx::new(self.runtime_state, self.store, target.id());
        self.store
            .dispatch_action(target.id(), action, &mut cx)
            .unwrap_or(ActionStatus::Continue)
    }

    /// Subscribe to a stream. Items and normal closure are delivered later on
    /// the runtime side, never from the stream worker.
    pub fn subscribe<S, Item, F>(&mut self, stream: S, callback: F) -> Subscription
    where
        T: Component<A, M>,
        S: futures_util::Stream<Item = Item> + Send + 'static,
        Item: Send + 'static,
        M: Send + 'static,
        F: for<'b> FnMut(SubscriptionEvent<Item>, &'b mut T, &'b mut Context<'b, T, A, M>)
            + 'static,
    {
        let id = SubscriptionId::allocate();
        let active = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let (cancellation, receiver) = tokio::sync::watch::channel(false);
        self.runtime_state.subscriptions.insert(
            id,
            SubscriptionRecord {
                target: self.entity.id(),
                active: Arc::clone(&active),
                cancellation: cancellation.clone(),
                handler: handler(callback),
            },
        );
        let worker = crate::subscription::worker(
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

    /// Spawn a background task whose output is delivered as a message to
    /// this entity.
    ///
    /// The task runs on the configured executor and must not block the UI
    /// loop. The entity id is captured now, so a removed component is never
    /// kept alive: delivery to a removed entity is a safe no-op. Errors
    /// surface as `RuntimeError::Task` from the run loop.
    pub fn spawn<F>(&mut self, future: F)
    where
        F: Future<Output = Result<M, TaskError>> + Send + 'static,
    {
        let target = self.entity.id();
        let delivery = async move {
            let result = future.await;
            TaskDelivery { target, result }
        };
        self.runtime_state
            .executor
            .spawn(Box::pin(delivery), self.runtime_state.sender.clone());
    }
}
