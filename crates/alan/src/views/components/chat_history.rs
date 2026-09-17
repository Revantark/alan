//! The chat transcript as a self-contained `tui` component.
//!
//! Owns the chat session model ([`ChatController`]), the incremental wrap
//! cache, scroll state, wheel coalescing, and selection. It subscribes to the
//! agent event stream itself and handles submit / mode / quit / mouse / wheel /
//! PageUp / PageDown input, which the root dispatches to it.

use crate::core::settings::{self, Settings, SettingsStore};
use crate::core::{Activity, ChatController, Entry, SlashCommand};
use crate::root::{AlanAction, PromptSubmission};
use crate::views::selection;
use crate::views::selection::{Selection, TextPosition};
use crate::views::theme;
use agent::{Agent, AgentEvent, AgentStream, Mode};
use crossterm::event::Event;
use crossterm::event::{KeyCode, KeyEventKind, MouseButton, MouseEvent, MouseEventKind};
use futures_util::Stream;
use llm::{ReasoningEffort, Usage};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::Text;
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};
use std::cell::RefCell;
use std::sync::Arc;
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

/// Cadence of the dot animation shown in the status line while a blocking
/// operation (e.g. `/summarize-new`) is in flight. One tick per interval; the
/// dot count advances by one and wraps at 4 (0 → 1 → 2 → 3 → 0 → …).
const LOADING_DOT_INTERVAL: Duration = Duration::from_millis(350);

/// Cached, plain-data view of the transcript status, rebuilt from the
/// [`ChatController`] by [`ChatHistory::refresh`] whenever the transcript
/// revision or activity changes. The transcript entries themselves are read
/// directly from the controller during render, so they are not cloned here.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatSnapshot {
    pub revision: u64,
    pub activity: Activity,
    /// Current dot count (0..3) for the loading animation. Only meaningful
    /// while `activity` is `Activity::Loading`.
    pub loading_dots: usize,
    pub mode: Mode,
    pub usage: Usage,
    pub model_name: String,
    pub max_context: Option<u64>,
    pub reasoning_effort: llm::ReasoningEffort,
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
    controller: ChatController,
    view: RefCell<View>,
    /// Subscription to the in-flight agent stream. Dropping it cancels the run.
    prompt: Option<Subscription>,
    /// Fixed-rate repaint ticker, alive only while the agent is streaming.
    /// Keeps render cadence independent of the token rate.
    stream_repaint: Option<Subscription>,
    /// Fixed-rate ticker driving the loading-dot animation, alive only while a
    /// blocking operation is in flight. Dropping it cancels the animation.
    loading_repaint: Option<Subscription>,
    /// Set when a `/login` submission needs the root to open the login overlay
    /// (the root owns the providers and credentials). Polled by the root after
    /// it dispatches a submission; the chat cannot dispatch back to its parent
    /// without deadlocking on the parent's locked slot.
    login_requested: bool,
    models_requested: bool,
    /// Set when a `/fork` submission needs the parent to open the fork overlay.
    fork_requested: bool,
    /// Self-scheduled momentum ticker, alive only while wheel notches are
    /// draining (plus a short idle grace). Dropping it cancels the ticker.
    momentum: Option<Subscription>,
    /// Consecutive ticker ticks with an empty queue; used to end the idle
    /// grace period.
    momentum_idle: u16,
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
    /// Current dot count (0..3) driving the loading animation. Advanced by the
    /// loading-repaint ticker while a blocking operation is in flight.
    loading_dots: usize,
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
            loading_dots: 0,
        }
    }
}

impl ChatHistory {
    /// Build a chat component that owns its session controller.
    pub fn new(controller: ChatController) -> Self {
        Self::from_controller(controller)
    }

    fn from_controller(controller: ChatController) -> Self {
        Self {
            controller,
            view: RefCell::new(View::default()),
            prompt: None,
            stream_repaint: None,
            loading_repaint: None,
            login_requested: false,
            models_requested: false,
            fork_requested: false,
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
        let controller = &self.controller;
        let activity = if controller.is_busy() {
            Activity::Thinking
        } else if let Some(label) = controller.loading() {
            Activity::Loading(label.to_owned())
        } else {
            Activity::Idle
        };
        let revision = controller.revision();
        let mode = controller.mode();
        let mut view = self.view.borrow_mut();
        let loading_dots = view.loading_dots;
        let unchanged = view.snap.as_ref().is_some_and(|s| {
            s.revision == revision
                && s.mode == mode
                && s.activity == activity
                && s.loading_dots == loading_dots
                && s.reasoning_effort == controller.reasoning_effort()
        });
        if unchanged {
            return;
        }

        let usage = controller.usage();
        let model_name = controller.model_name();
        view.snap = Some(ChatSnapshot {
            revision,
            activity,
            loading_dots,
            mode,
            usage,
            model_name,
            max_context: controller.max_context(),
            reasoning_effort: controller.reasoning_effort(),
        });
    }

    /// Snapshot of the current transcript status, refreshed from the controller.
    /// The chat container reads this during render to paint the status band it
    /// owns; the transcript entries themselves stay in the controller.
    pub fn snapshot(&self) -> Option<ChatSnapshot> {
        self.refresh();
        self.view.borrow().snap.clone()
    }

    pub fn entries(&self) -> &[Entry] {
        self.controller.entries()
    }

    /// Route a submission: slash commands act on the controller (login is
    /// forwarded to the parent), a plain prompt starts the agent stream and
    /// subscribes to it.
    fn handle_submit(
        &mut self,
        submission: PromptSubmission,
        cx: &mut Context<'_, Self, AlanAction>,
    ) {
        let controller = &mut self.controller;

        // Not trimmed: a leading space means this is a prompt.
        if let Some(command) = SlashCommand::parse(&submission.text) {
            match command {
                // The root owns the login overlay; flag it to open on return.
                SlashCommand::Login => self.login_requested = true,
                SlashCommand::Models => self.models_requested = true,
                SlashCommand::Fork => self.request_fork(&submission.text),
                SlashCommand::Plan => controller.set_mode(agent::Mode::Plan),
                SlashCommand::Review => controller.set_mode(agent::Mode::Review),
                SlashCommand::Normal => controller.set_mode(agent::Mode::Normal),
                SlashCommand::Effort => self.apply_effort(&submission.text, cx),

                SlashCommand::Help => controller.push_info(SlashCommand::help()),
                SlashCommand::New => self.start_new_session(cx),
                SlashCommand::SummarizeNew => self.start_summarize_new(cx, &submission.text),
                SlashCommand::ModelProviders => self.apply_model_provider(cx, &submission.text),
            }
            return;
        }

        let text = submission.text.trim().to_owned();
        let Some(stream) = controller.submit(text, submission.images) else {
            return;
        };
        self.view.borrow_mut().stick_to_bottom();
        self.prompt = Some(
            cx.subscribe_stream(agent_events(stream), |event, chat, cx| {
                match event {
                    SubscriptionEvent::Item(result) => {
                        chat.controller.apply_event(result);
                    }
                    SubscriptionEvent::Closed => {
                        chat.controller.disconnect_stream();

                        chat.prompt = None;
                    }
                }
                // Redraw immediately only once the run leaves the busy state
                // (finished, error, or disconnect); while it is still streaming
                // the fixed-rate ticker owns repaints so a fast token stream
                // cannot starve scroll input.
                if !chat.controller.is_busy() {
                    chat.stream_repaint = None;
                    cx.notify();
                }
            }),
        );
        self.ensure_stream_repaint(cx);
    }

    /// `/new`: reset to a fresh, empty session.
    fn start_new_session(&mut self, cx: &mut Context<'_, Self, AlanAction>) {
        if self.controller.is_busy() {
            return;
        }
        let agent = self.controller.agent();
        cx.spawn(
            async move {
                agent
                    .reset_session()
                    .await
                    .map_err(|error| tui::TaskError(Box::new(error)))
            },
            move |result, chat, cx| {
                match result {
                    Ok(()) => {
                        chat.controller.clear_transcript();
                        chat.controller.push_info("Started a new session.");
                    }
                    Err(error) => chat
                        .controller
                        .push_info(format!("failed to start new session: {error}")),
                }

                cx.notify();
            },
        );
    }

    /// `/fork`: open the fork overlay. Busy streams are rejected outright; the
    /// overlay itself is owned by the parent (see `take_fork_request`).
    fn request_fork(&mut self, text: &str) {
        if self.controller.is_busy() {
            self.controller
                .push_info("cannot fork while a response is streaming".to_owned());
            return;
        }
        // `/fork` takes no arguments; anything after the command is a usage
        // error rather than a silently ignored prompt.
        if let Some((_, args)) = SlashCommand::parse_with_args(text)
            && !args.trim().is_empty()
        {
            self.controller.push_info("usage: /fork".to_owned());
            return;
        }
        self.fork_requested = true;
    }

    fn apply_model_provider(&mut self, cx: &mut Context<'_, ChatHistory, AlanAction>, text: &str) {
        if self.controller.is_busy() {
            return;
        }
        let agent = self.controller.agent();

        let provider_order = match SlashCommand::parse_with_args(text).map(|(_, args)| args) {
            Some(args) if args.trim().eq_ignore_ascii_case("none") => Ok(Vec::new()),
            Some(args) => parse_provider_order(args),
            None => return,
        };
        let provider_order = match provider_order {
            Ok(order) => order,
            Err(error) => {
                self.controller.push_info(format!(
                    "usage: /providers <provider1,provider2,...>: {error}"
                ));

                return;
            }
        };

        cx.spawn(
            async move {
                let model_id = agent.info().await.id;
                agent
                    .set_provider_order(provider_order.clone())
                    .await
                    .map_err(|error| tui::TaskError(Box::new(error)))?;

                persist_provider_order(&model_id, &provider_order)
                    .await
                    .map_err(|error| tui::TaskError(error.into()))?;

                Ok::<_, tui::TaskError>(if provider_order.is_empty() {
                    "provider order cleared (using default)".to_owned()
                } else {
                    format!("provider order set to {}", provider_order.join(", "))
                })
            },
            move |result, chat, cx| {
                let message = match result {
                    Ok(msg) => msg,
                    Err(error) => {
                        chat.controller
                            .push_info(format!("failed to set provider order: {error}"));
                        cx.notify();
                        return;
                    }
                };
                chat.controller.push_info(message);

                cx.notify();
            },
        );
    }

    /// `/summarize-new [focus]`: summarize, then restart seeded with it.
    fn start_summarize_new(&mut self, cx: &mut Context<'_, Self, AlanAction>, text: &str) {
        if self.controller.is_busy() {
            return;
        }
        let agent = self.controller.agent();
        let focus = SlashCommand::parse_with_args(text)
            .map(|(_, args)| args.trim().to_owned())
            .filter(|args| !args.is_empty());

        // Set the loading state before spawning so the status line shows the
        // label immediately; the dot ticker animates it until completion.
        self.view.borrow_mut().loading_dots = 0;
        self.controller.set_loading(Some("summarizing".to_owned()));

        self.ensure_loading_repaint(cx);

        cx.spawn(
            async move {
                let summary = agent
                    .summarize(focus.as_deref())
                    .await
                    .map_err(|error| tui::TaskError(Box::new(error)))?;
                let seed = vec![agent::AgentMessage::user(format!(
                    "Session handoff — continue from this state:\n\n{summary}"
                ))];
                agent
                    .reset_session_with(seed)
                    .await
                    .map_err(|error| tui::TaskError(Box::new(error)))?;
                Ok::<(), tui::TaskError>(())
            },
            move |result, chat, cx| {
                // Stop the dot animation and clear the loading state regardless
                // of outcome; the completion closure runs exactly once.
                chat.loading_repaint = None;
                chat.view.borrow_mut().loading_dots = 0;
                let controller = &mut chat.controller;
                controller.set_loading(None);
                controller.clear_transcript();
                match result {
                    Ok(()) => controller.push_info("Summarized into a new session."),
                    Err(error) => {
                        controller.push_info(format!("failed to summarize session: {error}"))
                    }
                }
                cx.notify();
            },
        );
    }

    /// `/effort [none|minimal|low|medium|high|xhigh|max]`: set the reasoning
    /// effort on the bound model and remember it for the next session. With no
    /// argument, or an unknown one, show usage help instead of failing
    /// silently.
    fn apply_effort(&mut self, text: &str, cx: &mut Context<'_, Self, AlanAction>) {
        if self.controller.is_busy() {
            return;
        }
        let agent = self.controller.agent();

        let effort = match SlashCommand::parse_with_args(text).map(|(_, args)| args) {
            Some(args) => match SlashCommand::parse_effort(args) {
                Some(effort) => effort,
                None => {
                    self.controller.push_info(
                        "usage: /effort <none|minimal|low|medium|high|xhigh|max>".to_owned(),
                    );
                    return;
                }
            },
            None => return,
        };

        let agent = agent.clone();
        cx.spawn(
            async move {
                agent
                    .set_reasoning_effort(effort)
                    .await
                    .map_err(|error| tui::TaskError(Box::new(error)))?;

                persist_reasoning_effort(effort)
                    .await
                    .map_err(|error| tui::TaskError(error.into()))?;

                Ok::<_, tui::TaskError>(format!("reasoning effort set to {effort}"))
            },
            move |result, chat, cx| {
                let message = match result {
                    Ok(msg) => msg,
                    Err(error) => {
                        chat.controller
                            .push_info(format!("failed to set reasoning effort: {error}"));

                        cx.notify();
                        return;
                    }
                };
                chat.controller.set_reasoning_effort(effort);
                chat.controller.push_info(message);

                cx.notify();
            },
        );
    }

    /// Start the fixed-rate repaint ticker if a run is in flight and it is not
    /// already running. The ticker repaints the transcript at
    /// `STREAM_REPAINT_INTERVAL`; it is cancelled once the run finishes.
    fn ensure_stream_repaint(&mut self, cx: &mut Context<'_, Self, AlanAction>) {
        if self.stream_repaint.is_some() || !self.controller.is_busy() {
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
        self.controller.is_busy()
    }

    /// Start the loading-dot ticker if a blocking operation is in flight and
    /// the ticker is not already running. The ticker advances the dot count at
    /// `LOADING_DOT_INTERVAL` and repaints; it is cancelled once loading ends.
    fn ensure_loading_repaint(&mut self, cx: &mut Context<'_, Self, AlanAction>) {
        if self.loading_repaint.is_some() || !self.is_loading() {
            return;
        }
        self.loading_repaint = Some(cx.subscribe_stream(loading_ticks(), |event, chat, cx| {
            if matches!(event, SubscriptionEvent::Closed) || !chat.is_loading() {
                chat.loading_repaint = None;
                return;
            }
            let next = (chat.view.borrow().loading_dots + 1) % 4;
            chat.view.borrow_mut().loading_dots = next;
            cx.notify();
        }));
    }

    /// Whether a blocking operation is currently in flight.
    pub fn is_loading(&self) -> bool {
        self.controller.loading().is_some()
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

    /// Take the pending `/models` request, if any. Read by the root after it
    /// dispatches a submission so it can open the model picker overlay.
    pub(crate) fn take_models_request(&mut self) -> bool {
        std::mem::take(&mut self.models_requested)
    }

    /// Take the pending `/fork` request, if any. Read by the parent after it
    /// dispatches a submission so it can open the fork overlay.
    pub(crate) fn take_fork_request(&mut self) -> bool {
        std::mem::take(&mut self.fork_requested)
    }

    pub(crate) fn agent(&self) -> Arc<Agent> {
        self.controller.agent()
    }

    pub(crate) fn model_name(&self) -> String {
        self.controller.model_name()
    }

    pub(crate) fn apply_model_switch(&mut self, name: String) {
        self.controller.apply_model_switch(name);
    }

    pub(crate) fn set_max_context(&mut self, max_context: Option<u64>) {
        self.controller.set_max_context(max_context);
    }

    pub(crate) fn set_reasoning_effort(&mut self, reasoning_effort: llm::ReasoningEffort) {
        self.controller.set_reasoning_effort(reasoning_effort);
    }

    pub(crate) fn apply_model_switch_failed(&mut self, error: String) {
        self.controller.apply_model_switch_failed(error);
    }

    /// Rebuild the visible transcript from a message snapshot. Used by the
    /// fork completion path, which cannot `await` inside an update closure.
    pub(crate) fn apply_restored(
        &mut self,
        messages: Vec<agent::AgentMessage>,
        usage: llm::Usage,
        model_name: String,
        max_context: Option<u64>,
    ) {
        self.controller
            .apply_restored(messages, usage, model_name, max_context);
    }

    pub(crate) fn push_info(&mut self, text: impl Into<String>) {
        self.controller.push_info(text);
    }

    pub(crate) fn max_context(&self) -> Option<u64> {
        self.controller.max_context()
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

    fn stick_to_bottom(&mut self) {
        self.follow_output = true;
        self.scroll_target = self.max_scroll;
        self.scroll_offset = self.max_scroll;
        self.pending_wheel = 0;
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
                self.controller.toggle_mode();
                cx.notify();

                ActionStatus::Handled
            }
            // Ctrl-C: cancel the in-flight run by dropping its subscription,
            // otherwise quit.
            AlanAction::Quit => {
                if self.controller.is_busy() {
                    self.prompt = None;
                    self.stream_repaint = None;
                    self.controller.finish_stream();
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
        let controller = &self.controller;
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

/// Emit one item per `LOADING_DOT_INTERVAL`. Drives the status-line dot
/// animation while a blocking operation is in flight; owned by the loading
/// subscription, so it stops when that subscription is dropped.
fn loading_ticks() -> impl Stream<Item = ()> + Send + 'static {
    futures_util::stream::unfold((), |_| async {
        tokio::time::sleep(LOADING_DOT_INTERVAL).await;
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

/// Parse a comma-separated provider list into a non-empty `Vec<String>`,
/// mirroring `parse_provider_order` in `main.rs`.
fn parse_provider_order(value: &str) -> anyhow::Result<Vec<String>> {
    let order = value
        .split(',')
        .map(|entry| entry.trim().to_string())
        .filter(|entry| !entry.is_empty())
        .collect::<Vec<_>>();
    if order.is_empty() {
        return Err(anyhow::anyhow!("provider order must not be empty"));
    }
    Ok(order)
}

/// Write the reasoning effort to `settings.json`. Mirrors `persist_model`
/// in `chat_view.rs`: load, patch, save. Returns the error so the caller can
/// report it in the transcript; the save is best-effort and never blocks the
/// agent run.
async fn persist_reasoning_effort(effort: ReasoningEffort) -> anyhow::Result<()> {
    let store = SettingsStore::<Settings>::new(settings::default_settings_path()?);
    let mut settings = store.load().await?.unwrap_or_default();
    settings.reasoning = Some(effort);
    store.save(&settings).await
}

async fn persist_provider_order(model: &str, provider_order: &[String]) -> anyhow::Result<()> {
    let store = SettingsStore::<Settings>::new(settings::default_settings_path()?);
    let mut settings = store.load().await?.unwrap_or_default();
    if provider_order.is_empty() {
        settings.provider_orders.remove(model);
    } else {
        settings
            .provider_orders
            .insert(model.to_owned(), provider_order.to_vec());
    }
    store.save(&settings).await
}
