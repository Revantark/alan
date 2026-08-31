//! Draw pass.
//!
//! Rendering is immediate-mode: layout is recalculated every frame, state is
//! only read, and overlays render after (on top of) the screen content. The
//! root renders the full frame area; overlays render their own areas on top.

use ratatui::Frame;

use crate::component::RenderContext;
use crate::entity::{EntityId, EntityStore};
use crate::overlay::OverlayStack;

/// Draw the root component across the whole frame, then overlays bottom to
/// top, topmost last.
///
/// The screen (root) renders first; each overlay renders above it in stack
/// order. Visible focus styling is a component responsibility: components
/// render highlighted borders or cursors when their bound focus handle
/// matches the runtime's current focus.
pub(crate) fn draw<A: 'static, M: 'static>(
    root: EntityId,
    overlays: &OverlayStack,
    store: &EntityStore<A, M>,
    frame: &mut Frame,
) {
    let area = frame.area();
    let cx = RenderContext::new(store);
    if overlays.is_active() {
        // Behind a modal the screen still renders (it stays visible), but
        // receives no input; the overlay renders on top.
        store.render_entity(root, frame, area, &cx);
        for &overlay in overlays.overlays() {
            store.render_entity(overlay, frame, area, &cx);
        }
    } else {
        store.render_entity(root, frame, area, &cx);
    }
}
