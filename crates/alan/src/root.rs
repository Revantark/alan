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

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use futures_util::Stream;
use ratatui::Frame;
use ratatui::layout::Rect;
use tui::context::Context;
use tui::entity::Entity;
use tui::keymap::{InputContext, KeyMapper};
use tui::{ActionStatus, Component, RenderContext, Subscription, SubscriptionEvent};

use crate::core::Poll;
use crate::core::SlashCommand;
use crate::core::{Controller, ImageAttachment};
use crate::login_overlay::LoginOverlay;
use crate::views::Header;
use crate::views::theme;
use crate::views::{ChatHistory, ChatSnapshot, PromptEditor};

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
    Submit(PromptSubmission),
    Quit,
    /// Terminal was resized; components re-measure on the next frame.
    Resize,
    Raw(Event),
}

/// A prompt ready to be dispatched to the agent.
#[derive(Debug, Clone)]
pub struct PromptSubmission {
    pub images: Vec<ImageAttachment>,
    pub text: String,
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
            Event::Resize(..) => Some(AlanAction::Resize),
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
            Event::Key(key)
                if key.code == KeyCode::Char('c')
                    && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                Some(AlanAction::Quit)
            }
            event => Some(AlanAction::Raw(event.clone())),
        }
    }
}

/// Owns the whole Alan frontend as one `tui` component, plus the
/// dependencies needed to open feature overlays (today: login).
pub struct AlanRoot {
    controller: Mutex<Controller>,
    providers: Arc<ProviderRegistry>,
    credentials: Arc<dyn CredentialStore>,
    /// Retained so the poll stream keeps running. Dropping it cancels the stream.
    poll: Option<Subscription>,
    header: Option<Entity<Header>>,
    chat: Option<Entity<ChatHistory>>,
    editor: Option<Entity<PromptEditor>>,
}

impl AlanRoot {
    pub fn new(
        controller: Controller,
        providers: Arc<ProviderRegistry>,
        credentials: Arc<dyn CredentialStore>,
    ) -> Self {
        Self {
            controller: Mutex::new(controller),
            providers,
            credentials,
            poll: None,
            header: None,
            chat: None,
            editor: None,
        }
    }

    fn open_login(&mut self, cx: &mut Context<'_, Self, AlanAction>) {
        cx.open_overlay(LoginOverlay::new(
            Arc::clone(&self.providers),
            Arc::clone(&self.credentials),
        ));
    }

    /// Push the transcript snapshot (and its pinned status line) into the
    /// `ChatHistory` entity. Plain-data mapping lives here so `ChatHistory`
    /// never names a core type. Status (activity, mode, cost, model) can change
    /// between chat revisions, so it is pushed every tick; the transcript is
    /// skipped when its revision is unchanged so the 16ms poll tick stays
    /// quiet.
    fn push_chat(&self, cx: &mut Context<'_, Self, AlanAction>, controller: &mut Controller) {
        let Some(chat) = self.chat else {
            return;
        };

        let revision = controller.chat_revision();
        let activity = controller.activity();
        let unchanged = cx
            .read(chat, |chat| {
                chat.matches_revision(revision) && chat.matches_activity(activity)
            })
            .unwrap_or(false);
        if unchanged {
            return;
        }

        let snap = ChatSnapshot {
            entries: controller.chat().to_vec(),
            revision,
            activity,
            mode: controller.mode(),
            usage: controller.usage(),
            model_name: controller.model_name(),
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
        self.chat = Some(cx.insert(ChatHistory::default()));
        self.editor = Some(cx.insert(PromptEditor::new()));
        cx.focus_entity(self.editor.expect("editor entity"));
        // The root stays the input target; it routes mouse / wheel / page
        // actions to the transcript and keyboard / paste to the editor.
        // Seed the transcript (and its pinned status line) so the first frame isn't
        // blank before the first poll tick; later ticks skip them while
        // unchanged.
        {
            let mut controller = self.controller.lock().expect("alan root poisoned");
            self.push_chat(cx, &mut controller);
        }
        self.poll = Some(cx.subscribe_stream(poll_ticks(), |event, root, cx| {
            let SubscriptionEvent::Item(()) = event else {
                return;
            };
            let mut controller = root.controller.lock().expect("alan root poisoned");
            let poll = controller.poll();

            let chat = root.chat;

            root.push_chat(cx, &mut controller);
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
            if poll == Poll::Changed {
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
            AlanAction::Quit => {
                let mut controller = self.controller.lock().expect("alan root poisoned");
                if !controller.abort() {
                    cx.quit();
                };
                drop(controller);
                ActionStatus::Handled
            }
            // A resize invalidates cached layouts; components re-measure on
            // the next render pass. The dimensions are informational here —
            // the framework already re-renders on the next frame.
            AlanAction::Resize => {
                cx.notify();
                ActionStatus::Handled
            }
            AlanAction::ToggleMode => {
                let mut controller = self.controller.lock().expect("alan root poisoned");
                controller.toggle_mode();
                drop(controller);
                cx.notify();
                ActionStatus::Handled
            }
            AlanAction::Submit(submission) => {
                let mut controller = self.controller.lock().expect("alan root poisoned");
                let command = controller.submit(submission.text.clone(), submission.images.clone());
                drop(controller);
                if let Some(c) = command
                    && c == SlashCommand::Login
                {
                    self.open_login(cx);
                }
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
                    // if let Some(chat) = self.chat {
                    //     cx.update(chat, |c| c.cancel_wheel());
                    // }

                    // A submitted prompt resumes bottom-following.
                    // if is_submit && let Some(chat) = self.chat {
                    //     cx.update(chat, |c| c.resume_follow());
                    // }
                    // if let Some(outcome) = outcome {
                    //     if outcome.quit {
                    //         cx.quit();
                    //     }
                    //     drop(inner);
                    //     if outcome.open_login {
                    //         self.open_login(cx);
                    //     }
                    // }
                    ActionStatus::Handled
                }
            },
            _ => ActionStatus::Continue,
        }
    }

    fn render(&self, frame: &mut Frame, area: Rect, cx: &RenderContext<'_, AlanAction>) {
        let chat = self.chat;

        // Body area is everything below the header row (if present).
        let body_area = if let Some(header) = self.header {
            let [header_area, body] =
                Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area);
            cx.render_entity(header, frame, header_area);
            body
        } else {
            area
        };

        // The footer is sized from the wrapped editor rows plus attachments,
        // measured from the editor entity itself so it always reflects the
        // current buffer. `ChatHistory` owns the pinned status line, which
        // occupies the last row of the chat area, directly above the footer.
        let editor = self.editor.expect("editor entity");
        let editor_width = body_area.width.saturating_sub(theme::PROMPT_GUTTER);
        let editor_rows = cx.read(editor, |e| e.rows(editor_width)).unwrap_or(1);
        let attachment_height = cx.read(editor, |e| e.attachment_height()).unwrap_or(0);
        let [chat_area, footer_area] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(2 + editor_rows + attachment_height),
        ])
        .areas(body_area);

        if let Some(chat) = chat {
            cx.render_entity(chat, frame, chat_area);
        }
        cx.render_entity(editor, frame, footer_area);
        // paint_popup(popup, footer_area, frame, cx);
        // paint_popup_v2(self.popup_v2, footer_area, frame, cx);
    }
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
