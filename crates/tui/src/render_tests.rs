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

    fn render(&self, frame: &mut ratatui::Frame, area: Rect, _cx: &RenderContext<'_, '_, A>) {
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

    fn render(&self, frame: &mut ratatui::Frame, area: Rect, cx: &RenderContext<'_, '_, A>) {
        let inner = Rect::new(area.x + 2, area.y + 1, area.width - 4, area.height - 2);
        if let Some(pane) = self.pane {
            cx.render_entity(pane, frame, inner);
        }
    }
}

/// Mirrors the event loop's `flush_requests`: inserts queued by `init` land
/// in the store here, not synchronously during `cx.insert`. Loops until
/// nothing is pending, because children inserted during an `init` queue
/// their own inserts/inits.
fn flush_pending(core: &mut RuntimeState<A>, store: &mut EntityStore<A>) {
    loop {
        while let Some((id, slot)) = core.pending_inserts.pop_front() {
            store.insert_slot(id, slot);
        }
        let pending = std::mem::take(&mut core.pending_inits);
        if pending.is_empty() && core.pending_inserts.is_empty() {
            break;
        }
        for id in pending {
            let mut cx = Ctx::new(core, store, id);
            store.init_if_needed(id, &mut cx);
        }
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

// --- render_with_state / cx.state tests -------------------------------------

use std::cell::RefCell as StdRefCell;
use std::rc::Rc as StdRc;

/// Records the typed state seen during render, or "" when none.
struct StateReader {
    seen: StdRc<StdRefCell<String>>,
    expect: &'static str,
}

impl Component<A> for StateReader {
    fn render(&self, _frame: &mut ratatui::Frame, _area: Rect, cx: &RenderContext<'_, '_, A>) {
        let text = match cx.state::<String>() {
            Some(s) => s.clone(),
            None => "(none)".to_string(),
        };
        assert_eq!(text, self.expect, "state seen by StateReader");
        self.seen.borrow_mut().push_str(&text);
        self.seen.borrow_mut().push('\n');
    }
}

struct Provider {
    child: Option<Entity<StateReader>>,
    middle: Option<Entity<Middle>>,
    log: StdRc<StdRefCell<String>>,
}

impl Component<A> for Provider {
    fn init(&mut self, cx: &mut crate::context::Context<'_, Self, A>) {
        self.child = Some(cx.insert(StateReader {
            seen: self.log.clone(),
            expect: "hello",
        }));
        self.middle = Some(cx.insert(Middle {
            leaf: None,
            log: self.log.clone(),
        }));
    }

    fn render(&self, frame: &mut ratatui::Frame, area: Rect, cx: &RenderContext<'_, '_, A>) {
        let half = Rect::new(area.x, area.y, area.width / 2, area.height);
        let other = Rect::new(area.x + area.width / 2, area.y, area.width / 2, area.height);
        // Child reads the provided state.
        if let Some(child) = self.child {
            cx.render_with_state(child, frame, half, &"hello".to_string());
        }
        // Middle renders its own child via plain render_entity; the state
        // provided to Middle propagates down to it.
        if let Some(middle) = self.middle {
            cx.render_with_state(middle, frame, other, &"hello".to_string());
        }
    }
}

/// Renders its child with plain `render_entity`: the state provided to it by
/// its parent must stay visible to that child.
struct Middle {
    leaf: Option<Entity<StateReader>>,
    log: StdRc<StdRefCell<String>>,
}

impl Component<A> for Middle {
    fn init(&mut self, cx: &mut crate::context::Context<'_, Self, A>) {
        self.leaf = Some(cx.insert(StateReader {
            seen: self.log.clone(),
            expect: "hello",
        }));
    }

    fn render(&self, frame: &mut ratatui::Frame, area: Rect, cx: &RenderContext<'_, '_, A>) {
        if let Some(leaf) = self.leaf {
            cx.render_entity(leaf, frame, area);
        }
    }
}

#[test]
fn provided_state_reaches_child_and_propagates_to_grandchild() {
    let log = StdRc::new(StdRefCell::new(String::new()));
    let mut store = EntityStore::new();
    let root = store.insert(Provider {
        child: None,
        middle: None,
        log: log.clone(),
    });
    let mut core = core_for();
    {
        let mut cx = Ctx::new(&mut core, &store, root.id());
        store.init_if_needed(root.id(), &mut cx);
    }
    flush_pending(&mut core, &mut store);

    let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
    terminal
        .draw(|frame| {
            render::draw(root.id(), &OverlayStack::new(), &store, frame, None);
        })
        .unwrap();

    assert_eq!(*log.borrow(), "hello\nhello\n");
}

struct Shadower {
    leaf: Option<Entity<StateReader>>,
    log: StdRc<StdRefCell<String>>,
}

impl Component<A> for Shadower {
    fn init(&mut self, cx: &mut crate::context::Context<'_, Self, A>) {
        self.leaf = Some(cx.insert(StateReader {
            seen: self.log.clone(),
            expect: "shadow",
        }));
    }

    fn render(&self, frame: &mut ratatui::Frame, area: Rect, cx: &RenderContext<'_, '_, A>) {
        if let Some(leaf) = self.leaf {
            // Shadows the "hello" provided by the ancestor.
            cx.render_with_state(leaf, frame, area, &"shadow".to_string());
        }
    }
}

struct ShadowRoot {
    reader: Option<Entity<StateReader>>,
    shadower: Option<Entity<Shadower>>,
    log: StdRc<StdRefCell<String>>,
}

impl Component<A> for ShadowRoot {
    fn init(&mut self, cx: &mut crate::context::Context<'_, Self, A>) {
        self.reader = Some(cx.insert(StateReader {
            seen: self.log.clone(),
            expect: "outer",
        }));
        self.shadower = Some(cx.insert(Shadower {
            leaf: None,
            log: self.log.clone(),
        }));
    }

    fn render(&self, frame: &mut ratatui::Frame, area: Rect, cx: &RenderContext<'_, '_, A>) {
        let top = Rect::new(area.x, area.y, area.width, 1);
        let bottom = Rect::new(area.x, area.y + 1, area.width, area.height - 1);
        if let Some(reader) = self.reader {
            cx.render_with_state(reader, frame, top, &"outer".to_string());
        }
        if let Some(shadower) = self.shadower {
            cx.render_entity(shadower, frame, bottom);
        }
    }
}

#[test]
fn nested_render_with_state_shadows_outer() {
    let log = StdRc::new(StdRefCell::new(String::new()));
    let mut store = EntityStore::new();
    let root = store.insert(ShadowRoot {
        reader: None,
        shadower: None,
        log: log.clone(),
    });
    let mut core = core_for();
    {
        let mut cx = Ctx::new(&mut core, &store, root.id());
        store.init_if_needed(root.id(), &mut cx);
    }
    flush_pending(&mut core, &mut store);

    let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
    terminal
        .draw(|frame| {
            render::draw(root.id(), &OverlayStack::new(), &store, frame, None);
        })
        .unwrap();

    assert_eq!(*log.borrow(), "outer\nshadow\n");
}

/// No provider anywhere: `state()` must be `None`.
struct BareReader {
    seen: StdRc<StdRefCell<bool>>,
}

impl Component<A> for BareReader {
    fn render(&self, _frame: &mut ratatui::Frame, _area: Rect, cx: &RenderContext<'_, '_, A>) {
        assert!(cx.state::<String>().is_none(), "no state should be visible");
        *self.seen.borrow_mut() = true;
    }
}

struct BareRoot {
    reader: Option<Entity<BareReader>>,
    seen: StdRc<StdRefCell<bool>>,
}

impl Component<A> for BareRoot {
    fn init(&mut self, cx: &mut crate::context::Context<'_, Self, A>) {
        self.reader = Some(cx.insert(BareReader {
            seen: self.seen.clone(),
        }));
    }

    fn render(&self, frame: &mut ratatui::Frame, area: Rect, cx: &RenderContext<'_, '_, A>) {
        if let Some(reader) = self.reader {
            cx.render_entity(reader, frame, area);
        }
    }
}

#[test]
fn no_provider_means_no_state() {
    let seen = StdRc::new(StdRefCell::new(false));
    let mut store = EntityStore::new();
    let root = store.insert(BareRoot {
        reader: None,
        seen: seen.clone(),
    });
    let mut core = core_for();
    {
        let mut cx = Ctx::new(&mut core, &store, root.id());
        store.init_if_needed(root.id(), &mut cx);
    }
    flush_pending(&mut core, &mut store);

    let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
    terminal
        .draw(|frame| {
            render::draw(root.id(), &OverlayStack::new(), &store, frame, None);
        })
        .unwrap();

    assert!(*seen.borrow(), "bare reader rendered");
}

#[test]
fn expect_state_panics_without_provider() {
    struct Panner;
    impl Component<A> for Panner {
        fn render(&self, _: &mut ratatui::Frame, _: Rect, cx: &RenderContext<'_, '_, A>) {
            let _ = cx.expect_state::<String>();
        }
    }
    struct PannerRoot {
        child: Option<Entity<Panner>>,
    }
    impl Component<A> for PannerRoot {
        fn init(&mut self, cx: &mut crate::context::Context<'_, Self, A>) {
            self.child = Some(cx.insert(Panner));
        }
        fn render(&self, frame: &mut ratatui::Frame, area: Rect, cx: &RenderContext<'_, '_, A>) {
            if let Some(child) = self.child {
                // Plain render_entity: no state provided.
                cx.render_entity(child, frame, area);
            }
        }
    }

    let mut store = EntityStore::new();
    let root = store.insert(PannerRoot { child: None });
    let mut core = core_for();
    {
        let mut cx = Ctx::new(&mut core, &store, root.id());
        store.init_if_needed(root.id(), &mut cx);
    }
    flush_pending(&mut core, &mut store);

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        terminal
            .draw(|frame| {
                render::draw(root.id(), &OverlayStack::new(), &store, frame, None);
            })
            .unwrap();
    }));
    assert!(
        result.is_err(),
        "expect_state must panic without a provider"
    );
}

#[test]
fn expect_state_returns_value_when_provided() {
    struct Reader {
        seen: StdRc<StdRefCell<String>>,
    }
    impl Component<A> for Reader {
        fn render(&self, _: &mut ratatui::Frame, _: Rect, cx: &RenderContext<'_, '_, A>) {
            let state: &String = cx.expect_state::<String>();
            self.seen.borrow_mut().push_str(state);
        }
    }
    struct Root {
        child: Option<Entity<Reader>>,
        seen: StdRc<StdRefCell<String>>,
    }
    impl Component<A> for Root {
        fn init(&mut self, cx: &mut crate::context::Context<'_, Self, A>) {
            self.child = Some(cx.insert(Reader {
                seen: self.seen.clone(),
            }));
        }
        fn render(&self, frame: &mut ratatui::Frame, area: Rect, cx: &RenderContext<'_, '_, A>) {
            if let Some(child) = self.child {
                cx.render_with_state(child, frame, area, &"provided".to_string());
            }
        }
    }

    let seen = StdRc::new(StdRefCell::new(String::new()));
    let mut store = EntityStore::new();
    let root = store.insert(Root {
        child: None,
        seen: seen.clone(),
    });
    let mut core = core_for();
    {
        let mut cx = Ctx::new(&mut core, &store, root.id());
        store.init_if_needed(root.id(), &mut cx);
    }
    flush_pending(&mut core, &mut store);

    let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
    terminal
        .draw(|frame| {
            render::draw(root.id(), &OverlayStack::new(), &store, frame, None);
        })
        .unwrap();

    assert_eq!(*seen.borrow(), "provided");
}
