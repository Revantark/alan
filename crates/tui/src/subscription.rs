//! Deferred communication infrastructure.
//!
//! Stream subscriptions consume external asynchronous streams. Entity event
//! subscriptions deliver a typed occurrence from one specific source.
//! Observations deliver a source invalidation with no payload. All callbacks
//! run on the runtime side and are deferred until the current callback returns.

use std::any::{Any, TypeId};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use futures_util::StreamExt;
use tokio::sync::{mpsc::UnboundedSender, watch};

use crate::component::Component;
use crate::context::{Context, RuntimeState};
use crate::entity::{Entity, EntityId, EntityStore};
use crate::task::TaskDelivery;

/// A stream item or its normal end.
pub enum SubscriptionEvent<T> {
    Item(T),
    Closed,
}

/// A cancellation handle for a stream, event, or observation subscription.
///
/// Holding the handle keeps the subscription alive: dropping it cancels
/// delivery. One-shot subscriptions created with
/// [`Context::subscribe_once`](crate::context::Context::subscribe_once) are
/// runtime-owned instead and need no handle.
#[must_use = "store the Subscription or it cancels on drop; use subscribe_once for one-shot events"]
#[derive(Debug)]
pub struct Subscription {
    active: Arc<AtomicBool>,
    cancellation: watch::Sender<bool>,
}

impl Subscription {
    pub(crate) fn new(active: Arc<AtomicBool>, cancellation: watch::Sender<bool>) -> Self {
        Self {
            active,
            cancellation,
        }
    }

    pub fn cancel(&self) {
        if self.active.swap(false, Ordering::Release) {
            let _ = self.cancellation.send(true);
        }
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct SubscriptionId(u64);
impl SubscriptionId {
    pub(crate) fn allocate() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// Work arriving at the runtime from a worker or a component callback.
pub enum RuntimeDelivery {
    Task(TaskDelivery),
    Stream(StreamDelivery),
    Event(EventDelivery),
    Observation(EntityId),
}

pub struct StreamDelivery {
    pub(crate) id: SubscriptionId,
    pub(crate) event: StreamDeliveryEvent,
}

pub enum StreamDeliveryEvent {
    Item(Box<dyn Any + Send>),
    Closed,
}

pub struct EventDelivery {
    pub(crate) source: EntityId,
    pub(crate) event_type: TypeId,
    pub(crate) event: Box<dyn Any + Send>,
}

pub(crate) enum SubscriptionRecord<A> {
    Stream(StreamSubscription<A>),
    Event(EventSubscription<A>),
    OneShotEvent(EventSubscription<A>),
    Observation(ObservationSubscription<A>),
}

pub(crate) struct StreamSubscription<A> {
    pub(crate) target: EntityId,
    pub(crate) active: Arc<AtomicBool>,
    pub(crate) cancellation: watch::Sender<bool>,
    pub(crate) handler: Box<dyn StreamHandler<A>>,
}

pub(crate) struct EventSubscription<A> {
    pub(crate) source: EntityId,
    pub(crate) target: EntityId,
    pub(crate) active: Arc<AtomicBool>,
    pub(crate) cancellation: watch::Sender<bool>,
    pub(crate) handler: Box<dyn EventHandler<A>>,
}

pub(crate) struct ObservationSubscription<A> {
    pub(crate) source: EntityId,
    pub(crate) target: EntityId,
    pub(crate) active: Arc<AtomicBool>,
    pub(crate) cancellation: watch::Sender<bool>,
    pub(crate) handler: Box<dyn ObservationHandler<A>>,
}

pub(crate) trait StreamHandler<A>: 'static {
    fn invoke(
        &mut self,
        item: Box<dyn Any + Send>,
        target: EntityId,
        state: &mut RuntimeState<A>,
        store: &EntityStore<A>,
    );

    fn close(&mut self, target: EntityId, state: &mut RuntimeState<A>, store: &EntityStore<A>);
}

pub(crate) trait EventHandler<A>: 'static {
    fn event_type(&self) -> TypeId;

    fn invoke(
        &mut self,
        event: &dyn Any,
        target: EntityId,
        source: EntityId,
        state: &mut RuntimeState<A>,
        store: &EntityStore<A>,
    );
}

pub(crate) trait ObservationHandler<A>: 'static {
    fn invoke(
        &mut self,
        target: EntityId,
        source: EntityId,
        state: &mut RuntimeState<A>,
        store: &EntityStore<A>,
    );
}

struct TypedStreamHandler<T, Item, F> {
    callback: F,
    marker: std::marker::PhantomData<fn(T, Item)>,
}

impl<T, Item, F, A> StreamHandler<A> for TypedStreamHandler<T, Item, F>
where
    T: Component<A>,
    A: 'static,
    Item: 'static,
    F: for<'a> FnMut(SubscriptionEvent<Item>, &'a mut T, &'a mut Context<'a, T, A>) + 'static,
{
    fn invoke(
        &mut self,
        item: Box<dyn Any + Send>,
        target: EntityId,
        state: &mut RuntimeState<A>,
        store: &EntityStore<A>,
    ) {
        let Ok(item) = item.downcast::<Item>() else {
            return;
        };
        let Some(mut slot) = store.lock(target) else {
            return;
        };
        let Some(component) = slot
            .as_mut()
            .and_then(|v| v.as_any_mut().downcast_mut::<T>())
        else {
            return;
        };
        let mut cx = Context::new(state, store, target);
        (self.callback)(SubscriptionEvent::Item(*item), component, &mut cx);
    }

    fn close(&mut self, target: EntityId, state: &mut RuntimeState<A>, store: &EntityStore<A>) {
        let Some(mut slot) = store.lock(target) else {
            return;
        };
        let Some(component) = slot
            .as_mut()
            .and_then(|v| v.as_any_mut().downcast_mut::<T>())
        else {
            return;
        };
        let mut cx = Context::new(state, store, target);
        (self.callback)(SubscriptionEvent::Closed, component, &mut cx);
    }
}

pub(crate) fn stream_handler<T, Item, F, A>(callback: F) -> Box<dyn StreamHandler<A>>
where
    T: Component<A>,
    A: 'static,
    Item: 'static,
    F: for<'a> FnMut(SubscriptionEvent<Item>, &'a mut T, &'a mut Context<'a, T, A>) + 'static,
{
    Box::new(TypedStreamHandler {
        callback,
        marker: std::marker::PhantomData,
    })
}

struct TypedEventHandler<T, Source, Ev, F> {
    callback: F,
    marker: std::marker::PhantomData<fn(T, Source, Ev)>,
}

impl<T, Source, Ev, F, A> EventHandler<A> for TypedEventHandler<T, Source, Ev, F>
where
    T: Component<A>,
    Source: Component<A>,
    Ev: Send + 'static,
    A: 'static,
    F: for<'a> FnMut(&Ev, &'a mut T, Entity<Source>, &'a mut Context<'a, T, A>) + 'static,
{
    fn event_type(&self) -> TypeId {
        TypeId::of::<Ev>()
    }

    fn invoke(
        &mut self,
        event: &dyn Any,
        target: EntityId,
        source: EntityId,
        state: &mut RuntimeState<A>,
        store: &EntityStore<A>,
    ) {
        let Some(event) = event.downcast_ref::<Ev>() else {
            return;
        };
        let Some(mut slot) = store.lock(target) else {
            return;
        };
        let Some(component) = slot
            .as_mut()
            .and_then(|v| v.as_any_mut().downcast_mut::<T>())
        else {
            return;
        };
        let mut cx = Context::new(state, store, target);
        (self.callback)(event, component, Entity::from_id(source), &mut cx);
    }
}

pub(crate) fn event_handler<T, Source, Ev, F, A>(callback: F) -> Box<dyn EventHandler<A>>
where
    T: Component<A>,
    Source: Component<A>,
    Ev: Send + 'static,
    A: 'static,
    F: for<'a> FnMut(&Ev, &'a mut T, Entity<Source>, &'a mut Context<'a, T, A>) + 'static,
{
    Box::new(TypedEventHandler {
        callback,
        marker: std::marker::PhantomData,
    })
}

struct TypedObservationHandler<T, Source, F> {
    callback: F,
    marker: std::marker::PhantomData<fn(T, Source)>,
}

impl<T, Source, F, A> ObservationHandler<A> for TypedObservationHandler<T, Source, F>
where
    T: Component<A>,
    Source: Component<A>,
    A: 'static,
    F: for<'a> FnMut(&'a mut T, Entity<Source>, &'a mut Context<'a, T, A>) + 'static,
{
    fn invoke(
        &mut self,
        target: EntityId,
        source: EntityId,
        state: &mut RuntimeState<A>,
        store: &EntityStore<A>,
    ) {
        let Some(mut slot) = store.lock(target) else {
            return;
        };
        let Some(component) = slot
            .as_mut()
            .and_then(|v| v.as_any_mut().downcast_mut::<T>())
        else {
            return;
        };
        let mut cx = Context::new(state, store, target);
        (self.callback)(component, Entity::from_id(source), &mut cx);
    }
}

pub(crate) fn observation_handler<T, Source, F, A>(callback: F) -> Box<dyn ObservationHandler<A>>
where
    T: Component<A>,
    Source: Component<A>,
    A: 'static,
    F: for<'a> FnMut(&'a mut T, Entity<Source>, &'a mut Context<'a, T, A>) + 'static,
{
    Box::new(TypedObservationHandler {
        callback,
        marker: std::marker::PhantomData,
    })
}

pub(crate) async fn worker<S, Item>(
    stream: S,
    id: SubscriptionId,
    active: Arc<AtomicBool>,
    mut cancellation: watch::Receiver<bool>,
    sender: UnboundedSender<RuntimeDelivery>,
) where
    S: futures_util::Stream<Item = Item> + Send + 'static,
    Item: Send + 'static,
{
    let mut stream = Box::pin(stream);
    loop {
        tokio::select! {
            changed = cancellation.changed() => { if changed.is_err() || *cancellation.borrow() { break; } }
            item = stream.next() => { match item {
                Some(item) if active.load(Ordering::Acquire) => { if sender.send(RuntimeDelivery::Stream(StreamDelivery { id, event: StreamDeliveryEvent::Item(Box::new(item)) })).is_err() { break; } }
                Some(_) | None => { if active.load(Ordering::Acquire) { let _ = sender.send(RuntimeDelivery::Stream(StreamDelivery { id, event: StreamDeliveryEvent::Closed })); } break; }
            }}
        }
    }
}

pub(crate) fn cancellation() -> (Arc<AtomicBool>, watch::Sender<bool>, watch::Receiver<bool>) {
    let active = Arc::new(AtomicBool::new(true));
    let (tx, rx) = watch::channel(false);
    (active, tx, rx)
}
