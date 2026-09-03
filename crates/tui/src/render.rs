//! Draw pass.
//!
//! Rendering is immediate-mode: layout is recalculated every frame, state is
//! only read, and overlays render after (on top of) the screen content. The
//! root renders the full frame area; overlays render their own areas on top.

use ratatui::Frame;
use ratatui::layout::Rect;

use crate::component::RenderContext;
use crate::entity::{EntityId, EntityStore};
use crate::overlay::OverlayStack;

/// Draw the root component across the whole frame, then overlays bottom to
/// top, topmost last.
///
/// The screen (root) renders first; each overlay renders above it in stack
/// order. Every rendered entity receives a render context that knows both
/// which entity it belongs to and which entity holds focus, so components can
/// draw visible focus styling through
/// [`RenderContext::is_focused`](crate::RenderContext::is_focused).
pub(crate) fn draw<A: 'static, M: 'static>(
    root: EntityId,
    overlays: &OverlayStack,
    store: &EntityStore<A, M>,
    frame: &mut Frame,
    focused: Option<EntityId>,
) {
    let area = frame.area();

    if overlays.is_active() {
        // Behind a modal the screen still renders (it stays visible), but
        // receives no input; the overlay renders on top.
        render_entity(store, root, frame, area, focused);
        for &overlay in overlays.overlays() {
            render_entity(store, overlay, frame, area, focused);
        }
    } else {
        render_entity(store, root, frame, area, focused);
    }
}

/// Render one top-level entity with a context that identifies it.
fn render_entity<A: 'static, M: 'static>(
    store: &EntityStore<A, M>,
    id: EntityId,
    frame: &mut Frame,
    area: Rect,
    focused: Option<EntityId>,
) {
    let cx = RenderContext::new(store, focused, Some(id));
    store.render_entity(id, frame, area, &cx);
}
