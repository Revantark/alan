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
    store.clear_mouse_areas();
    let area = frame.area();
    render_entity(store, root, frame, area, focused);
    if overlays.is_active() {
        for &overlay in overlays.overlays() {
            render_entity(store, overlay, frame, area, focused);
        }
        // Active overlays consume the whole frame: pointer hits anywhere
        // route to the topmost overlay, which decides how to react.
        store.record_mouse_area(
            *overlays.overlays().last().expect("overlays non-empty"),
            area,
        );
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
