//! Draw pass. Rendering is immediate-mode, read-only, and performs no I/O.

use crate::component::RenderContext;
use crate::entity::{EntityId, EntityStore};
use crate::overlay::OverlayStack;
use ratatui::Frame;
use ratatui::layout::Rect;

pub(crate) fn draw<A: 'static>(
    root: EntityId,
    overlays: &OverlayStack,
    store: &EntityStore<A>,
    frame: &mut Frame,
    focused: Option<EntityId>,
) {
    let area = frame.area();
    render_entity(store, root, frame, area, focused);
    if overlays.is_active() {
        for &overlay in overlays.overlays() {
            render_entity(store, overlay, frame, area, focused);
        }
    }
}
fn render_entity<A: 'static>(
    store: &EntityStore<A>,
    id: EntityId,
    frame: &mut Frame,
    area: Rect,
    focused: Option<EntityId>,
) {
    let cx = RenderContext::new(store, focused, Some(id));
    store.render_entity(id, frame, area, &cx);
}
