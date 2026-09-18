//! Integration-style test: mirrors the production path exactly — `render::draw`
//! on a real terminal buffer, then `hit_test` + `dispatch_mouse` as the event
//! loop does. Regression test for the parent-before-child area ordering bug.

use crate::context::{Ctx, RuntimeState};
use crate::entity::{Entity, EntityStore};
use crate::overlay::OverlayStack;
use crate::render;
use crate::task::TokioExecutor;
use crate::{ActionStatus, Component, RenderContext};
use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use ratatui::widgets::{Paragraph, Widget};
use std::sync::Arc;
use tokio::sync::mpsc::unbounded_channel;

type A = ();

struct Pane {
    hit_count: std::rc::Rc<std::cell::Cell<u32>>,
}

impl Component<A> for Pane {
    fn handle_mouse(
        &mut self,
        _mouse: MouseEvent,
        _area: Rect,
        _cx: &mut crate::context::Context<'_, Self, A>,
    ) -> ActionStatus {
        self.hit_count.set(self.hit_count.get() + 1);
        ActionStatus::Handled
    }

    fn render(&self, frame: &mut ratatui::Frame, area: Rect, _cx: &RenderContext<'_, A>) {
        Paragraph::new("content").render(area, frame.buffer_mut());
    }
}

struct Root {
    pane: Option<Entity<Pane>>,
    hits: std::rc::Rc<std::cell::Cell<u32>>,
}

impl Component<A> for Root {
    fn init(&mut self, cx: &mut crate::context::Context<'_, Self, A>) {
        let pane = cx.insert(Pane {
            hit_count: self.hits.clone(),
        });
        self.pane = Some(pane);
    }

    fn render(&self, frame: &mut ratatui::Frame, area: Rect, cx: &RenderContext<'_, A>) {
        let inner = Rect::new(area.x + 2, area.y + 1, area.width - 4, area.height - 2);
        if let Some(pane) = self.pane {
            cx.render_entity(pane, frame, inner);
        }
    }
}

/// Mirrors the event loop's `flush_requests`: inserts queued by `init` land
/// in the store here, not synchronously during `cx.insert`.
fn flush_pending(core: &mut RuntimeState<A>, store: &mut EntityStore<A>) {
    while let Some((id, slot)) = core.pending_inserts.pop_front() {
        store.insert_slot(id, slot);
    }
    for id in std::mem::take(&mut core.pending_inits) {
        let mut cx = Ctx::new(core, store, id);
        store.init_if_needed(id, &mut cx);
    }
}

fn core_for() -> RuntimeState<A> {
    let (sender, _) = unbounded_channel();
    RuntimeState::new(sender, Arc::new(TokioExecutor))
}

#[test]
fn draw_then_mouse_hits_child_not_root() {
    let hits = std::rc::Rc::new(std::cell::Cell::new(0));
    let mut store = EntityStore::new();
    let root = store.insert(Root {
        pane: None,
        hits: hits.clone(),
    });
    let mut core = core_for();
    {
        let mut cx = Ctx::new(&mut core, &store, root.id());
        store.init_if_needed(root.id(), &mut cx);
    }
    flush_pending(&mut core, &mut store);
    let pane = store
        .typed_read(root.id(), |r: &Root| r.pane.map(|p| p.id()))
        .flatten()
        .expect("pane inserted by init");

    // Production draw pass over a real buffer.
    let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
    terminal
        .draw(|frame| {
            render::draw(root.id(), &OverlayStack::new(), &store, frame, None);
        })
        .unwrap();

    // Point inside the child's inner area (offset 2,1 from the frame origin).
    let (col, row) = (5u16, 3u16);
    let hit = store
        .hit_test(col, row)
        .expect("hit_test should find a hit");
    assert_eq!(hit, pane, "hit_test should return the pane, not root");

    let mouse = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: col,
        row,
        modifiers: KeyModifiers::NONE,
    };
    let mut cx = Ctx::new(&mut core, &store, hit);
    assert_eq!(
        store.dispatch_mouse(hit, mouse, &mut cx),
        ActionStatus::Handled
    );
    assert_eq!(hits.get(), 1, "pane should have received the mouse event");
}
