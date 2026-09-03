//! Runtime-internal focus management.
//!
//! Focus is application state; Ratatui does not infer it from rendering.
//! Focus targets entities directly: there are no user-facing handles.
//! A parent explicitly focuses a child with
//! [`Context::focus_entity`](crate::context::Context::focus_entity) and may
//! declare a cycling order with
//! [`Context::focus_order`](crate::context::Context::focus_order).
//!
//! The manager tracks the current focus, supports next/previous cycling
//! within a declared order, and saves/restores focus paths around overlays.
//! When an entity is removed, every trace of it disappears: its declared
//! orders are dropped, its entries are filtered from other owners' orders,
//! and focus paths that point at it are cleared — a closed overlay leaves no
//! focus behind.

use crate::entity::EntityId;

/// A cycling order declared by one entity (its children, for example).
#[derive(Debug)]
struct RegisteredOrder {
    owner: EntityId,
    entities: Vec<EntityId>,
}

/// Runtime focus state: current entity, declared cycling orders, saved paths.
#[derive(Debug, Default)]
pub(crate) struct FocusManager {
    orders: Vec<RegisteredOrder>,
    current: Option<EntityId>,
    saved: Vec<Option<EntityId>>,
}

impl FocusManager {
    /// Register a cycling order owned by `owner`.
    pub(crate) fn register_order(&mut self, owner: EntityId, entities: Vec<EntityId>) {
        self.orders.push(RegisteredOrder { owner, entities });
    }

    /// Remove every trace of an entity: orders it owns, entries pointing at
    /// it, and focus paths that reference it.
    pub(crate) fn remove_entity(&mut self, id: EntityId) {
        for order in &mut self.orders {
            order.entities.retain(|entity| *entity != id);
        }
        self.orders
            .retain(|order| order.owner != id && !order.entities.is_empty());
        if self.current == Some(id) {
            self.current = None;
        }
        for saved in &mut self.saved {
            if *saved == Some(id) {
                *saved = None;
            }
        }
    }

    /// Set the current focus.
    pub(crate) fn focus(&mut self, id: EntityId) {
        self.current = Some(id);
    }

    /// The currently focused entity, if any.
    pub(crate) fn current(&self) -> Option<EntityId> {
        self.current
    }

    /// Focus the next entity in the active entity's declared order (wrapping).
    /// With no current focus, focuses the first entity of the first order.
    pub(crate) fn focus_next(&mut self) {
        self.cycle(1);
    }

    /// Focus the previous entity in the active entity's declared order
    /// (wrapping).
    pub(crate) fn focus_prev(&mut self) {
        self.cycle(-1);
    }

    fn cycle(&mut self, direction: isize) {
        let Some(current) = self.current else {
            if let Some(order) = self.orders.first() {
                self.current = order.entities.first().copied();
            }
            return;
        };
        let Some(entities) = self
            .orders
            .iter()
            .find(|order| order.entities.contains(&current))
            .map(|order| &order.entities)
        else {
            return;
        };
        let index = entities
            .iter()
            .position(|entity| *entity == current)
            .unwrap_or(0);
        let count = entities.len() as isize;
        let next = (index as isize + direction).rem_euclid(count) as usize;
        self.current = Some(entities[next]);
    }

    /// Save the current focus path (before opening an overlay).
    pub(crate) fn save(&mut self) {
        self.saved.push(self.current);
    }

    /// Restore the previously saved focus path (after closing an overlay).
    pub(crate) fn restore(&mut self) {
        self.current = self.saved.pop().flatten();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order() -> (EntityId, EntityId, EntityId, FocusManager) {
        let owner = EntityId::allocate();
        let (a, b, c) = (
            EntityId::allocate(),
            EntityId::allocate(),
            EntityId::allocate(),
        );
        let mut manager = FocusManager::default();
        manager.register_order(owner, vec![a, b, c]);
        (a, b, c, manager)
    }

    #[test]
    fn no_focus_starts_first_entity_on_next() {
        let (a, _, _, mut manager) = order();
        assert_eq!(manager.current(), None);
        manager.focus_next();
        assert_eq!(manager.current(), Some(a));
        manager.focus_next();
        manager.focus_next();
        manager.focus_next();
        assert_eq!(manager.current(), Some(a), "cycles back to first");
    }

    #[test]
    fn focus_prev_wraps_backwards() {
        let (a, _, c, mut manager) = order();
        manager.focus_next();
        manager.focus_prev();
        assert_eq!(manager.current(), Some(c));
        manager.focus_next();
        assert_eq!(manager.current(), Some(a));
    }

    #[test]
    fn explicit_focus_outside_any_order_does_not_cycle() {
        let (_, _, _, mut manager) = order();
        let standalone = EntityId::allocate();
        manager.focus(standalone);
        assert_eq!(manager.current(), Some(standalone));
        manager.focus_next();
        assert_eq!(manager.current(), Some(standalone));
    }

    #[test]
    fn cycles_within_the_order_containing_the_current_entity() {
        let (_, _, _, mut manager) = order();
        let owner = EntityId::allocate();
        let (x, y) = (EntityId::allocate(), EntityId::allocate());
        manager.register_order(owner, vec![x, y]);
        let standalone = EntityId::allocate();
        manager.focus(standalone);
        manager.focus_next();
        assert_eq!(manager.current(), Some(standalone), "no order contains it");
        manager.focus(x);
        manager.focus_next();
        assert_eq!(manager.current(), Some(y));
        manager.focus_next();
        assert_eq!(
            manager.current(),
            Some(x),
            "wraps within x/y, not into a/b/c"
        );
    }

    #[test]
    fn save_and_restore() {
        let (_, _, _, mut manager) = order();
        manager.focus_next();
        let before = manager.current();
        manager.save();
        manager.focus_next();
        assert_ne!(manager.current(), before);
        manager.restore();
        assert_eq!(manager.current(), before);
    }

    #[test]
    fn removing_entity_drops_its_orders_and_references() {
        let owner = EntityId::allocate();
        let a = EntityId::allocate();
        let mut manager = FocusManager::default();
        manager.register_order(owner, vec![a]);
        manager.focus(a);
        manager.remove_entity(a);
        assert_eq!(manager.current(), None);
    }

    #[test]
    fn removing_entity_cleans_it_from_other_owners_orders() {
        let owner = EntityId::allocate();
        let (a, b) = (EntityId::allocate(), EntityId::allocate());
        let mut manager = FocusManager::default();
        manager.register_order(owner, vec![a, b]);
        manager.focus(b);
        manager.remove_entity(a);
        manager.focus_prev();
        assert_eq!(manager.current(), Some(b));
    }

    #[test]
    fn removing_entity_clears_saved_focus() {
        let owner = EntityId::allocate();
        let a = EntityId::allocate();
        let mut manager = FocusManager::default();
        manager.register_order(owner, vec![a]);
        manager.focus(a);
        manager.save();
        manager.remove_entity(a);
        manager.restore();
        assert_eq!(manager.current(), None);
    }
}
