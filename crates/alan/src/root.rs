//! Single-root `tui` adapter for Alan.
//!
//! The chat session (controller + agent stream) lives in [`ChatHistory`], the
//! prompt editor owns input, and this root is a thin layout/router: it routes
//! input to the focused child or dispatches it to the chat component, and
//! composes the header, transcript, and footer. It also owns the providers and
//! credentials needed to open the login overlay.

use providers::{CredentialStore, ProviderRegistry};
use ratatui::layout::Constraint;
use ratatui::layout::Layout;
use std::sync::Arc;

use crossterm::event::{Event, KeyCode, KeyModifiers, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use tui::context::Context;
use tui::entity::Entity;
use tui::keymap::{InputContext, KeyMapper};
use tui::{ActionStatus, Component, RenderContext, Subscription};

use crate::core::ImageAttachment;
use crate::login_overlay::LoginOverlay;
use crate::views::{ChatHistory, ChatView, Header, LoginRequested};

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
    providers: Arc<ProviderRegistry>,
    credentials: Arc<dyn CredentialStore>,
    provider: Arc<dyn providers::Provider>,
    /// The chat component to install on `init`; taken when inserted.
    chat_source: Option<ChatHistory>,
    header: Option<Entity<Header>>,
    view: Option<Entity<ChatView>>,
    /// Subscription that opens the login overlay when the chat requests it.
    /// Kept alive so the request is never missed.
    login_subscription: Option<Subscription>,
}

impl AlanRoot {
    pub fn new(
        chat: ChatHistory,
        providers: Arc<ProviderRegistry>,
        credentials: Arc<dyn CredentialStore>,
    ) -> Self {
        let provider = providers
            .providers()
            .first()
            .cloned()
            .expect("at least one provider");
        Self {
            providers,
            credentials,
            provider,
            chat_source: Some(chat),
            header: None,
            view: None,
            login_subscription: None,
        }
    }

    fn open_login(&mut self, cx: &mut Context<'_, Self, AlanAction>) {
        cx.open_overlay(LoginOverlay::new(
            Arc::clone(&self.providers),
            Arc::clone(&self.credentials),
        ));
    }
}

impl Component<AlanAction> for AlanRoot {
    fn init(&mut self, cx: &mut Context<'_, Self, AlanAction>)
    where
        Self: Sized,
    {
        self.header = Some(cx.insert(Header));
        let view = cx.insert(ChatView::new(
            self.chat_source
                .take()
                .expect("chat component installed once"),
            Arc::clone(&self.provider),
        ));
        self.view = Some(view);
        self.login_subscription = Some(
            cx.subscribe::<LoginRequested, ChatView, _>(view, |_event, root, _view, cx| {
                root.open_login(cx)
            }),
        );
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
            // A resize invalidates cached layouts; components re-measure on
            // the next render pass. The framework already re-renders on the
            // next frame, so this just asks for one.
            AlanAction::Resize => {
                cx.notify();
                ActionStatus::Handled
            }
            // All other actions are handled by the chat column (transcript,
            // status, editor) or by the focused editor before they reach here.
            _ => ActionStatus::Continue,
        }
    }

    fn render(&self, frame: &mut Frame, area: Rect, cx: &RenderContext<'_, AlanAction>) {
        // Body area is everything below the header row (if present).
        let body_area = if let Some(header) = self.header {
            let [header_area, body] =
                Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area);
            cx.render_entity(header, frame, header_area);
            body
        } else {
            area
        };

        if let Some(view) = self.view {
            cx.render_entity(view, frame, body_area);
        }
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
}
