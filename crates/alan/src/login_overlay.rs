//! Login overlay owning the provider authentication flow.
use std::sync::Arc;

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::Stream;
use futures_util::stream::unfold;
use providers::{
    AuthEvent, AuthInteraction, AuthPrompt, CredentialStore, InteractionError, ProviderId,
    ProviderRegistry,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;
use tokio::sync::{mpsc, oneshot};
use tui::context::Context;
use tui::{ActionStatus, Component, RenderContext, Subscription, SubscriptionEvent, TaskHandle};

use crate::root::AlanAction;
use crate::views::theme;

#[derive(Debug, Clone, PartialEq, Eq)]
struct LoginProvider {
    pub id: ProviderId,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LoginState {
    Selecting {
        providers: Vec<LoginProvider>,
        selected: usize,
    },
    Prompting {
        provider: ProviderId,
        prompt: AuthPrompt,
    },
    Validating {
        provider: ProviderId,
        message: String,
    },
    Success {
        provider: ProviderId,
    },
    Error(String),
}

/// Terminal occurrence the parent subscribes to at open time.
#[derive(Debug, Clone)]
pub enum LoginDone {
    Succeeded { provider: ProviderId },
    Dismissed,
}

/// Answer channel for one prompt request: the overlay sends the user's input
/// (or cancellation) back to the blocked `prompt()` call in the auth task.
type PromptResponder = oneshot::Sender<Result<String, InteractionError>>;

/// Intermediate auth messages sent from the background auth task to the UI
/// thread over an `mpsc` channel (see [`ChannelAuthInteraction`]).
///
/// - `Prompt`: background asks the user for input and blocks on `responder`.
///   The overlay stores `responder` as `pending_prompt` and shows the prompt;
///   submitting the draft sends the answer back through the `oneshot`.
/// - `Event`: one-way status update rendered as `Validating`.
enum LoginStreamMsg {
    Prompt {
        provider: ProviderId,
        prompt: AuthPrompt,
        responder: PromptResponder,
    },
    Event {
        provider: ProviderId,
        event: AuthEvent,
    },
}

/// Successful auth task outcome delivered via `cx.spawn`.
struct LoginSuccess {
    provider: ProviderId,
}

#[derive(Debug)]
struct LoginFailed(String);

impl std::fmt::Display for LoginFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for LoginFailed {}

pub struct LoginOverlay {
    providers: Arc<ProviderRegistry>,
    credentials: Arc<dyn CredentialStore>,
    state: LoginState,
    draft: String,
    login_task: Option<TaskHandle>,
    interaction_subscription: Option<Subscription>,
    pending_prompt: Option<PromptResponder>,
}

impl LoginOverlay {
    pub fn new(providers: Arc<ProviderRegistry>, credentials: Arc<dyn CredentialStore>) -> Self {
        let list = providers
            .providers()
            .iter()
            .map(|provider| LoginProvider {
                id: provider.id().clone(),
                name: provider.id().0.clone(),
            })
            .collect();
        Self {
            providers,
            credentials,
            state: LoginState::Selecting {
                providers: list,
                selected: 0,
            },
            draft: String::new(),
            login_task: None,
            interaction_subscription: None,
            pending_prompt: None,
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let LoginState::Selecting {
            providers,
            selected,
        } = &mut self.state
        else {
            return;
        };
        if providers.is_empty() {
            return;
        }
        let max = providers.len().saturating_sub(1) as isize;
        *selected = (*selected as isize + delta).clamp(0, max) as usize;
    }

    /// Applies one intermediate message from the background auth task.
    /// `Prompt` shows the input UI and stores the answer channel;
    /// `Event` renders a status line.
    fn apply_interaction_message(&mut self, message: LoginStreamMsg) {
        match message {
            LoginStreamMsg::Prompt {
                provider,
                prompt,
                responder,
            } => {
                self.pending_prompt = Some(responder);
                self.draft.clear();
                self.state = LoginState::Prompting { provider, prompt };
            }
            LoginStreamMsg::Event { provider, event } => {
                self.state = LoginState::Validating {
                    provider,
                    message: auth_event_message(event),
                };
            }
        }
    }

    /// Starts the background login for `provider_id`:
    /// 1. subscribe the mpsc receiver so intermediate `Prompt`/`Event`
    ///    messages from the auth task update the overlay, then
    /// 2. spawn the one-shot task that runs `Provider::login()` and stores
    ///    the credential (`LoginSuccess`/`LoginFailed` on completion).
    fn start(&mut self, provider_id: ProviderId, cx: &mut Context<'_, Self, AlanAction>) {
        let Some(provider) = self.providers.get(&provider_id) else {
            self.state = LoginState::Error(format!("Unknown provider: {}", provider_id.0));
            cx.notify();
            return;
        };
        let (interaction_tx, interaction_rx) = mpsc::unbounded_channel();
        self.interaction_subscription = Some(subscribe_interaction_messages(cx, interaction_rx));
        self.login_task = Some(cx.spawn(
            login_task_future(
                Arc::clone(&provider),
                Arc::clone(&self.credentials),
                provider_id.clone(),
                interaction_tx,
            ),
            |result, overlay, cx| {
                overlay.login_task = None;
                match result {
                    Ok(success) => {
                        overlay.state = LoginState::Success {
                            provider: success.provider.clone(),
                        };
                        cx.emit(LoginDone::Succeeded {
                            provider: success.provider,
                        });
                    }
                    Err(error) => {
                        overlay.state = LoginState::Error(error.to_string());
                    }
                }
                cx.notify();
            },
        ));
        self.state = LoginState::Validating {
            provider: provider_id,
            message: "Starting login".into(),
        };
        cx.notify();
    }

    fn submit_draft(&mut self) {
        if !matches!(self.state, LoginState::Prompting { .. }) {
            return;
        }
        if let Some(response) = self.pending_prompt.take() {
            let _ = response.send(Ok(std::mem::take(&mut self.draft)));
        }
    }

    /// Abort background work. Idempotent; called from explicit cancel paths
    /// and from `cleanup` when the entity is removed.
    fn cancel(&mut self) {
        if let Some(responder) = self.pending_prompt.take() {
            let _ = responder.send(Err(InteractionError::Cancelled));
        }
        if let Some(task) = self.login_task.take() {
            task.cancel();
        }
        // Dropping the subscription stops the message pump.
        self.interaction_subscription.take();
    }

    fn dismiss(&mut self, cx: &mut Context<'_, Self, AlanAction>, done: LoginDone) {
        self.cancel();
        cx.emit(done);
        cx.close_overlay();
    }

    fn on_enter(&mut self, cx: &mut Context<'_, Self, AlanAction>) {
        match &self.state {
            LoginState::Selecting {
                providers,
                selected,
            } => {
                let Some(provider) = providers.get(*selected).map(|p| p.id.clone()) else {
                    self.state = LoginState::Error("No providers available".into());
                    cx.notify();
                    return;
                };
                self.start(provider, cx);
            }
            LoginState::Prompting { .. } => self.submit_draft(),
            LoginState::Validating { .. } => {}
            LoginState::Success { .. } | LoginState::Error(_) => {
                self.dismiss(cx, LoginDone::Dismissed);
            }
        }
    }
}

impl Component<AlanAction> for LoginOverlay {
    fn handle_action(
        &mut self,
        action: &AlanAction,
        cx: &mut Context<'_, Self, AlanAction>,
    ) -> ActionStatus
    where
        Self: Sized,
    {
        match action {
            AlanAction::MouseScrollUp => {
                self.move_selection(-1);
                cx.notify();
                ActionStatus::Handled
            }
            AlanAction::MouseScrollDown => {
                self.move_selection(1);
                cx.notify();
                ActionStatus::Handled
            }
            AlanAction::Paste(text) => {
                if matches!(self.state, LoginState::Prompting { .. }) {
                    self.draft.push_str(text);
                    cx.notify();
                }
                ActionStatus::Handled
            }
            AlanAction::Raw(event) => {
                let Event::Key(key) = event else {
                    return ActionStatus::Handled;
                };
                if key.kind != KeyEventKind::Press {
                    return ActionStatus::Handled;
                }
                match key.code {
                    KeyCode::Up | KeyCode::PageUp => {
                        self.move_selection(-1);
                        cx.notify();
                    }
                    KeyCode::Down | KeyCode::PageDown => {
                        self.move_selection(1);
                        cx.notify();
                    }
                    KeyCode::Enter => self.on_enter(cx),
                    KeyCode::Esc => self.dismiss(cx, LoginDone::Dismissed),
                    KeyCode::Backspace => {
                        if matches!(self.state, LoginState::Prompting { .. }) {
                            self.draft.pop();
                            cx.notify();
                        }
                    }
                    KeyCode::Char(character)
                        if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
                    {
                        if matches!(self.state, LoginState::Prompting { .. }) {
                            self.draft.push(character);
                            cx.notify();
                        }
                    }
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.dismiss(cx, LoginDone::Dismissed);
                    }
                    _ => {}
                }
                ActionStatus::Handled
            }
            AlanAction::Resize => ActionStatus::Continue,
        }
    }

    fn cleanup(&mut self, _cx: &mut Context<'_, Self, AlanAction>)
    where
        Self: Sized,
    {
        self.cancel();
    }

    fn render(&self, frame: &mut Frame, area: Rect, _cx: &RenderContext<'_, AlanAction>) {
        let area = centered_rect(70, 60, area);
        frame.render_widget(ratatui::widgets::Clear, area);
        frame.render_widget(
            Paragraph::new("").style(Style::default().bg(theme::EDITOR_BG)),
            area,
        );

        let [content_area, shortcuts_area] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area.inner(
                Margin {
                    horizontal: 2,
                    vertical: 1,
                },
            ));

        match &self.state {
            LoginState::Selecting {
                providers,
                selected,
            } => {
                let items = providers
                    .iter()
                    .enumerate()
                    .map(|(index, provider)| {
                        let marker = if index == *selected { "› " } else { "  " };
                        Line::from(vec![
                            Span::styled(marker, Style::default().fg(theme::PROMPT_FG)),
                            Span::styled(
                                provider.name.clone(),
                                Style::default().fg(theme::EDITOR_FG),
                            ),
                        ])
                    })
                    .collect::<Vec<_>>();
                frame.render_widget(Paragraph::new(Text::from(items)), content_area);
                draw_shortcuts(
                    frame,
                    shortcuts_area,
                    "↑↓ select · Enter confirm · Esc cancel",
                );
            }
            LoginState::Prompting { prompt, .. } => {
                let secret = matches!(prompt, AuthPrompt::Secret { .. });
                let value = if secret {
                    "•".repeat(self.draft.chars().count())
                } else {
                    self.draft.clone()
                };
                let prompt_line = Line::from(prompt_message(prompt));
                let input_line = Line::from(vec![
                    Span::styled("› ", Style::default().fg(theme::PROMPT_FG)),
                    Span::styled(value.clone(), Style::default().fg(theme::EDITOR_FG)),
                ]);
                let content = Text::from(vec![prompt_line, Line::default(), input_line]);
                frame.render_widget(Paragraph::new(content), content_area);
                draw_shortcuts(frame, shortcuts_area, "Enter submit · Esc cancel");

                let input_width = Line::from(value.as_str()).width() as u16;
                let cursor_x = content_area
                    .x
                    .saturating_add(2)
                    .saturating_add(input_width)
                    .min(content_area.right().saturating_sub(1));
                frame.set_cursor_position((cursor_x, content_area.y + 2));
            }
            LoginState::Validating { message, .. } => {
                frame.render_widget(Paragraph::new(message.as_str()), content_area);
                draw_shortcuts(frame, shortcuts_area, "Esc cancel");
            }
            LoginState::Success { provider } => {
                frame.render_widget(
                    Paragraph::new(format!("Logged in to {}", provider.0)),
                    content_area,
                );
                draw_shortcuts(frame, shortcuts_area, "Esc close");
            }
            LoginState::Error(message) => {
                frame.render_widget(
                    Paragraph::new(message.as_str())
                        .style(Style::default().fg(ratatui::style::Color::Red)),
                    content_area,
                );
                draw_shortcuts(frame, shortcuts_area, "Esc close");
            }
        }
    }
}

// ── Background-auth bridge ────────────────────────────────────────────────
// The provider's `login()` runs on a background task and cannot touch UI
// state. `ChannelAuthInteraction` adapts `AuthInteraction` to message
// passing: `prompt()`/`notify()` send `LoginStreamMsg` over `mpsc`, and the
// overlay pumps them back on the UI thread via `subscribe_stream`. Prompt
// answers travel the other way through a per-prompt `oneshot`.

/// Runs `Provider::login()` then stores the credential; the single terminal
/// result is delivered by `cx.spawn`.
async fn login_task_future(
    provider: Arc<dyn providers::Provider>,
    credentials: Arc<dyn CredentialStore>,
    provider_id: ProviderId,
    interaction_tx: mpsc::UnboundedSender<LoginStreamMsg>,
) -> Result<LoginSuccess, tui::TaskError> {
    let mut interaction = ChannelAuthInteraction::new(provider_id.clone(), interaction_tx);
    let credential = provider
        .auth()
        .login(&mut interaction)
        .await
        .map_err(|error| tui::TaskError(Box::new(LoginFailed(error.to_string()))))?;
    credentials
        .put(&provider_id, credential.clone())
        .await
        .map_err(|error| tui::TaskError(Box::new(LoginFailed(error.to_string()))))?;
    Ok(LoginSuccess {
        provider: provider_id,
    })
}

/// Pumps intermediate `Prompt`/`Event` messages from the auth task into
/// `apply_interaction_message` on the UI thread.
fn subscribe_interaction_messages(
    cx: &mut Context<'_, LoginOverlay, AlanAction>,
    rx: mpsc::UnboundedReceiver<LoginStreamMsg>,
) -> Subscription {
    cx.subscribe_stream(
        interaction_messages_from_channel(rx),
        |event, overlay, cx| {
            match event {
                SubscriptionEvent::Item(message) => overlay.apply_interaction_message(message),
                SubscriptionEvent::Closed => {
                    overlay.interaction_subscription = None;
                }
            }
            cx.notify();
        },
    )
}

fn interaction_messages_from_channel(
    rx: mpsc::UnboundedReceiver<LoginStreamMsg>,
) -> impl Stream<Item = LoginStreamMsg> + Send + 'static {
    unfold(rx, |mut rx| async move {
        rx.recv().await.map(|message| (message, rx))
    })
}

fn auth_event_message(event: AuthEvent) -> String {
    match event {
        AuthEvent::Info(message) | AuthEvent::Progress(message) => message,
        AuthEvent::AuthUrl { url, instructions } => instructions
            .map(|text| format!("{text}: {url}"))
            .unwrap_or(url),
    }
}

struct ChannelAuthInteraction {
    provider: ProviderId,
    sender: mpsc::UnboundedSender<LoginStreamMsg>,
}

impl ChannelAuthInteraction {
    fn new(provider: ProviderId, sender: mpsc::UnboundedSender<LoginStreamMsg>) -> Self {
        Self { provider, sender }
    }
}

#[async_trait::async_trait]
impl AuthInteraction for ChannelAuthInteraction {
    async fn prompt(&mut self, prompt: AuthPrompt) -> Result<String, InteractionError> {
        let (sender, receiver) = oneshot::channel();
        self.sender
            .send(LoginStreamMsg::Prompt {
                provider: self.provider.clone(),
                prompt,
                responder: sender,
            })
            .map_err(|_| InteractionError::Cancelled)?;
        receiver.await.map_err(|_| InteractionError::Cancelled)?
    }

    fn notify(&mut self, event: AuthEvent) {
        let _ = self.sender.send(LoginStreamMsg::Event {
            provider: self.provider.clone(),
            event,
        });
    }
}

fn draw_shortcuts(frame: &mut Frame, area: Rect, text: &str) {
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            text,
            Style::default().fg(theme::MUTED_FG),
        ))),
        area,
    );
}

fn prompt_message(prompt: &AuthPrompt) -> String {
    match prompt {
        AuthPrompt::Secret { message }
        | AuthPrompt::Text { message }
        | AuthPrompt::ManualCode { message } => message.clone(),
        AuthPrompt::Select { message, .. } => message.clone(),
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let horizontal: [Rect; 3] = Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .areas(area);
    let vertical: [Rect; 3] = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .areas(horizontal[1]);
    vertical[1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use providers::ProviderId;

    fn overlay_with(ids: &[&str]) -> LoginOverlay {
        let mut overlay = LoginOverlay::new(
            Arc::new(ProviderRegistry::default()),
            Arc::new(providers::InMemoryCredentialStore::new()),
        );
        overlay.state = LoginState::Selecting {
            providers: ids
                .iter()
                .map(|id| LoginProvider {
                    id: ProviderId::new(*id),
                    name: (*id).to_owned(),
                })
                .collect(),
            selected: 0,
        };
        overlay
    }

    #[test]
    fn selection_clamps_at_the_ends() {
        let mut overlay = overlay_with(&["a", "b"]);
        overlay.move_selection(-5);
        assert!(matches!(
            overlay.state,
            LoginState::Selecting { selected: 0, .. }
        ));
        overlay.move_selection(99);
        assert!(matches!(
            overlay.state,
            LoginState::Selecting { selected: 1, .. }
        ));
    }

    #[test]
    fn selection_ignores_empty_provider_lists() {
        let mut overlay = overlay_with(&[]);
        overlay.move_selection(1);
        assert!(matches!(
            overlay.state,
            LoginState::Selecting { selected: 0, .. }
        ));
    }

    #[test]
    fn prompt_arrival_clears_a_stale_draft() {
        let mut overlay = overlay_with(&["a"]);
        overlay.draft.push_str("stale");
        let (responder, _) = oneshot::channel();
        overlay.apply_interaction_message(LoginStreamMsg::Prompt {
            provider: ProviderId::new("a"),
            prompt: AuthPrompt::Text {
                message: "key".into(),
            },
            responder,
        });
        assert!(overlay.draft.is_empty());
        assert!(matches!(overlay.state, LoginState::Prompting { .. }));
    }

    #[test]
    fn event_arrival_shows_validating_status() {
        let mut overlay = overlay_with(&["a"]);
        overlay.apply_interaction_message(LoginStreamMsg::Event {
            provider: ProviderId::new("a"),
            event: AuthEvent::Progress("Validating".into()),
        });
        assert!(matches!(overlay.state, LoginState::Validating { .. }));
    }

    #[test]
    fn auth_url_without_instructions_falls_back_to_the_url() {
        let message = auth_event_message(AuthEvent::AuthUrl {
            url: "https://example.test".into(),
            instructions: None,
        });
        assert_eq!(message, "https://example.test");
    }
}
