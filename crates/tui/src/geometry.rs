//! Small geometry helpers shared across hit-testing and selection code.

use ratatui::layout::Rect;

/// Whether the point (`col`, `row`) lies inside `rect` (half-open on the
/// right/bottom edges).
pub fn rect_contains(rect: Rect, col: u16, row: u16) -> bool {
    col >= rect.x
        && col < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}
