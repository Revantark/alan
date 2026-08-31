//! Explicit focus management.
//!
//! Focus is application state; Ratatui does not infer it from rendering.
//! Handles are stable identities, not screen coordinates. The manager tracks
//! the current focus, supports next/previous within a registered scope, and
//! saves/restores focus paths around overlays.
//!
//! Registrations keep their owning entity so focus state can be dropped when
//! that entity is removed: a closed overlay must leave no focus behind, or
//! cycling would walk through components that no longer exist.

use std::collections::HashSet;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::entity::EntityId;

fn next_counter() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Identity of a focusable control.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FocusId(u64);

impl fmt::Display for FocusId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "focus-{}", self.0)
    }
}

/// Identity of a focus scope (an ordered group of handles).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ScopeId(u64);

impl ScopeId {
    fn allocate() -> Self {
        Self(next_counter())
    }
}

/// Stable handle to a focusable component.
///
/// Copies cheaply; compare by value. Created through [`FocusScope::handle`]
/// (ordered, for tab cycling) or [`FocusHandle::new`] (standalone, e.g. for
/// popups).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FocusHandle {
    pub(crate) id: FocusId,
    pub(crate) scope: ScopeId,
}

impl FocusHandle {
    /// Create a standalone handle outside any registered scope.
    pub fn new() -> Self {
        Self {
            id: FocusId(next_counter()),
            scope: ScopeId(next_counter()),
        }
    }

    /// The handle's focus id, used for visible focus styling.
    pub fn id(&self) -> FocusId {
        self.id
    }
}

impl Default for FocusHandle {
    fn default() -> Self {
        Self::new()
    }
}

/// An ordered group of focus handles, cycled with next/previous.
#[derive(Debug, Clone)]
pub struct FocusScope {
    id: ScopeId,
    handles: Vec<FocusHandle>,
}

impl Default for FocusScope {
    fn default() -> Self {
        Self::new()
    }
}

impl FocusScope {
    /// Create an empty scope.
    pub fn new() -> Self {
        Self {
            id: ScopeId::allocate(),
            handles: Vec::new(),
        }
    }

    /// Create and register a handle in this scope, in tab order.
    pub fn handle(&mut self) -> FocusHandle {
        let handle = FocusHandle {
            id: FocusId(next_counter()),
            scope: self.id,
        };
        self.handles.push(handle);
        handle
    }

    /// Handles in registration (tab) order.
    pub fn handles(&self) -> &[FocusHandle] {
        &self.handles
    }
}

#[derive(Debug)]
struct RegisteredScope {
    owner: EntityId,
    scope: FocusScope,
}

/// Runtime focus state: current handle, registered scopes, saved paths.
#[derive(Debug, Default)]
pub(crate) struct FocusManager {
    scopes: Vec<RegisteredScope>,
    current: Option<FocusHandle>,
    saved: Vec<Option<FocusHandle>>,
}

impl FocusManager {
    /// Register a scope so its handles participate in next/previous cycling.
    pub(crate) fn register(&mut self, owner: EntityId, scope: FocusScope) {
        self.scopes.push(RegisteredScope { owner, scope });
    }

    /// Remove all state associated with an entity owner.
    pub(crate) fn remove_entity(&mut self, owner: EntityId) {
        let removed_scope_ids: HashSet<ScopeId> = self
            .scopes
            .iter()
            .filter(|registered| registered.owner == owner)
            .map(|registered| registered.scope.id)
            .collect();
        self.scopes.retain(|registered| registered.owner != owner);
        if self
            .current
            .is_some_and(|handle| removed_scope_ids.contains(&handle.scope))
        {
            self.current = None;
        }
        for saved in &mut self.saved {
            if saved.is_some_and(|handle| removed_scope_ids.contains(&handle.scope)) {
                *saved = None;
            }
        }
    }

    /// The scope registered for a handle, if any.
    fn scope_of(&self, handle: &FocusHandle) -> Option<&FocusScope> {
        self.scopes
            .iter()
            .find(|s| s.scope.id == handle.scope)
            .map(|s| &s.scope)
    }

    /// Set the current focus.
    pub(crate) fn focus(&mut self, handle: FocusHandle) {
        self.current = Some(handle);
    }

    /// The currently focused handle, if any.
    pub(crate) fn current(&self) -> Option<FocusHandle> {
        self.current
    }

    /// Focus the next handle in the current handle's scope (wrapping).
    /// With no current focus, focuses the first registered handle.
    pub(crate) fn focus_next(&mut self) {
        self.cycle(1);
    }

    /// Focus the previous handle in the current handle's scope (wrapping).
    pub(crate) fn focus_prev(&mut self) {
        self.cycle(-1);
    }

    fn cycle(&mut self, direction: isize) {
        let Some(current) = self.current else {
            if let Some(handle) = self.scopes.first().and_then(|s| s.scope.handles.first()) {
                self.current = Some(*handle);
            }
            return;
        };
        let Some(scope) = self.scope_of(&current) else {
            return;
        };
        let handles = &scope.handles;
        if handles.is_empty() {
            return;
        }
        let index = handles.iter().position(|h| h.id == current.id).unwrap_or(0);
        let count = handles.len() as isize;
        let next = (index as isize + direction).rem_euclid(count) as usize;
        self.current = Some(handles[next]);
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

    fn manager() -> FocusManager {
        let mut scope = FocusScope::new();
        let _a = scope.handle();
        let _b = scope.handle();
        let _c = scope.handle();
        let mut manager = FocusManager::default();
        manager.register(EntityId::from_u64(0), scope);
        manager
    }

    #[test]
    fn handles_are_unique() {
        let mut scope = FocusScope::new();
        let a = scope.handle();
        let b = scope.handle();
        assert_ne!(a, b);
        assert_eq!(scope.handles().len(), 2);
    }

    #[test]
    fn no_focus_starts_first_handle_on_next() {
        let mut manager = manager();
        assert_eq!(manager.current(), None);
        manager.focus_next();
        let first = manager.current().unwrap();
        manager.focus_next();
        manager.focus_next();
        manager.focus_next();
        assert_eq!(manager.current(), Some(first), "cycles back to first");
    }

    #[test]
    fn focus_prev_wraps_backwards() {
        let mut manager = manager();
        manager.focus_next();
        let first = manager.current().unwrap();
        manager.focus_prev();
        let last = manager.current().unwrap();
        assert_ne!(first, last);
        manager.focus_next();
        assert_eq!(manager.current(), Some(first));
    }

    #[test]
    fn explicit_focus_then_cycle() {
        let mut manager = manager();
        let mut scope = FocusScope::new();
        let handle = scope.handle();
        manager.focus(handle);
        assert_eq!(manager.current(), Some(handle));
        // Handle's scope is unregistered: cycling is a no-op.
        manager.focus_next();
        assert_eq!(manager.current(), Some(handle));
    }

    #[test]
    fn save_and_restore() {
        let mut manager = manager();
        manager.focus_next();
        let before = manager.current();
        manager.save();
        manager.focus_next();
        assert_ne!(manager.current(), before);
        manager.restore();
        assert_eq!(manager.current(), before);
    }

    #[test]
    fn removing_scope_owner_removes_it_from_cycling() {
        let owner = EntityId::from_u64(42);
        let mut scope = FocusScope::new();
        let handle = scope.handle();
        let mut manager = FocusManager::default();
        manager.register(owner, scope);
        manager.focus(handle);
        manager.remove_entity(owner);
        manager.focus_next();
        assert_eq!(manager.current(), None);
    }

    #[test]
    fn removed_scope_is_cleared_from_saved_focus() {
        let owner = EntityId::from_u64(43);
        let mut scope = FocusScope::new();
        let handle = scope.handle();
        let mut manager = FocusManager::default();
        manager.register(owner, scope);
        manager.focus(handle);
        manager.save();
        manager.remove_entity(owner);
        manager.restore();
        assert_eq!(manager.current(), None);
    }

    #[test]
    fn detached_handles_do_not_cycle() {
        let mut manager = FocusManager::default();
        let handle = FocusHandle::new();
        manager.focus(handle);
        manager.focus_next();
        assert_eq!(manager.current(), Some(handle));
    }
}
