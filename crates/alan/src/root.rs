//! Single-root `tui` adapter for Alan.
//!
//! [`Controller`] owns application state, [`UiState`] owns the prompt editor,
//! and [`ChatHistory`] owns the transcript (scroll, wheel, selection). The root
//! orchestrates: each 16ms tick it pushes plain-data snapshots down to the
//! child entities and routes input (mouse/wheel to the transcript, keys to the
//! editor). `render` composes the children into the body layout.
//!
//! `Controller` is not `Sync` (it holds `JoinHandle`s and plain state), so the
//! root keeps it behind a `Mutex`. `render` is `&self` by framework contract.

use providers::{CredentialStore, ProviderRegistry};
use ratatui::layout::Constraint;
use ratatui::layout::Layout;
use std::io;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use strum::IntoEnumIterator;

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use futures_util::Stream;
use ratatui::Frame;
use ratatui::layout::Rect;
use tui::context::Context;
use tui::entity::Entity;
use tui::keymap::{InputContext, KeyMapper};
use tui::{ActionStatus, Component, RenderContext, Subscription, SubscriptionEvent};

use crate::core::{
    CommandCompleterBackend, CommandOutcome, CommandsContext, Completer, CompletionRequest,
    Controller, PathCompleterBackend, PathsContext,
};
use crate::login_overlay::LoginOverlay;
use crate::views::Header;
use crate::views::component::Component as _;
use crate::views::theme;
use crate::views::{
    ChatHistory, ChatSnapshot, PopupListv2, PopupSelected, PromptEditor, Status, StatusSnapshot,
    UiState,
};

/// How often streamed agent output is collected while the app is idle.
const TICK_INTERVAL: Duration = Duration::from_millis(16);

/// Item type of the poll ticker. The value is unused; each item means "poll now".
type PollTick = ();

/// Semantic input for the Alan frontend.
///
/// Context-free inputs (resize, mouse wheel, bracketed paste) are semantic
/// variants decoded in [`AlanKeyMapper`]; everything else stays a 1:1
/// [`AlanAction::Raw`] wrapper until a later slice moves it over.
#[derive(Debug, Clone)]
pub enum AlanAction {
    MouseScrollUp,
    MouseScrollDown,
    ToggleMode,
    Paste(String),
    Raw(Event),
}

/// Passes terminal events through as [`AlanAction::Raw`], except for the
/// context-free inputs decoded above.
///
/// Owns the `KeyMapper` seam so future refinements happen here, at the
/// runtime boundary, and components never depend on raw crossterm mapping.
#[derive(Debug, Default, Clone, Copy)]
pub struct AlanKeyMapper;

impl KeyMapper<AlanAction> for AlanKeyMapper {
    fn map(&self, event: &Event, _context: &InputContext) -> Option<AlanAction> {
        match event {
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => Some(AlanAction::MouseScrollUp),
                MouseEventKind::ScrollDown => Some(AlanAction::MouseScrollDown),
                _ => Some(AlanAction::Raw(event.clone())),
            },
            Event::Paste(data) => Some(AlanAction::Paste(data.clone())),
            // Shift+Tab toggles the agent mode (plan/review/normal) before the
            // editor sees it, so the popup never consumes it as tab-completion.
            Event::Key(key)
                if key.code == KeyCode::BackTab
                    || (key.code == KeyCode::Tab
                        && key.modifiers.contains(KeyModifiers::SHIFT)) =>
            {
                Some(AlanAction::ToggleMode)
            }

            event => Some(AlanAction::Raw(event.clone())),
        }
    }
}

/// Owns the whole Alan frontend as one `tui` component, plus the
/// dependencies needed to open feature overlays (today: login).
pub struct AlanRoot {
    inner: Mutex<Inner>,
    completer: Completer,
    providers: Arc<ProviderRegistry>,
    credentials: Arc<dyn CredentialStore>,
    /// Retained so the poll stream keeps running. Dropping it cancels the stream.
    poll: Option<Subscription>,
    /// Retained so the popup's accept/dismiss events keep being delivered.
    /// Dropping it cancels the subscription.
    popup_subscription: Option<Subscription>,
    header: Option<Entity<Header>>,
    popup_v2: Option<Entity<PopupListv2>>,
    status: Option<Entity<Status>>,
    chat: Option<Entity<ChatHistory>>,
}

struct Inner {
    controller: Controller,
    ui: UiState,
    /// Trigger of the last request sent to the completer. A change means a
    /// backend switched active, so its data must be refreshed.
    last_trigger: Option<char>,
    /// The request the user dismissed with Esc. Suppressed until the editor
    /// state changes, so the 16ms tick cannot reopen a popup the user closed.
    dismissed: Option<CompletionRequest>,
}

impl AlanRoot {
    pub fn new(
        controller: Controller,
        providers: Arc<ProviderRegistry>,
        credentials: Arc<dyn CredentialStore>,
    ) -> Self {
        Self {
            inner: Mutex::new(Inner {
                controller,
                ui: UiState::new(),
                last_trigger: None,
                dismissed: None,
            }),
            completer: Completer::new()
                .with_backend(
                    Box::new(CommandCompleterBackend),
                    Box::new(CommandsContext {
                        commands: crate::core::SlashCommand::iter().collect(),
                    }),
                )
                .with_backend(
                    Box::new(PathCompleterBackend),
                    Box::new(PathsContext {
                        paths: Vec::new(),
                        status: crate::core::CompletionStatus::Loading,
                    }),
                ),
            providers,
            credentials,
            poll: None,
            header: None,
            popup_v2: None,
            popup_subscription: None,
            status: None,
            chat: None,
        }
    }

    fn open_login(&mut self, cx: &mut Context<'_, Self, AlanAction>) {
        cx.open_overlay(LoginOverlay::new(
            Arc::clone(&self.providers),
            Arc::clone(&self.credentials),
        ));
    }

    /// Push the status-line snapshot into the `Status` entity. Plain-data
    /// mapping lives here so `Status` never names core types. Skips the push
    /// when the snapshot is unchanged so the 16ms poll tick stays quiet.
    fn push_status(&self, cx: &mut Context<'_, Self, AlanAction>, inner: &mut Inner) {
        let Some(status) = self.status else {
            return;
        };
        let snap = StatusSnapshot {
            activity: inner.controller.activity(),
            mode: inner.controller.mode(),
            usage: inner.controller.usage(),
        };
        let unchanged = cx
            .read(status, |status| status.matches(&snap))
            .unwrap_or(false);
        if unchanged {
            return;
        }
        cx.update(status, |status| status.set(snap));
    }

    /// Push the transcript snapshot into the `ChatHistory` entity. Plain-data
    /// mapping lives here so `ChatHistory` never names a core type. Skips the
    /// push when the snapshot is unchanged so the 16ms poll tick stays quiet.
    fn push_chat(&self, cx: &mut Context<'_, Self, AlanAction>, inner: &mut Inner) {
        let Some(chat) = self.chat else {
            return;
        };

        let revision = inner.controller.chat_revision();
        let unchanged = cx
            .read(chat, |chat| chat.matches_revision(revision))
            .unwrap_or(false);
        if unchanged {
            return;
        }

        let snap = ChatSnapshot {
            entries: inner.controller.chat().to_vec(),
            revision,
        };
        cx.update(chat, |chat| chat.set(snap));
    }
}

/// Refresh the v2 completion popup from the current editor state. Called
/// after every editor event and on every tick while a request is open.
///
/// A trigger change (or first request) starts a scan and marks the
/// completer loading; otherwise the cached index is ranked against the
/// pattern. The popup is fed plain data — open/message/items/selected — and
/// never names a core type.
///
/// A free function rather than a method so the caller can pass `&mut Inner`
/// and `&mut completer` as disjoint borrows without holding the root lock.
fn refresh_completion(
    completer: &mut Completer,
    popup: Option<Entity<PopupListv2>>,
    cx: &mut Context<'_, AlanRoot, AlanAction>,
    inner: &mut Inner,
) {
    let Some(popup) = popup else {
        return;
    };
    let Some(request) = inner.ui.completion_request() else {
        // No token under the cursor: close the popup and forget any
        // dismissal, so re-typing the trigger later can reopen it.
        inner.dismissed = None;
        cx.update(popup, |popup| popup.set(false, None, Vec::new()));
        return;
    };

    // The user dismissed this exact request with Esc; keep the popup closed
    // until the editor state changes, so the 16ms tick cannot reopen it.
    if inner.dismissed.as_ref() == Some(&request) {
        cx.update(popup, |popup| popup.set(false, None, Vec::new()));
        return;
    }
    inner.dismissed = None;

    // A trigger change means a backend switched active. The path backend's data
    // is a filesystem scan, so it gets a loading context and a spawned task.
    // The command backend's data is immutable config — nothing to refresh.
    if inner.last_trigger != Some(request.trigger) {
        inner.last_trigger = Some(request.trigger);
        if request.trigger == '@' {
            let ctx = PathsContext {
                paths: Vec::new(),
                status: crate::core::CompletionStatus::Loading,
            };
            completer.set_context('@', Box::new(ctx));
            let root_path =
                std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            let trigger = request.trigger;
            // The handler runs after this function returns, in a deferred
            // non-reentrant batch, and receives `&mut AlanRoot` as a parameter.
            cx.spawn(scan_task(root_path), move |result, root, cx| {
                let paths = match result {
                    Ok(paths) => paths,
                    Err(_) => Vec::new(),
                };
                let ctx = PathsContext {
                    paths,
                    status: crate::core::CompletionStatus::Ready,
                };
                root.completer.set_context(trigger, Box::new(ctx));
                cx.notify();
            });
        }
    }

    let Some(result) = completer.complete(request) else {
        cx.update(popup, |popup| popup.set(false, None, Vec::new()));
        return;
    };

    let message = match &result.status {
        crate::core::CompletionStatus::Loading => Some("Loading…".to_owned()),
        crate::core::CompletionStatus::Ready if result.items.is_empty() => {
            Some("No matches".to_owned())
        }
        crate::core::CompletionStatus::Ready => None,
    };
    let items: Vec<String> = result
        .items
        .iter()
        .map(|item| item.display.clone())
        .collect();
    // Skip the push when the snapshot is unchanged, so the 16ms tick stays
    // quiet and a navigation key isn't clobbered by a reset.
    let unchanged = cx
        .read(popup, |popup| {
            popup.matches(true, message.as_deref(), &items)
        })
        .unwrap_or(false);
    if unchanged {
        return;
    }
    cx.update(popup, |popup| popup.set(true, message, items));
    // Focus the popup so Up/Down/Enter/Tab reach it directly; it returns
    // focus to root on accept/dismiss via the subscription callback.
    cx.focus_entity(popup);
}

impl Component<AlanAction> for AlanRoot {
    fn init(&mut self, cx: &mut Context<'_, Self, AlanAction>)
    where
        Self: Sized,
    {
        self.header = Some(cx.insert(Header));
        self.popup_v2 = Some(cx.insert(PopupListv2::default()));
        self.status = Some(cx.insert(Status::default()));
        self.chat = Some(cx.insert(ChatHistory::default()));
        // The v2 popup owns its own selection and emits on accept/dismiss.
        // Root subscribes (persistent, re-arming) and applies the accepted
        // item by re-deriving the request from the editor — the popup only
        // carries display strings, never a replacement or byte range.
        if let Some(popup) = self.popup_v2 {
            self.popup_subscription = Some(cx.subscribe::<PopupSelected, PopupListv2, _>(
                popup,
                |event, root, popup, cx| {
                    let mut inner = root.inner.lock().expect("alan root poisoned");
                    match event {
                        PopupSelected::Dismiss => {
                            // Remember the dismissed request so the 16ms tick
                            // cannot reopen the popup the user closed.
                            inner.dismissed = inner.ui.completion_request();
                        }
                        PopupSelected::Accept { index } => {
                            let Some(request) = inner.ui.completion_request() else {
                                return;
                            };
                            let Some(result) = root.completer.complete(request) else {
                                return;
                            };
                            let Some(item) = result.items.get(*index) else {
                                return;
                            };
                            inner.ui.insert_completion(&item.replacement, result.range);
                            // The token changed, so a dismissal is no longer
                            // relevant.
                            inner.dismissed = None;
                        }
                    }
                    drop(inner);
                    cx.update(popup, |popup| popup.set(false, None, Vec::new()));
                    cx.focus_entity(cx.entity());
                    cx.notify();
                },
            ));
        }
        // The root stays the input target; it routes mouse / wheel / page
        // actions to the transcript and keyboard / paste to the editor.
        // Seed the status line and transcript so the first frame isn't blank
        // before the first poll tick; later ticks skip them while unchanged.
        {
            let mut inner = self.inner.lock().expect("alan root poisoned");
            self.push_status(cx, &mut inner);
            self.push_chat(cx, &mut inner);
        }
        self.poll = Some(cx.subscribe_stream(poll_ticks(), |event, root, cx| {
            let SubscriptionEvent::Item(()) = event else {
                return;
            };
            let mut inner = root.inner.lock().expect("alan root poisoned");
            let poll = inner.controller.poll();
            inner.ui.on_poll(poll);
            let chat = root.chat;
            refresh_completion(&mut root.completer, root.popup_v2, cx, &mut inner);

            root.push_status(cx, &mut inner);
            root.push_chat(cx, &mut inner);
            // Apply queued wheel notches on the tick.
            if let Some(chat) = chat
                && cx
                    .read(chat, |chat| chat.has_pending_wheel())
                    .unwrap_or(false)
            {
                cx.update(chat, |chat| {
                    chat.tick();
                });
            }
            if inner.ui.take_dirty() {
                cx.notify();
            }
        }));
    }

    fn handle_action(
        &mut self,
        action: &AlanAction,
        cx: &mut Context<'_, Self, AlanAction>,
    ) -> ActionStatus
    where
        Self: Sized,
    {
        match action {
            // Wheel and mouse traffic is owned by `ChatHistory`; the root just
            // routes it. `ChatHistory` hit-tests its own rect and ignores
            // misses, so no parent-side geometry check is needed.
            AlanAction::MouseScrollUp | AlanAction::MouseScrollDown => {
                let chat = self.chat;
                if let Some(chat) = chat {
                    cx.dispatch(chat, action);
                }
                ActionStatus::Handled
            }
            AlanAction::ToggleMode => {
                let mut inner = self.inner.lock().expect("alan root poisoned");
                inner.controller.toggle_mode();
                drop(inner);
                cx.notify();
                ActionStatus::Handled
            }
            AlanAction::Raw(event) => match event {
                // Mouse traffic is owned by `ChatHistory`; the root just routes
                // it there. `ChatHistory` hit-tests its own rect and ignores
                // misses.
                Event::Mouse(_) => {
                    let chat = self.chat;
                    if let Some(chat) = chat {
                        cx.dispatch(chat, action);
                    }
                    ActionStatus::Handled
                }
                // PageUp/PageDown scroll the transcript, not the editor.
                Event::Key(key)
                    if matches!(
                        key.code,
                        crossterm::event::KeyCode::PageUp | crossterm::event::KeyCode::PageDown
                    ) && key.kind == KeyEventKind::Press =>
                {
                    let chat = self.chat;
                    if let Some(chat) = chat {
                        cx.dispatch(chat, action);
                    }
                    ActionStatus::Handled
                }
                // Esc clears an active transcript selection before it reaches
                // the editor (which pops attachments). Selection state is owned
                // by `ChatHistory`, so the parent only arbitrates priority.
                Event::Key(key)
                    if key.code == KeyCode::Esc
                        && key.kind == KeyEventKind::Press
                        && cx
                            .read(self.chat.expect("chat entity"), |c| {
                                c.has_active_selection()
                            })
                            .unwrap_or(false) =>
                {
                    cx.update(self.chat.expect("chat entity"), |c| {
                        c.clear_selection();
                    });
                    ActionStatus::Handled
                }
                // Everything else is editor input.
                _ => {
                    // Typing cancels queued wheel momentum, as before.
                    if let Some(chat) = self.chat {
                        cx.update(chat, |c| c.cancel_wheel());
                    }
                    let mut inner = self.inner.lock().expect("alan root poisoned");
                    let Inner { ui, .. } = &mut *inner;
                    let command = ui.handle_event(event.clone());
                    let is_submit = matches!(command, Some(crate::core::Command::Submit { .. }));
                    let outcome: Option<CommandOutcome> =
                        command.map(|command| inner.controller.handle(command));
                    refresh_completion(&mut self.completer, self.popup_v2, cx, &mut inner);

                    if inner.ui.take_dirty() {
                        cx.notify();
                    }
                    // A submitted prompt resumes bottom-following.
                    if is_submit && let Some(chat) = self.chat {
                        cx.update(chat, |c| c.resume_follow());
                    }
                    if let Some(outcome) = outcome {
                        if outcome.quit {
                            cx.quit();
                        }
                        drop(inner);
                        if outcome.open_login {
                            self.open_login(cx);
                        }
                    }
                    ActionStatus::Handled
                }
            },
            AlanAction::Paste(text) => {
                let mut inner = self.inner.lock().expect("alan root poisoned");
                let Inner { ui, .. } = &mut *inner;
                let command = ui.handle_event(Event::Paste(text.clone()));
                let outcome: Option<CommandOutcome> =
                    command.map(|command| inner.controller.handle(command));
                refresh_completion(&mut self.completer, self.popup_v2, cx, &mut inner);
                if inner.ui.take_dirty() {
                    cx.notify();
                }
                if let Some(outcome) = outcome {
                    if outcome.quit {
                        cx.quit();
                    }
                    drop(inner);
                    if outcome.open_login {
                        self.open_login(cx);
                    }
                }
                ActionStatus::Handled
            }
        }
    }

    fn render(&self, frame: &mut Frame, area: Rect, cx: &RenderContext<'_, AlanAction>) {
        let chat = self.chat;
        let status = self.status;

        // Body area is everything below the header row (if present).
        let body_area = if let Some(header) = self.header {
            let [header_area, body] =
                Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area);
            cx.render_entity(header, frame, header_area);
            body
        } else {
            area
        };

        let mut inner = self.inner.lock().expect("alan root poisoned");
        let Inner { controller, ui, .. } = &mut *inner;

        // Same split the old `AppView` used: transcript takes the remainder,
        // footer is sized from the wrapped editor rows plus attachments.
        let editor_width = body_area.width.saturating_sub(theme::PROMPT_GUTTER);
        let [chat_area, footer_area] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(4 + ui.editor_rows(editor_width) + ui.attachment_height()),
        ])
        .areas(body_area);

        if let Some(chat) = chat {
            cx.render_entity(chat, frame, chat_area);
        }
        let mut prompt_editor = PromptEditor;
        prompt_editor.render(frame, footer_area, controller, ui);

        paint_status(status, footer_area, ui.attachment_height(), frame, cx);
        // paint_popup(popup, footer_area, frame, cx);
        paint_popup_v2(self.popup_v2, footer_area, frame, cx);
    }
}

/// Paint the v2 popup above the footer. It owns its own selection and input,
/// so root only renders it — it never mutates it here.
fn paint_popup_v2(
    popup: Option<Entity<PopupListv2>>,
    footer: Rect,
    frame: &mut Frame,
    cx: &RenderContext<'_, AlanAction>,
) {
    let Some(popup) = popup else {
        return;
    };

    if let Some(area) = PopupListv2::area_above(footer, frame.area(), 5) {
        cx.render_entity(popup, frame, area);
    }
}

/// Paint the status line over the footer's reserved status row, which sits one
/// row below the attachment area (`area.y + attachment_height + 1`).
fn paint_status(
    status: Option<Entity<Status>>,
    footer: Rect,
    attachment_height: u16,
    frame: &mut Frame,
    cx: &RenderContext<'_, AlanAction>,
) {
    let Some(status) = status else {
        return;
    };
    let status_area = Rect {
        y: footer.y + attachment_height + 1,
        height: 1,
        ..footer
    };
    cx.render_entity(status, frame, status_area);
}

fn poll_ticks() -> impl Stream<Item = PollTick> + Send + 'static {
    futures_util::stream::unfold((), |state| async move {
        tokio::time::sleep(TICK_INTERVAL).await;
        Some(((), state))
    })
}

/// A spawned scan: walks the workspace and returns its relative paths.
/// Runs on the blocking thread pool, so it never blocks the UI loop.
fn scan_task(
    root: std::path::PathBuf,
) -> impl Future<Output = Result<Vec<String>, tui::TaskError>> + Send + 'static {
    async move {
        let result =
            tokio::task::spawn_blocking(move || crate::core::completion::scan::scan_dir(&root))
                .await
                .unwrap_or_else(|_| Err(io::Error::new(io::ErrorKind::Other, "scan panicked")));
        result.map_err(|error| tui::TaskError(Box::new(error)))
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyModifiers};

    use super::*;

    #[test]
    fn mapper_passes_other_events_through_unchanged() {
        let mapper = AlanKeyMapper;
        let event = Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        ));
        let mapped = mapper.map(&event, &InputContext::default());
        assert!(matches!(mapped, Some(AlanAction::Raw(actual)) if actual == event));
    }

    #[test]
    fn mapper_maps_mouse_wheel_regardless_of_context() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

        let mapper = AlanKeyMapper;
        for context in [
            InputContext::default(),
            InputContext {
                overlay_active: true,
                focus_active: true,
            },
        ] {
            let up = Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            });
            assert!(matches!(
                mapper.map(&up, &context),
                Some(AlanAction::MouseScrollUp)
            ));
            let down = Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            });
            assert!(matches!(
                mapper.map(&down, &context),
                Some(AlanAction::MouseScrollDown)
            ));
            // Clicks still need chat-area geometry, so they stay raw.
            let click = Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            });
            assert!(matches!(
                mapper.map(&click, &context),
                Some(AlanAction::Raw(_))
            ));
        }
    }

    #[test]
    fn mapper_maps_paste_regardless_of_context() {
        let mapper = AlanKeyMapper;
        for context in [
            InputContext::default(),
            InputContext {
                overlay_active: true,
                focus_active: true,
            },
        ] {
            let event = Event::Paste("hello\nworld".into());
            assert!(matches!(
                mapper.map(&event, &context),
                Some(AlanAction::Paste(actual)) if actual == "hello\nworld"
            ));
        }
    }
}
