//! Entity store with typed, non-owning handles.
//!
//! Each component is behind its own lock. Re-entrant access to the current
//! entity is rejected by `Context`; missing entities are safe no-ops.

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

/// Stable identity of a stored entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EntityId(u64);
impl EntityId {
    pub(crate) fn allocate() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
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

/// Cheap, non-owning typed handle to an entity.
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
        self.id.hash(state)
    }
}
impl<T> Entity<T> {
    pub(crate) fn from_id(id: EntityId) -> Self {
        Self {
            id,
            _marker: std::marker::PhantomData,
        }
    }
    pub fn id(&self) -> EntityId {
        self.id
    }
}

pub(crate) trait ComponentSlot<A>: 'static {
    fn init(&mut self, cx: &mut Ctx<'_, A>);
    fn handle_action(&mut self, action: &A, cx: &mut Ctx<'_, A>) -> ActionStatus;
    fn render(&self, frame: &mut Frame, area: Rect, cx: &RenderContext<'_, A>);
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}
impl<S, A> ComponentSlot<A> for S
where
    S: Component<A>,
    A: 'static,
{
    fn init(&mut self, cx: &mut Ctx<'_, A>) {
        let mut typed = cx.typed::<S>();
        S::init(self, &mut typed);
    }
    fn handle_action(&mut self, action: &A, cx: &mut Ctx<'_, A>) -> ActionStatus {
        let mut typed = cx.typed::<S>();
        S::handle_action(self, action, &mut typed)
    }
    fn render(&self, frame: &mut Frame, area: Rect, cx: &RenderContext<'_, A>) {
        S::render(self, frame, area, cx);
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

type SlotValue<A> = Option<Box<dyn ComponentSlot<A>>>;
type Slot<A> = Mutex<SlotValue<A>>;

pub(crate) struct EntityStore<A> {
    slots: HashMap<EntityId, Slot<A>>,
    initialised: RefCell<HashSet<EntityId>>,
}
impl<A> Default for EntityStore<A> {
    fn default() -> Self {
        Self {
            slots: HashMap::new(),
            initialised: RefCell::new(HashSet::new()),
        }
    }
}
impl<A: 'static> EntityStore<A> {
    pub(crate) fn new() -> Self {
        Self::default()
    }
    pub(crate) fn insert<T: Component<A>>(&mut self, state: T) -> Entity<T> {
        let id = EntityId::allocate();
        self.slots.insert(id, Mutex::new(Some(Box::new(state))));
        Entity::from_id(id)
    }
    pub(crate) fn insert_slot(&mut self, id: EntityId, slot: Box<dyn ComponentSlot<A>>) {
        self.slots.insert(id, Mutex::new(Some(slot)));
    }
    pub(crate) fn init_if_needed(&self, id: EntityId, cx: &mut Ctx<'_, A>) {
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
    #[allow(dead_code)]
    pub(crate) fn is_initialised(&self, id: EntityId) -> bool {
        self.initialised.borrow().contains(&id)
    }
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
    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn contains(&self, id: EntityId) -> bool {
        self.slots.contains_key(&id)
    }
    pub(crate) fn is_active_entity(&self, id: EntityId) -> bool {
        self.slots.contains_key(&id)
    }
    pub(crate) fn lock(&self, id: EntityId) -> Option<MutexGuard<'_, SlotValue<A>>> {
        self.slots.get(&id).and_then(|slot| slot.lock().ok())
    }
    pub(crate) fn dispatch_action(
        &self,
        id: EntityId,
        action: &A,
        cx: &mut Ctx<'_, A>,
    ) -> Option<ActionStatus> {
        let mut guard = self.lock(id)?;
        Some(guard.as_mut()?.handle_action(action, cx))
    }
    pub(crate) fn render_entity(
        &self,
        id: EntityId,
        frame: &mut Frame,
        area: Rect,
        cx: &RenderContext<'_, A>,
    ) {
        if let Some(guard) = self.lock(id)
            && let Some(component) = guard.as_ref()
        {
            component.render(frame, area, cx);
        }
    }
    pub(crate) fn typed_update<E: 'static, R>(
        &self,
        id: EntityId,
        f: impl FnOnce(&mut E) -> R,
    ) -> Option<R> {
        let mut guard = self.lock(id)?;
        let state = guard.as_mut()?.as_any_mut().downcast_mut::<E>()?;
        Some(f(state))
    }
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
    use crate::context::{Context, Ctx, RuntimeState};
    use crate::task::TokioExecutor;
    use std::sync::Arc;
    struct Counter {
        value: i32,
    }
    impl Component<()> for Counter {
        fn render(&self, _: &mut Frame, _: Rect, _: &RenderContext<'_, ()>) {}
    }
    struct ActionProbe;
    impl Component<()> for ActionProbe {
        fn handle_action(&mut self, _: &(), _: &mut Context<'_, Self, ()>) -> ActionStatus {
            ActionStatus::Handled
        }
        fn render(&self, _: &mut Frame, _: Rect, _: &RenderContext<'_, ()>) {}
    }
    fn core_for<A: 'static>() -> RuntimeState<A> {
        let (sender, _) = tokio::sync::mpsc::unbounded_channel();
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
            store.typed_read(entity.id(), |c: &Counter| c.value),
            Some(2)
        );
        assert!(store.remove(entity.id()));
        assert!(!store.remove(entity.id()));
    }
    #[test]
    fn typed_access_rejects_wrong_type() {
        let mut store = EntityStore::new();
        let entity = store.insert(Counter { value: 0 });
        assert!(
            store
                .typed_update::<String, _>(entity.id(), |_| ())
                .is_none()
        );
        assert!(store.typed_read::<String, _>(entity.id(), |_| ()).is_none());
    }
    #[test]
    fn action_dispatch_returns_propagation() {
        let mut store = EntityStore::new();
        let entity = store.insert(ActionProbe);
        let mut core = core_for();
        let mut cx = Ctx::new(&mut core, &store, EntityId::allocate());
        assert_eq!(
            store.dispatch_action(entity.id(), &(), &mut cx),
            Some(ActionStatus::Handled)
        );
    }
    #[test]
    fn missing_entity_is_a_safe_no_op() {
        let store = EntityStore::new();
        let mut core = core_for();
        let missing = EntityId::allocate();
        let mut cx = Ctx::new(&mut core, &store, missing);
        assert_eq!(store.dispatch_action(missing, &(), &mut cx), None);
    }
}
