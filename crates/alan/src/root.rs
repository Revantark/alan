//! Single-root `tui` adapter for Alan.
//!
//! The chat session (controller + agent stream) lives in [`ChatView`], the
//! prompt editor owns input, and this root is a thin layout/router: it routes
//! input to the focused child or dispatches it to the chat component, and
//! composes the header, transcript, and footer. It also owns the providers and
//! credentials needed to open the login overlay.

use providers::{CredentialStore, ProviderRegistry};
use ratatui::layout::Constraint;
use ratatui::layout::Layout;
use std::sync::Arc;

use alan_tui::context::Context;
use alan_tui::entity::Entity;
use alan_tui::keymap::{InputContext, KeyMapper};
use alan_tui::{ActionStatus, Component, RenderContext, Subscription};
use crossterm::event::{Event, KeyCode, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;

use crate::core::ImageAttachment;
use crate::core::chat::ChatController;
use crate::core::permissions::Answer;
use crate::core::permissions::PermissionHandler;
use crate::core::permissions::ToolPolicy;
use crate::login_overlay::LoginOverlay;
use crate::views::{ChatView, Header, LoginRequested};

/// Semantic input for the Alan frontend.
///
/// Context-free inputs (resize, mouse wheel, bracketed paste) are semantic
/// variants decoded in [`AlanKeyMapper`]; everything else stays a 1:1
/// [`AlanAction::Raw`] wrapper until a later slice moves it over.
#[derive(Debug, Clone)]
pub enum AlanAction {
    ToggleMode,
    Paste(String),
    Submit(PromptSubmission),
    Quit,
    /// Terminal was resized; components re-measure on the next frame.
    Resize,
    /// TODO: SHOULD be moved from here
    SetLoadingDots(bool),
    SetSteering(Option<String>),
    CancelSteer,
    /// A pending permission request was answered; ChatView restores focus.
    PermissionAnswered(Answer),
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
    /// Tool-permission request handler routed to the chat view.
    permission_handler: PermissionHandler,
    /// Tool-permission policy, shared with the permission manager.
    policy: ToolPolicy,
    /// The chat controller to install on `init`; taken when inserted.
    chat_source: Option<ChatController>,
    header: Option<Entity<Header>>,
    view: Option<Entity<ChatView>>,
    /// Subscription that opens the login overlay when the chat requests it.
    /// Kept alive so the request is never missed.
    login_subscription: Option<Subscription>,
}

impl AlanRoot {
    pub fn new(
        chat: ChatController,
        providers: Arc<ProviderRegistry>,
        credentials: Arc<dyn CredentialStore>,
        permission_handler: PermissionHandler,
        policy: ToolPolicy,
    ) -> Self {
        Self {
            providers,
            credentials,
            permission_handler,
            policy,
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
            Arc::clone(&self.providers),
            self.permission_handler.clone(),
            self.policy.clone(),
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

    fn render(&self, frame: &mut Frame, area: Rect, cx: &RenderContext<'_, '_, AlanAction>) {
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
