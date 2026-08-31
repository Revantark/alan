//! Runtime-owned overlay stack.
//!
//! The topmost overlay receives input first and renders last. Opening an
//! overlay saves the focus path, closes capture, and schedules a redraw;
//! closing restores the previous focus. A modal is a focus boundary:
//! components behind an overlay receive no input.

use crate::entity::EntityId;

/// An overlay id (an [`EntityId`]) occupying the stack.
pub type OverlayId = EntityId;

/// Stack of open overlays; the last entry is topmost.
#[derive(Debug, Default)]
pub(crate) struct OverlayStack {
    overlays: Vec<OverlayId>,
}

impl OverlayStack {
    pub(crate) fn new() -> Self {
        Self {
            overlays: Vec::new(),
        }
    }

    /// Push an overlay, capturing input for it.
    pub(crate) fn push(&mut self, id: OverlayId) {
        self.overlays.push(id);
    }

    /// Pop the topmost overlay, returning its id.
    pub(crate) fn pop(&mut self) -> Option<OverlayId> {
        self.overlays.pop()
    }

    /// The topmost overlay, receiving input first and rendering last.
    pub(crate) fn top(&self) -> Option<OverlayId> {
        self.overlays.last().copied()
    }

    /// Whether any overlay is capturing input.
    pub(crate) fn is_active(&self) -> bool {
        !self.overlays.is_empty()
    }

    /// All overlays, bottom to top.
    pub(crate) fn overlays(&self) -> &[OverlayId] {
        &self.overlays
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::EntityId;

    fn id(value: u64) -> OverlayId {
        EntityId::from_u64(value)
    }

    #[test]
    fn push_top_pop() {
        let mut stack = OverlayStack::new();
        assert!(!stack.is_active());
        stack.push(id(1));
        stack.push(id(2));
        assert_eq!(stack.top(), Some(id(2)));
        assert_eq!(stack.pop(), Some(id(2)));
        assert_eq!(stack.top(), Some(id(1)));
        assert!(stack.is_active());
        stack.pop();
        assert!(!stack.is_active());
    }

    #[test]
    fn remove_middle_overlay() {
        let mut stack = OverlayStack::new();
        stack.push(id(1));
        stack.push(id(2));
        stack.push(id(3));
        assert_eq!(stack.top(), Some(id(3)));
        assert_eq!(stack.pop(), Some(id(3)));
        assert_eq!(stack.overlays(), &[id(1), id(2)]);
        stack.pop();
        stack.pop();
        assert!(!stack.is_active());
    }
}
