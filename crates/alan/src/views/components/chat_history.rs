//! The chat transcript as a self-contained `tui` component.
//!
//! Owns the chat session model ([`ChatController`]), the incremental wrap
//! cache, scroll state, wheel coalescing, and selection. It subscribes to the
//! agent event stream itself and handles submit / mode / quit / mouse / wheel /
//! PageUp / PageDown input, which the root dispatches to it.

use crate::core::{Activity, ChatController, SlashCommand};
use crate::root::{AlanAction, PromptSubmission};
use crate::views::selection;
use crate::views::selection::{Selection, TextPosition};
use crate::views::theme;
use agent::{AgentEvent, AgentStream, Mode};
use crossterm::event::Event;
use crossterm::event::{KeyCode, KeyEventKind, MouseButton, MouseEvent, MouseEventKind};
use futures_util::Stream;
use llm::Usage;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::Text;
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};
use std::cell::RefCell;
use std::time::{Duration, Instant};
use tui::component::{ActionStatus, Component, RenderContext};
use tui::context::Context;
use tui::{Subscription, SubscriptionEvent};

use super::transcript::{TranscriptLayout, scrollbar_position};

/// Lines moved per mouse-wheel notch. Crossterm reports the wheel as discrete
/// notches with no pressure data, so one line per notch keeps a single tick
/// precise; bursts are rate-limited by the flush cap instead.
const WHEEL_LINES_PER_NOTCH: isize = 1;

/// Hard bound on accumulated wheel notches. A trackpad swipe can queue hundreds
/// of events; without a bound the tail keeps scrolling long after the fingers stop.
const MAX_PENDING_WHEEL: isize = 48;

/// Cadence of the self-scheduled momentum ticker. While wheel notches are
/// pending, one flush step is applied per interval; the ticker cancels itself
/// once the queue drains, so no timer runs while the transcript is idle.
const MOMENTUM_TICK_INTERVAL: Duration = Duration::from_millis(16);

/// Idle ticks the momentum ticker stays alive after the queue drains. This
/// keeps one timer running across the natural lulls within a scroll session
/// instead of tearing the worker down and paying a fresh startup on every
/// notch, which showed up as lag.
const MOMENTUM_IDLE_TICKS: u16 = 16;

/// Repaint cadence for streamed agent output. Stream items mutate the model
/// immediately but only this ticker redraws the transcript, so a fast token
/// stream cannot outpace the renderer or starve scroll input. Terminal events
/// (finish, error, disconnect) still repaint at once.
const STREAM_REPAINT_INTERVAL: Duration = Duration::from_millis(32);

/// Cached, plain-data view of the transcript status, rebuilt from the
/// [`ChatController`] by [`ChatHistory::refresh`] whenever the transcript
/// revision or activity changes. The transcript entries themselves are read
/// directly from the controller during render, so they are not cloned here.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatSnapshot {
    pub revision: u64,
    pub activity: Activity,
    pub mode: Mode,
    pub usage: Usage,
    pub model_name: String,
}

/// The chat transcript area: owns the session controller, incremental wrap
/// cache, scroll state, wheel coalescing, and selection.
///
/// Render-mutable state (the layout cache and scroll position) sits behind a
/// `RefCell` because `render` is `&self` by framework contract yet must sync
/// the wrap cache against the width it is given. Input handlers run with
/// `&mut self` and borrow through the same `RefCell`.
pub struct ChatHistory {
    /// The UI-agnostic chat session model. `None` only in tests that exercise
    /// transcript layout/scroll in isolation.
    controller: Option<ChatController>,
    view: RefCell<View>,
    /// Subscription to the in-flight agent stream. Dropping it cancels the run.
    prompt: Option<Subscription>,
    /// Fixed-rate repaint ticker, alive only while the agent is streaming.
    /// Keeps render cadence independent of the token rate.
    stream_repaint: Option<Subscription>,
    /// Set when a `/login` submission needs the root to open the login overlay
    /// (the root owns the providers and credentials). Polled by the root after
    /// it dispatches a submission; the chat cannot dispatch back to its parent
    /// without deadlocking on the parent's locked slot.
    login_requested: bool,
    /// Self-scheduled momentum ticker, alive only while wheel notches are
    /// draining (plus a short idle grace). Dropping it cancels the ticker.
    momentum: Option<Subscription>,
    /// Consecutive ticker ticks with an empty queue; used to end the idle
    /// grace period.
    momentum_idle: u16,
}

impl Default for ChatHistory {
    fn default() -> Self {
        Self::from_controller(None)
    }
}

#[derive(Debug)]
struct View {
    layout: TranscriptLayout,
    snap: Option<ChatSnapshot>,
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
    /// Last rendered content area of the chat (inner, excluding the scrollbar).
    chat_area: Rect,
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
            snap: None,
            scroll_offset: 0,
            scroll_target: 0,
            // The transcript starts pinned to the newest content.
            follow_output: true,
            viewport_height: 0,
            max_scroll: 0,
            chat_area: Rect::default(),
            selection: None,
            pending_wheel: 0,
            last_click: None,
        }
    }
}

impl ChatHistory {
    /// Build a chat component that owns its session controller.
    pub fn new(controller: ChatController) -> Self {
        Self::from_controller(Some(controller))
    }

    fn from_controller(controller: Option<ChatController>) -> Self {
        Self {
            controller,
            view: RefCell::new(View::default()),
            prompt: None,
            stream_repaint: None,
            login_requested: false,
            momentum: None,
            momentum_idle: 0,
        }
    }

    /// Drive the transcript mouse state from a raw mouse event. Returns true if
    /// the viewport or selection changed and a redraw is needed.
    fn handle_mouse_action(&mut self, mouse: &MouseEvent) -> bool {
        self.view.borrow_mut().handle_mouse(mouse)
    }

    /// Refresh the cached snapshot from the controller when the transcript
    /// revision, mode, or activity changed. A cheap no-op otherwise.
    fn refresh(&self) {
        let Some(controller) = &self.controller else {
            return;
        };
        let activity = if controller.is_busy() {
            Activity::Thinking
        } else {
            Activity::Idle
        };
        let revision = controller.revision();
        let mode = controller.mode();
        let mut view = self.view.borrow_mut();
        let unchanged = view
            .snap
            .as_ref()
            .is_some_and(|s| s.revision == revision && s.mode == mode && s.activity == activity);
        if unchanged {
            return;
        }

        let usage = controller.usage();
        let model_name = controller.model_name();
        view.snap = Some(ChatSnapshot {
            revision,
            activity,
            mode,
            usage,
            model_name,
        });
    }

    /// Snapshot of the current transcript status, refreshed from the controller.
    /// The chat container reads this during render to paint the status band it
    /// owns; the transcript entries themselves stay in the controller.
    pub fn snapshot(&self) -> Option<ChatSnapshot> {
        self.refresh();
        self.view.borrow().snap.clone()
    }

    /// Route a submission: slash commands act on the controller (login is
    /// forwarded to the parent), a plain prompt starts the agent stream and
    /// subscribes to it.
    fn handle_submit(
        &mut self,
        submission: PromptSubmission,
        cx: &mut Context<'_, Self, AlanAction>,
    ) {
        let Some(controller) = &mut self.controller else {
            return;
        };

        // Not trimmed: a leading space means this is a prompt.
        if let Some(command) = SlashCommand::parse(&submission.text) {
            match command {
                // The root owns the login overlay; flag it to open on return.
                SlashCommand::Login => self.login_requested = true,
                SlashCommand::Plan => controller.set_mode(agent::Mode::Plan),
                SlashCommand::Review => controller.set_mode(agent::Mode::Review),
                SlashCommand::Normal => controller.set_mode(agent::Mode::Normal),
                SlashCommand::Help => controller.push_info(SlashCommand::help()),
                SlashCommand::New => {
                    self.start_new_session(cx);
                    return;
                }
            }
            return;
        }

        let text = submission.text.trim().to_owned();
        let Some(stream) = controller.submit(text, submission.images) else {
            return;
        };
        self.prompt = Some(
            cx.subscribe_stream(agent_events(stream), |event, chat, cx| {
                match event {
                    SubscriptionEvent::Item(result) => {
                        if let Some(controller) = &mut chat.controller {
                            controller.apply_event(result);
                        }
                    }
                    SubscriptionEvent::Closed => {
                        if let Some(controller) = &mut chat.controller {
                            controller.disconnect_stream();
                        }
                        chat.prompt = None;
                    }
                }
                // Redraw immediately only once the run leaves the busy state
                // (finished, error, or disconnect); while it is still streaming
                // the fixed-rate ticker owns repaints so a fast token stream
                // cannot starve scroll input.
                if chat.controller.as_ref().is_none_or(|c| !c.is_busy()) {
                    chat.stream_repaint = None;
                    cx.notify();
                }
            }),
        );
        self.ensure_stream_repaint(cx);
    }

    /// `/new`: reset to a fresh, empty session.
    fn start_new_session(&mut self, cx: &mut Context<'_, Self, AlanAction>) {
        let agent = match &self.controller {
            Some(controller) if !controller.is_busy() => controller.agent(),
            _ => return,
        };
        cx.spawn(
            async move {
                agent
                    .reset_session()
                    .await
                    .map_err(|error| tui::TaskError(Box::new(error)))
            },
            move |result, chat, cx| {
                if let Some(controller) = &mut chat.controller {
                    match result {
                        Ok(()) => {
                            controller.clear_transcript();
                            controller.push_info("Started a new session.");
                        }
                        Err(error) => {
                            controller.push_info(format!("failed to start new session: {error}"))
                        }
                    }
                }
                cx.notify();
            },
        );
    }

    /// Start the fixed-rate repaint ticker if a run is in flight and it is not
    /// already running. The ticker repaints the transcript at
    /// `STREAM_REPAINT_INTERVAL`; it is cancelled once the run finishes.
    fn ensure_stream_repaint(&mut self, cx: &mut Context<'_, Self, AlanAction>) {
        if self.stream_repaint.is_some() || !self.controller.as_ref().is_some_and(|c| c.is_busy()) {
            return;
        }
        self.stream_repaint = Some(cx.subscribe_stream(
            stream_repaint_ticks(),
            |event, chat, cx| {
                if matches!(event, SubscriptionEvent::Closed) || !chat.is_streaming() {
                    chat.stream_repaint = None;
                    return;
                }
                cx.notify();
            },
        ));
    }

    /// Whether an agent run is currently in flight.
    fn is_streaming(&self) -> bool {
        self.controller.as_ref().is_some_and(|c| c.is_busy())
    }

    /// Whether wheel notches are queued and need a flush this tick.
    pub fn has_pending_wheel(&self) -> bool {
        self.view.borrow().pending_wheel != 0
    }

    /// Take the pending `/login` request, if any. Read by the root after it
    /// dispatches a submission so it can open the login overlay.
    pub(crate) fn take_login_request(&mut self) -> bool {
        std::mem::take(&mut self.login_requested)
    }

    /// Apply one capped step of queued wheel notches, returning whether the
    /// viewport moved. Driven by the self-scheduled momentum ticker
    /// (`ensure_momentum`); bottom-follow itself is synchronized by
    /// `sync_scroll` during render.
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

    // --- Inherent state mutators, shared by `handle_action` and the tests. ---

    fn push_wheel(&mut self, delta: isize) {
        let mut view = self.view.borrow_mut();
        view.pending_wheel =
            (view.pending_wheel + delta).clamp(-MAX_PENDING_WHEEL, MAX_PENDING_WHEEL);
    }

    fn scroll_by(&mut self, delta: isize) -> bool {
        self.view.borrow_mut().scroll_by(delta)
    }

    #[cfg(test)]
    fn flush_wheel(&mut self) -> bool {
        self.view.borrow_mut().flush_wheel()
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

    fn handle_mouse(&mut self, mouse: &MouseEvent) -> bool {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if self.is_mouse_in_chat(mouse.column, mouse.row) {
                    let now = Instant::now();
                    let is_double_click = self.last_click.is_some_and(|(t, c, r)| {
                        c == mouse.column
                            && r == mouse.row
                            && now.duration_since(t).as_millis() <= 500
                    });

                    if let Some(pos) = self.screen_to_text_pos(mouse.column, mouse.row) {
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
                } else if self.selection.is_some() {
                    self.selection = None;
                    true
                } else {
                    false
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let is_dragging = self.selection.as_ref().is_some_and(|s| s.is_dragging);
                if is_dragging {
                    if mouse.row < self.chat_area.top() {
                        self.scroll_by(-1);
                    } else if mouse.row >= self.chat_area.bottom() {
                        self.scroll_by(1);
                    }

                    let pos = self.screen_to_text_pos(mouse.column, mouse.row);
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

    fn is_mouse_in_chat(&self, column: u16, row: u16) -> bool {
        column >= self.chat_area.left()
            && column < self.chat_area.right()
            && row >= self.chat_area.top()
            && row < self.chat_area.bottom()
    }

    fn screen_to_text_pos(&self, column: u16, row: u16) -> Option<TextPosition> {
        let rel_row = row.saturating_sub(self.chat_area.top()) as usize;
        let line = self.scroll_offset.saturating_add(rel_row);
        let col = column.saturating_sub(self.chat_area.left()) as usize;
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
    fn handle_action(
        &mut self,
        action: &AlanAction,
        cx: &mut Context<'_, Self, AlanAction>,
    ) -> ActionStatus {
        match action {
            AlanAction::Submit(submission) => {
                self.handle_submit(submission.clone(), cx);
                cx.notify();
                ActionStatus::Handled
            }
            AlanAction::ToggleMode => {
                if let Some(controller) = &mut self.controller {
                    controller.toggle_mode();
                    cx.notify();
                }
                ActionStatus::Handled
            }
            // Ctrl-C: cancel the in-flight run by dropping its subscription,
            // otherwise quit.
            AlanAction::Quit => {
                if self.controller.as_ref().is_some_and(|c| c.is_busy()) {
                    self.prompt = None;
                    self.stream_repaint = None;
                    if let Some(controller) = &mut self.controller {
                        controller.finish_stream();
                    }
                    cx.notify();
                } else {
                    cx.quit();
                }
                ActionStatus::Handled
            }
            // Wheel notches are coalesced; the self-scheduled momentum ticker
            // drains them a capped step at a time.
            AlanAction::MouseScrollUp => {
                self.push_wheel(-WHEEL_LINES_PER_NOTCH);
                self.ensure_momentum(cx);
                ActionStatus::Handled
            }
            AlanAction::MouseScrollDown => {
                self.push_wheel(WHEEL_LINES_PER_NOTCH);
                self.ensure_momentum(cx);
                ActionStatus::Handled
            }
            AlanAction::Raw(event) => match event {
                Event::Key(key)
                    if key.kind == KeyEventKind::Press
                        && matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) =>
                {
                    let delta = match key.code {
                        KeyCode::PageUp => -(self.view.borrow().viewport_height.max(1) as isize),
                        _ => self.view.borrow().viewport_height.max(1) as isize,
                    };
                    if self.scroll_by(delta) {
                        cx.notify();
                    }
                    ActionStatus::Handled
                }
                Event::Mouse(mouse) => {
                    let changed = self.handle_mouse_action(mouse);
                    if changed {
                        cx.notify();
                    }
                    ActionStatus::Handled
                }
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

    fn render(&self, frame: &mut Frame, area: Rect, _cx: &RenderContext<'_, AlanAction>) {
        self.refresh();
        let Some(controller) = &self.controller else {
            return;
        };
        let mut view = self.view.borrow_mut();
        let Some(snap) = view.snap.clone() else {
            return;
        };

        let [content_area, scrollbar_area] =
            Layout::horizontal([Constraint::Min(1), Constraint::Length(1)]).areas(area);
        view.chat_area = content_area;
        view.layout.sync(
            controller.entries(),
            snap.revision,
            content_area.width.max(1),
        );

        let content_height = view.layout.height();
        let viewport_height = usize::from(content_area.height.max(1));
        let scroll = view.sync_scroll(content_height, viewport_height);

        let viewport_lines = view.layout.viewport(scroll, viewport_height);
        let highlighted_lines = selection::apply_selection_to_lines(
            &viewport_lines,
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

impl ChatHistory {}

/// Emit an item immediately, then one per `MOMENTUM_TICK_INTERVAL`. The
/// immediate first item removes the startup dead zone so the first notch moves
/// on the next frame; the stream is owned by the momentum subscription, so it
/// stops as soon as the subscription is cancelled and dropped.
fn momentum_ticks() -> impl Stream<Item = ()> + Send + 'static {
    futures_util::stream::unfold(true, |first| async move {
        if !first {
            tokio::time::sleep(MOMENTUM_TICK_INTERVAL).await;
        }
        Some(((), false))
    })
}

/// Emit one item per `STREAM_REPAINT_INTERVAL`. Drives fixed-rate repaints
/// while the agent streams; owned by the repaint subscription, so it stops when
/// that subscription is dropped.
fn stream_repaint_ticks() -> impl Stream<Item = ()> + Send + 'static {
    futures_util::stream::unfold((), |_| async {
        tokio::time::sleep(STREAM_REPAINT_INTERVAL).await;
        Some(((), ()))
    })
}

/// Adapt an [`AgentStream`] into a `futures_util::Stream` so it can be driven
/// by `cx.subscribe_stream`. Each item is one display event; the stream ends
/// when the agent task finishes or the subscription is dropped.
fn agent_events(
    stream: AgentStream,
) -> impl Stream<Item = Result<AgentEvent, agent::AgentError>> + Send + 'static {
    futures_util::stream::unfold(stream, |mut stream| async move {
        stream.recv().await.map(|event| (event, stream))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Migrated from `UiState` (scroll / follow / wheel), adapted to the
    // `RefCell<View>` storage. ---

    fn view<'a>(state: &'a ChatHistory) -> std::cell::Ref<'a, View> {
        state.view.borrow()
    }

    fn view_mut<'a>(state: &'a mut ChatHistory) -> std::cell::RefMut<'a, View> {
        state.view.borrow_mut()
    }

    #[test]
    fn follows_bottom_until_user_scrolls_up() {
        let mut state = ChatHistory::default();
        assert_eq!(view_mut(&mut state).sync_scroll(100, 20), 80);

        view_mut(&mut state).scroll_by(-5);
        assert_eq!(view(&state).scroll_target, 75);
        assert_eq!(view(&state).scroll_offset, 75);
        assert!(!view(&state).follow_output);
        state.tick();
        assert_eq!(view(&state).scroll_offset, 75);

        assert_eq!(view_mut(&mut state).sync_scroll(120, 20), 75);
        assert!(!view(&state).follow_output);
    }

    #[test]
    fn scrolling_to_bottom_restores_follow_mode() {
        let mut state = ChatHistory::default();
        view_mut(&mut state).sync_scroll(100, 20);
        view_mut(&mut state).scroll_by(-10);
        view_mut(&mut state).scroll_by(10);

        assert_eq!(view(&state).scroll_target, 80);
        assert!(view(&state).follow_output);
        state.tick();
        assert!(view(&state).follow_output);
        assert_eq!(view_mut(&mut state).sync_scroll(120, 20), 100);
    }

    #[test]
    fn content_shrink_clamps_manual_scroll() {
        let mut state = ChatHistory::default();
        view_mut(&mut state).sync_scroll(100, 20);
        view_mut(&mut state).scroll_by(-10);

        assert_eq!(view_mut(&mut state).sync_scroll(30, 20), 10);
        assert_eq!(view(&state).scroll_target, 10);
        assert!(view(&state).follow_output);
    }

    #[test]
    fn scrolling_updates_rendered_offset_immediately() {
        let mut state = ChatHistory::default();
        view_mut(&mut state).sync_scroll(100, 20);
        view_mut(&mut state).scroll_by(-10);

        assert_eq!(view(&state).scroll_offset, 70);
        assert_eq!(view(&state).scroll_target, 70);
        assert!(!view(&state).follow_output);
    }

    #[test]
    fn scroll_at_bottom_is_a_no_op() {
        let mut state = ChatHistory::default();
        view_mut(&mut state).sync_scroll(100, 20);
        // At the bottom already: scrolling down moves nothing.
        assert!(!view_mut(&mut state).scroll_by(10));
        assert_eq!(view(&state).scroll_offset, 80);
    }

    #[test]
    fn wheel_notches_coalesce_and_cap_per_flush() {
        let mut state = ChatHistory::default();
        view_mut(&mut state).sync_scroll(200, 20);
        view_mut(&mut state).scroll_by(-100);
        assert_eq!(view(&state).scroll_offset, 80);

        // A 100-notch swipe queues without moving the viewport: no redraw per
        // event.
        for _ in 0..100 {
            state.push_wheel(WHEEL_LINES_PER_NOTCH);
        }
        assert_eq!(view(&state).pending_wheel, MAX_PENDING_WHEEL);

        // One flush moves at most a viewport-capped step.
        assert!(state.flush_wheel());
        assert!(view(&state).scroll_offset > 80);
        assert!(view(&state).scroll_offset <= 80 + 12);
    }

    #[test]
    fn clear_selection_clears_active_selection() {
        let mut state = ChatHistory::default();
        let mut sel = Selection::new(TextPosition::new(0, 0));
        sel.cursor = TextPosition::new(0, 3);
        view_mut(&mut state).selection = Some(sel);
        assert!(state.has_active_selection());

        state.clear_selection();
        assert!(!state.has_active_selection());
    }
}
