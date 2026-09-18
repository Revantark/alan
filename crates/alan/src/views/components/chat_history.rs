use super::transcript::{TranscriptLayout, scrollbar_position};
use crate::core::ChatController;
use crate::root::AlanAction;
use crate::views::theme;
use alan_tui::component::{ActionStatus, Component, RenderContext};
use alan_tui::context::Context;
use alan_tui::selection;
use alan_tui::selection::{Selection, TextPosition};
use alan_tui::{Subscription, SubscriptionEvent};
use crossterm::event::{Event, KeyCode, KeyEventKind, MouseButton, MouseEvent, MouseEventKind};
use futures_util::Stream;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::Text;
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};
use std::cell::RefCell;
use std::time::{Duration, Instant};

/// Lines moved per mouse-wheel notch.
const WHEEL_LINES_PER_NOTCH: isize = 1;

/// Hard bound on accumulated wheel notches.
const MAX_PENDING_WHEEL: isize = 48;

const MOMENTUM_IDLE_TICKS: u16 = 16;

/// Keeps track of entries
pub struct ChatHistory {
    view: RefCell<View>,
    /// for scrolling
    momentum: Option<Subscription>,
    momentum_idle: u16,
}

struct View {
    layout: TranscriptLayout,
    /// Current rendered top line.
    scroll_offset: usize,
    /// Desired top line. Kept equal to `scroll_offset` for immediate input response.
    scroll_target: usize,
    /// Keep viewport pinned to newest content while true.
    follow_output: bool,
    /// Last rendered viewport height. Input events use it before the next
    /// render clamps it again.
    viewport_height: usize,
    max_scroll: usize,
    /// Active text selection in the transcript.
    selection: Option<Selection>,
    /// Wheel notches accumulated since the last flush. Trackpad swipes emit
    /// hundreds of events; they are coalesced and applied once per tick
    /// instead of one redraw per event.
    pending_wheel: isize,
    /// Last click timestamp and position for double-click detection.
    last_click: Option<(Instant, u16, u16)>,
}

impl Default for View {
    fn default() -> Self {
        Self {
            layout: TranscriptLayout::default(),
            scroll_offset: 0,
            scroll_target: 0,
            follow_output: true,
            viewport_height: 0,
            max_scroll: 0,
            selection: None,
            pending_wheel: 0,
            last_click: None,
        }
    }
}

impl ChatHistory {
    pub fn new() -> Self {
        Self {
            view: RefCell::new(View::default()),
            momentum: None,
            momentum_idle: 0,
        }
    }

    /// Whether wheel notches are queued and need a flush this tick.
    pub fn has_pending_wheel(&self) -> bool {
        self.view.borrow().pending_wheel != 0
    }

    fn tick(&mut self) -> bool {
        self.view.borrow_mut().flush_wheel()
    }

    /// Start the momentum ticker if wheel notches are queued and it is not
    /// already running. The ticker applies one capped flush step per
    /// `MOMENTUM_TICK_INTERVAL`, requests a redraw only when the viewport
    /// actually moved, and cancels itself after `MOMENTUM_IDLE_TICKS` idle
    /// ticks — so no timer runs once scrolling settles.
    fn ensure_momentum(&mut self, cx: &mut Context<'_, Self, AlanAction>) {
        if self.momentum.is_some() || self.view.borrow().pending_wheel == 0 {
            return;
        }
        self.momentum_idle = 0;
        self.momentum = Some(cx.subscribe_stream(momentum_ticks(), |event, chat, cx| {
            match event {
                SubscriptionEvent::Item(()) => {}
                SubscriptionEvent::Closed => {
                    chat.momentum = None;
                    return;
                }
            }
            if chat.tick() {
                cx.notify();
            }
            if chat.has_pending_wheel() {
                chat.momentum_idle = 0;
            } else {
                chat.momentum_idle += 1;
                if chat.momentum_idle >= MOMENTUM_IDLE_TICKS {
                    chat.momentum = None;
                }
            }
        }));
    }

    pub fn stick_to_bottom(&self) {
        let mut view = self.view.borrow_mut();
        view.follow_output = true;
        view.scroll_target = view.max_scroll;
        view.scroll_offset = view.max_scroll;
        view.pending_wheel = 0;
    }

    /// Whether there is a non-empty selection active.
    pub fn has_active_selection(&self) -> bool {
        self.view
            .borrow()
            .selection
            .as_ref()
            .is_some_and(|s| !s.is_empty())
    }

    /// Clear the active selection (used by Esc in the parent).
    pub fn clear_selection(&mut self) {
        self.view.borrow_mut().selection = None;
    }

    fn push_wheel(&mut self, delta: isize) {
        let mut view = self.view.borrow_mut();
        view.pending_wheel =
            (view.pending_wheel + delta).clamp(-MAX_PENDING_WHEEL, MAX_PENDING_WHEEL);
    }
}

impl View {
    fn scroll_by(&mut self, delta: isize) -> bool {
        let current = if self.follow_output {
            self.max_scroll
        } else {
            self.scroll_target
        };
        let target = if delta.is_negative() {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current.saturating_add(delta as usize).min(self.max_scroll)
        };
        if target == self.scroll_offset && target == self.scroll_target {
            return false;
        }
        self.scroll_target = target;
        self.scroll_offset = self.scroll_target;
        self.follow_output = self.scroll_offset == self.max_scroll;
        true
    }

    /// Synchronize scroll bounds with the current content/viewport size.
    /// Called during render.
    fn sync_scroll(&mut self, content_height: usize, viewport_height: usize) -> usize {
        self.viewport_height = viewport_height;
        self.max_scroll = content_height.saturating_sub(viewport_height);
        if self.follow_output {
            self.scroll_offset = self.max_scroll;
            self.scroll_target = self.max_scroll;
        } else {
            self.scroll_target = self.scroll_target.min(self.max_scroll);
            self.scroll_offset = self.scroll_offset.min(self.max_scroll);
            if self.scroll_offset == self.scroll_target && self.scroll_target == self.max_scroll {
                self.follow_output = true;
            }
        }
        self.scroll_offset
    }

    /// Apply queued wheel notches, capped per flush so one swipe can't fling
    /// across the whole transcript. Returns true if the viewport moved.
    fn flush_wheel(&mut self) -> bool {
        let pending = std::mem::take(&mut self.pending_wheel);
        if pending == 0 {
            return false;
        }
        let cap = self.viewport_height.clamp(1, 12) as isize;
        let (now, rest) = if pending.abs() > cap {
            (pending.signum() * cap, pending - pending.signum() * cap)
        } else {
            (pending, 0)
        };
        if self.scroll_by(now) {
            self.pending_wheel = rest.clamp(-MAX_PENDING_WHEEL, MAX_PENDING_WHEEL);
            true
        } else {
            false
        }
    }

    fn handle_mouse(&mut self, mouse: &MouseEvent, area: &Rect) -> bool {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let now = Instant::now();
                let is_double_click = self.last_click.is_some_and(|(t, c, r)| {
                    c == mouse.column && r == mouse.row && now.duration_since(t).as_millis() <= 500
                });

                if let Some(pos) = self.screen_to_text_pos(mouse.column, mouse.row, area) {
                    let lines = self.layout.lines();
                    if is_double_click && pos.line < lines.len() {
                        let (start_col, end_col) =
                            selection::find_word_bounds_at(&lines[pos.line], pos.col);
                        let sel = Selection::new_word(pos, start_col, end_col);
                        self.selection = Some(sel);
                        self.last_click = None;
                        self.copy_selection();
                    } else {
                        self.selection = Some(Selection::new(pos));
                        self.last_click = Some((now, mouse.column, mouse.row));
                    }
                    true
                } else {
                    false
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let is_dragging = self.selection.as_ref().is_some_and(|s| s.is_dragging);
                if is_dragging {
                    if mouse.row < area.top() {
                        self.scroll_by(-1);
                    } else if mouse.row >= area.bottom() {
                        self.scroll_by(1);
                    }

                    let pos = self.screen_to_text_pos(mouse.column, mouse.row, area);
                    if let (Some(sel), Some(pos)) = (&mut self.selection, pos) {
                        sel.update_cursor(pos, self.layout.lines());
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(sel) = &mut self.selection {
                    sel.is_dragging = false;
                    if sel.is_empty() {
                        self.selection = None;
                    } else {
                        self.copy_selection();
                    }
                    true
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    fn screen_to_text_pos(&self, column: u16, row: u16, area: &Rect) -> Option<TextPosition> {
        let rel_row = row.saturating_sub(area.top()) as usize;
        let line = self.scroll_offset.saturating_add(rel_row);
        let col = column.saturating_sub(area.left()) as usize;
        Some(TextPosition::new(line, col))
    }

    fn copy_selection(&mut self) {
        let Some(sel) = &self.selection else {
            return;
        };
        let text = selection::extract_selected_text(self.layout.lines(), sel);
        if !text.is_empty()
            && let Ok(mut clipboard) = arboard::Clipboard::new()
        {
            let _ = clipboard.set_text(text);
        }
    }
}

impl Component<AlanAction> for ChatHistory {
    fn handle_mouse(
        &mut self,
        mouse: MouseEvent,
        area: Rect,
        cx: &mut Context<'_, Self, AlanAction>,
    ) -> ActionStatus {
        if mouse.kind == MouseEventKind::ScrollUp {
            self.push_wheel(-WHEEL_LINES_PER_NOTCH);
            self.ensure_momentum(cx);
            return ActionStatus::Handled;
        } else if mouse.kind == MouseEventKind::ScrollDown {
            self.push_wheel(WHEEL_LINES_PER_NOTCH);
            self.ensure_momentum(cx);
            return ActionStatus::Handled;
        }
        let changed = self.view.borrow_mut().handle_mouse(&mouse, &area);
        if changed {
            cx.notify();
        }
        ActionStatus::Handled
    }

    fn handle_action(
        &mut self,
        action: &AlanAction,
        cx: &mut Context<'_, Self, AlanAction>,
    ) -> ActionStatus {
        match action {
            AlanAction::Raw(event) => match event {
                // Esc clears an active transcript selection (the editor gets
                // first refusal and pops attachments).
                Event::Key(key)
                    if key.code == KeyCode::Esc
                        && key.kind == KeyEventKind::Press
                        && self.has_active_selection() =>
                {
                    self.clear_selection();
                    cx.notify();
                    ActionStatus::Handled
                }
                _ => ActionStatus::Continue,
            },
            _ => ActionStatus::Continue,
        }
    }

    fn render(&self, frame: &mut Frame, area: Rect, cx: &RenderContext<'_, '_, AlanAction>) {
        let controller = cx.expect_state::<ChatController>();
        let (entries, revision) = (controller.entries(), controller.revision());
        let mut view = self.view.borrow_mut();

        let [content_area, scrollbar_area] =
            Layout::horizontal([Constraint::Min(1), Constraint::Length(1)]).areas(area);
        view.layout
            .sync(entries, revision, content_area.width.max(1));

        let content_height = view.layout.height();
        let viewport_height = usize::from(content_area.height.max(1));
        let scroll = view.sync_scroll(content_height, viewport_height);

        let highlighted_lines = selection::apply_selection_to_lines(
            view.layout.viewport(scroll, viewport_height),
            scroll,
            view.selection.as_ref(),
            theme::SELECTION_BG,
            theme::SELECTION_FG,
        );

        frame.render_widget(Paragraph::new(Text::from(highlighted_lines)), content_area);

        if view.max_scroll > 0 {
            let scrollbar_position = scrollbar_position(scroll, view.max_scroll, content_height);
            let mut scrollbar = ScrollbarState::new(content_height)
                .viewport_content_length(viewport_height)
                .position(scrollbar_position);
            let scrollbar_widget = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(None)
                .thumb_symbol("▌")
                .thumb_style(Style::default().fg(Color::DarkGray));
            frame.render_stateful_widget(scrollbar_widget, scrollbar_area, &mut scrollbar);
        }
    }
}

fn momentum_ticks() -> impl Stream<Item = ()> + Send + 'static {
    futures_util::stream::unfold(true, |first| async move {
        if !first {
            tokio::time::sleep(Duration::from_millis(16)).await;
        }
        Some(((), false))
    })
}
