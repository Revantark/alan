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
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEventKind, MouseEventKind};
use futures_util::Stream;
use ratatui::Frame;
use ratatui::layout::Rect;
use tui::context::Context;
use tui::entity::Entity;
use tui::keymap::{InputContext, KeyMapper};
use tui::{ActionStatus, Component, RenderContext, Subscription, SubscriptionEvent};

use crate::core::{Action, CommandOutcome, CompletionController, Controller};
use crate::login_overlay::{LoginDone, LoginOverlay};
use crate::views::Header;
use crate::views::component::Component as _;
use crate::views::theme;
use crate::views::{
    ChatHistory, ChatSnapshot, Footer, PopupList, PopupStatus, Status, StatusSnapshot, UiState,
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
    Resize,
    MouseScrollUp,
    MouseScrollDown,
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
            Event::Resize(_, _) => Some(AlanAction::Resize),
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => Some(AlanAction::MouseScrollUp),
                MouseEventKind::ScrollDown => Some(AlanAction::MouseScrollDown),
                _ => Some(AlanAction::Raw(event.clone())),
            },
            Event::Paste(data) => Some(AlanAction::Paste(data.clone())),
            event => Some(AlanAction::Raw(event.clone())),
        }
    }
}

/// Owns the whole Alan frontend as one `tui` component, plus the
/// dependencies needed to open feature overlays (today: login).
pub struct AlanRoot {
    inner: Mutex<Inner>,
    providers: Arc<ProviderRegistry>,
    credentials: Arc<dyn CredentialStore>,
    /// Retained so the poll stream keeps running. Dropping it cancels the stream.
    poll: Option<Subscription>,
    header: Option<Entity<Header>>,
    popup: Option<Entity<PopupList>>,
    status: Option<Entity<Status>>,
    chat: Option<Entity<ChatHistory>>,
}

struct Inner {
    controller: Controller,
    ui: UiState,
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
            }),
            providers,
            credentials,
            poll: None,
            header: None,
            popup: None,
            status: None,
            chat: None,
        }
    }

    fn open_login(&mut self, cx: &mut Context<'_, Self, AlanAction>) {
        let overlay = cx.open_overlay(LoginOverlay::new(
            Arc::clone(&self.providers),
            Arc::clone(&self.credentials),
        ));
        cx.subscribe_once::<LoginDone, _, _>(overlay, |done, root, _, cx| match done {
            LoginDone::Succeeded { provider } => {
                root.login_done(provider.clone(), cx);
            }
            LoginDone::Dismissed => {}
        });
    }

    fn login_done(
        &mut self,
        provider: providers::ProviderId,
        cx: &mut Context<'_, Self, AlanAction>,
    ) {
        let mut inner = self.inner.lock().expect("alan root poisoned");
        inner
            .controller
            .push_info(format!("Logged in to {}", provider.0));
        // The transcript revision bumped but `UiState` doesn't know; the
        // 16ms tick stream would catch it within a frame, but notify now.
        cx.notify();
    }

    /// Push the completion snapshot into the popup entity. Plain-data mapping
    /// lives here so `PopupList` never names core types. Skips the push when
    /// the snapshot is unchanged so the 16ms poll tick stays quiet.
    fn push_snapshot(&self, cx: &mut Context<'_, Self, AlanAction>, inner: &mut Inner) {
        let Some(popup) = self.popup else {
            return;
        };
        let (open, status, items, selected) = popup_snapshot(inner.controller.completion());
        let unchanged = cx
            .read(popup, |popup| {
                popup.matches_snapshot(open, &status, &items, selected)
            })
            .unwrap_or(false);
        if unchanged {
            return;
        }
        cx.update(popup, |popup| {
            popup.set(open, status, items, selected);
        });
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

impl Component<AlanAction> for AlanRoot {
    fn init(&mut self, cx: &mut Context<'_, Self, AlanAction>)
    where
        Self: Sized,
    {
        self.header = Some(cx.insert(Header));
        self.popup = Some(cx.insert(PopupList::default()));
        self.status = Some(cx.insert(Status::default()));
        self.chat = Some(cx.insert(ChatHistory::default()));
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
            root.push_snapshot(cx, &mut inner);
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
            AlanAction::Resize => {
                let mut inner = self.inner.lock().expect("alan root poisoned");
                inner.ui.apply(Action::Resize);
                if inner.ui.take_dirty() {
                    cx.notify();
                }
                ActionStatus::Handled
            }
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
                    let Inner { controller, ui, .. } = &mut *inner;
                    let command = ui.handle_event(event.clone(), controller.completion_mut());
                    let is_submit = matches!(command, Some(crate::core::Command::Submit { .. }));
                    let outcome: Option<CommandOutcome> =
                        command.map(|command| inner.controller.handle(command));
                    self.push_snapshot(cx, &mut inner);
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
                let Inner { controller, ui, .. } = &mut *inner;
                let command =
                    ui.handle_event(Event::Paste(text.clone()), controller.completion_mut());
                let outcome: Option<CommandOutcome> =
                    command.map(|command| inner.controller.handle(command));
                self.push_snapshot(cx, &mut inner);
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
        let popup = self.popup;
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
        let Inner { controller, ui } = &mut *inner;

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
        let mut footer = Footer;
        footer.render(frame, footer_area, controller, ui);

        paint_status(status, footer_area, ui.attachment_height(), frame, cx);
        paint_popup(popup, footer_area, frame, cx);
    }
}

/// Snapshot the completion controller into plain popup data. The only place
/// that maps core completion types onto popup types.
fn popup_snapshot(completion: &CompletionController) -> (bool, PopupStatus, Vec<String>, usize) {
    use crate::core::CompletionStatus as CoreStatus;

    let open = completion.is_open();
    let status = match completion.status() {
        CoreStatus::Loading => PopupStatus::Loading,
        CoreStatus::Ready => PopupStatus::Ready,
        CoreStatus::Error(error) => PopupStatus::Error(error),
    };
    let items = completion
        .items(0, completion.item_count())
        .iter()
        .map(|item| item.display.clone())
        .collect();
    (open, status, items, completion.selected())
}

/// Paint the popup above the footer after the body, so it sits visually on
/// top without capturing input. Zero rows means hidden: nothing to anchor.
fn paint_popup(
    popup: Option<Entity<PopupList>>,
    footer: Rect,
    frame: &mut Frame,
    cx: &RenderContext<'_, AlanAction>,
) {
    let Some(popup) = popup else {
        return;
    };

    if let Some(area) = PopupList::area_above(footer, frame.area(), 5) {
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

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyModifiers};

    use super::*;

    /// Map the login path (`UiState::apply`) onto pre-completion keys. Kept while
    /// `AlanAction::Raw` still carries `Event::Key`; each arm is a 1:1 legacy
    /// bridge, not new behavior. Used by the unit tests below; production routes
    /// keys through `UiState::handle_event` instead.
    #[cfg(test)]
    fn action_from_event(event: &Event) -> Option<Action> {
        match event {
            Event::Key(key) => {
                use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};

                if key.kind != KeyEventKind::Press {
                    return None;
                }
                Some(match key.code {
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        Action::Interrupt
                    }
                    KeyCode::Char('v') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        Action::PasteOrAttachImage
                    }
                    KeyCode::Tab | KeyCode::BackTab
                        if key.modifiers.contains(KeyModifiers::SHIFT)
                            || key.code == KeyCode::BackTab =>
                    {
                        Action::TogglePlanMode
                    }
                    KeyCode::Enter => Action::Submit,
                    KeyCode::Esc => Action::ClearInput,
                    KeyCode::Backspace => Action::Backspace,
                    KeyCode::Char(c) => Action::Insert(c),
                    KeyCode::PageUp | KeyCode::Up => Action::ScrollUp,
                    KeyCode::PageDown | KeyCode::Down => Action::ScrollDown,
                    _ => return None,
                })
            }
            Event::Mouse(mouse) => Some(match mouse.kind {
                MouseEventKind::ScrollUp => Action::MouseScrollUp,
                MouseEventKind::ScrollDown => Action::MouseScrollDown,
                _ => return None,
            }),
            Event::Resize(_, _) => Some(Action::Resize),
            Event::Paste(data) => Some(Action::Paste(data.clone())),
            _ => None,
        }
    }

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
    fn mapper_maps_resize_regardless_of_context() {
        let mapper = AlanKeyMapper;
        for context in [
            InputContext::default(),
            InputContext {
                overlay_active: true,
                focus_active: true,
            },
        ] {
            assert!(matches!(
                mapper.map(&Event::Resize(120, 40), &context),
                Some(AlanAction::Resize)
            ));
        }
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

    #[test]
    fn q_is_editor_input_not_quit() {
        let event = Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
        ));
        assert_eq!(action_from_event(&event), Some(Action::Insert('q')));
    }

    #[test]
    fn ctrl_c_interrupts_and_resize_invalidates() {
        let interrupt = Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        ));
        assert_eq!(action_from_event(&interrupt), Some(Action::Interrupt));
        assert_eq!(
            action_from_event(&Event::Resize(120, 40)),
            Some(Action::Resize)
        );
    }

    #[test]
    fn arrow_keys_scroll_or_navigate() {
        let up = Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        ));
        let down = Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        ));
        assert_eq!(action_from_event(&up), Some(Action::ScrollUp));
        assert_eq!(action_from_event(&down), Some(Action::ScrollDown));
    }
}
