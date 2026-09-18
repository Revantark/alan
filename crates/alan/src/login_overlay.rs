//! Login overlay owning the provider authentication flow.
use crate::root::AlanAction;
use crate::views::theme;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use providers::{AuthMethod, AuthResult, CredentialStore, ProviderId, ProviderRegistry};
use ratatui::Frame;
use ratatui::layout::Alignment;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Wrap};
use std::sync::Arc;
use tui::context::Context;
use tui::{ActionStatus, Component, RenderContext};

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
        provider_id: ProviderId,
        auth_method: providers::AuthMethod,
        input: String,
    },
    Validating {
        message: String,
    },
    Success,
    Error(String),
}

pub struct LoginOverlay {
    providers: Arc<ProviderRegistry>,
    credentials: Arc<dyn CredentialStore>,
    state: LoginState,
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

    fn submit_input(&mut self, cx: &mut Context<'_, Self, AlanAction>) {
        let LoginState::Prompting {
            provider_id,
            auth_method,
            input,
        } = &mut self.state
        else {
            return;
        };

        let auth_result = match auth_method {
            AuthMethod::ApiKey => providers::AuthResult::ApiKey(std::mem::take(input)),
        };

        let Some(provider) = self.providers.get(provider_id) else {
            self.state = LoginState::Error("Provider not found".into());
            cx.notify();
            return;
        };

        let provider = Arc::clone(&provider);
        let credentials = Arc::clone(&self.credentials);

        cx.spawn(
            async move {
                provider
                    .validate_auth(&auth_result)
                    .await
                    .map_err(|e| tui::TaskError(e.into()))?;

                match auth_result {
                    AuthResult::ApiKey(key) => {
                        let credential = providers::Credential::ApiKey { key };
                        if let Err(e) = credentials.put(&provider.id(), credential).await {
                            return Err(tui::TaskError(e.into()));
                        }
                    }
                }

                Ok(())
            },
            |result, overlay, cx| {
                match result {
                    Ok(_) => {
                        overlay.state = LoginState::Success;
                    }
                    Err(error) => {
                        overlay.state = LoginState::Error(error.to_string());
                    }
                }
                cx.notify();
            },
        );

        self.state = LoginState::Validating {
            message: "Validating API key".into(),
        };
        cx.notify();
    }

    fn dismiss(&mut self, cx: &mut Context<'_, Self, AlanAction>) {
        cx.close_overlay();
    }

    fn on_enter(&mut self, cx: &mut Context<'_, Self, AlanAction>) {
        match &self.state {
            LoginState::Selecting {
                providers,
                selected,
            } => {
                let Some(provider_id) = providers.get(*selected).map(|p| p.id.clone()) else {
                    self.state = LoginState::Error("No providers available".into());
                    cx.notify();
                    return;
                };

                let Some(provider) = self.providers.get(&provider_id) else {
                    self.state = LoginState::Error("Provider not found".into());
                    cx.notify();
                    return;
                };

                let auth_method = provider.auth_methods().into_iter().next();

                if auth_method.is_none() {
                    self.state = LoginState::Error("No authentication methods available".into());
                    cx.notify();
                    return;
                }

                self.state = LoginState::Prompting {
                    provider_id,
                    auth_method: auth_method.unwrap(),
                    input: String::new(),
                };
                cx.notify();
            }
            LoginState::Prompting { .. } => self.submit_input(cx),
            LoginState::Validating { .. } => {}
            LoginState::Success | LoginState::Error(_) => {
                self.dismiss(cx);
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
            AlanAction::Paste(text) => {
                if let LoginState::Prompting { input, .. } = &mut self.state {
                    input.push_str(text);
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
                    KeyCode::Char('p') if key.modifiers == KeyModifiers::CONTROL => {
                        self.move_selection(-1);
                        cx.notify();
                    }
                    KeyCode::Char('n') if key.modifiers == KeyModifiers::CONTROL => {
                        self.move_selection(1);
                        cx.notify();
                    }
                    KeyCode::Enter => self.on_enter(cx),
                    KeyCode::Esc => self.dismiss(cx),
                    KeyCode::Backspace => {
                        if let LoginState::Prompting { input, .. } = &mut self.state {
                            input.pop();
                            cx.notify();
                        }
                    }
                    KeyCode::Char(character)
                        if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
                    {
                        if let LoginState::Prompting { input, .. } = &mut self.state {
                            input.push(character);
                            cx.notify();
                        }
                    }
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.dismiss(cx);
                    }
                    _ => {}
                }
                ActionStatus::Handled
            }
            _ => ActionStatus::Continue,
        }
    }

    fn render(&self, frame: &mut Frame, area: Rect, _cx: &RenderContext<'_, '_, AlanAction>) {
        render_login(frame, area, &self.state);
    }
}

fn render_login(frame: &mut Frame, area: Rect, state: &LoginState) {
    let width = area.width.saturating_sub(4).min(88);
    let height = area.height.saturating_sub(2).min(18);
    if width < 12 || height < 6 {
        return;
    }
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    let accent = Style::default().fg(theme::PROMPT_FG);
    let muted = Style::default().fg(theme::TOOL_FG);
    let (heading, footer) = match state {
        LoginState::Selecting { .. } => (
            "Choose a provider".to_owned(),
            if width >= 60 {
                " ↑↓ / C-n C-p move   Enter select   Esc close "
            } else if width >= 38 {
                " ↑↓ move · Enter select · Esc close "
            } else {
                " Enter / Esc "
            },
        ),
        LoginState::Prompting { provider_id, .. } => (
            format!("{} / API key", provider_id.0),
            if width >= 38 {
                " Enter sign in · Esc cancel "
            } else {
                " Enter / Esc "
            },
        ),
        LoginState::Validating { .. } => ("Signing in…".to_owned(), " Esc close "),
        LoginState::Success => ("Signed in".to_owned(), " Enter / Esc close "),
        LoginState::Error(_) => ("Sign-in failed".to_owned(), " Enter / Esc close "),
    };
    let title = vec![Span::styled(" Login ", accent.add_modifier(Modifier::BOLD))];
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(accent)
        .style(Style::default().bg(Color::Reset).fg(theme::USER_FG))
        .title(Line::from(title))
        .title_alignment(Alignment::Center)
        .title_bottom(Line::from(footer).style(muted));
    let inner = block.inner(popup);
    frame.render_widget(Clear, popup);
    frame.render_widget(block, popup);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" ❯ ", accent),
            Span::raw(heading),
        ])),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    frame.render_widget(
        Paragraph::new("─".repeat(inner.width as usize)).style(muted),
        Rect::new(inner.x, inner.y + 1, inner.width, 1),
    );
    let content = Rect::new(
        inner.x,
        inner.y + 2,
        inner.width,
        inner.height.saturating_sub(2),
    );
    match state {
        LoginState::Selecting {
            providers,
            selected,
        } => {
            let rows = content.height as usize;
            let start = selected
                .saturating_sub(rows / 2)
                .min(providers.len().saturating_sub(rows));
            let lines: Vec<Line> = providers
                .iter()
                .skip(start)
                .take(rows)
                .enumerate()
                .map(|(offset, provider)| {
                    let active = start + offset == *selected;
                    let style = if active {
                        Style::default()
                            .bg(theme::SELECTION_BG)
                            .fg(theme::SELECTION_FG)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(theme::EDITOR_FG)
                    };

                    let prefix = "   ";
                    let current_len =
                        (prefix.chars().count() + provider.name.chars().count()) as u16;
                    let padding_len = width.saturating_sub(current_len) as usize;
                    let padding = " ".repeat(padding_len);
                    Line::from(vec![
                        Span::styled(prefix, accent),
                        Span::raw(provider.name.as_str()),
                        Span::styled(padding, style),
                    ])
                    .style(style)
                })
                .collect();
            if providers.is_empty() {
                frame.render_widget(
                    Paragraph::new("  No providers available").style(muted),
                    content,
                );
            } else {
                frame.render_widget(Paragraph::new(lines), content);
            }
        }
        LoginState::Prompting { input, .. } => {
            let visible = input
                .chars()
                .count()
                .min(content.width.saturating_sub(4) as usize);
            let value = if input.is_empty() {
                Span::styled("Paste or type your API key", muted)
            } else {
                Span::raw("•".repeat(visible))
            };
            frame.render_widget(
                Paragraph::new(Line::from(vec![Span::styled(" ❯ ", accent), value])),
                content,
            );
            frame.set_cursor_position((content.x + 3 + visible as u16, content.y));
        }
        LoginState::Validating { message } => render_message(frame, content, message, muted),
        LoginState::Success => render_message(
            frame,
            content,
            "Logged in successfully",
            Style::default().fg(theme::TOOL_DONE_FG),
        ),
        LoginState::Error(message) => render_message(
            frame,
            content,
            message,
            Style::default().fg(theme::TOOL_ERROR_FG),
        ),
    }
}

fn render_message(frame: &mut Frame, area: Rect, message: &str, style: Style) {
    let area = Rect::new(
        area.x + 1,
        area.y,
        area.width.saturating_sub(2),
        area.height,
    );
    frame.render_widget(
        Paragraph::new(message)
            .style(style)
            .wrap(Wrap { trim: false }),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn provider_selection_scrolls_into_view() {
        let state = LoginState::Selecting {
            providers: (0..30)
                .map(|i| LoginProvider {
                    id: ProviderId(format!("provider-{i:02}")),
                    name: format!("provider-{i:02}"),
                })
                .collect(),
            selected: 29,
        };
        for (width, height) in [(100, 30), (45, 12), (20, 8), (8, 3)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| render_login(frame, frame.area(), &state))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let screen: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
            if width >= 20 {
                assert!(screen.contains("provider-29"));
                assert!(
                    buffer
                        .content
                        .iter()
                        .any(|cell| cell.bg == theme::SELECTION_BG)
                );
            }
        }
    }

    #[test]
    fn login_states_render_with_terminal_background() {
        let states = [
            LoginState::Prompting {
                provider_id: ProviderId("example".into()),
                auth_method: AuthMethod::ApiKey,
                input: "secret-key".repeat(80),
            },
            LoginState::Selecting {
                providers: Vec::new(),
                selected: 0,
            },
            LoginState::Validating {
                message: "Validating credentials…".into(),
            },
            LoginState::Success,
            LoginState::Error("Could not authenticate. Check your API key.".into()),
        ];
        for state in states {
            let mut terminal = Terminal::new(TestBackend::new(45, 12)).unwrap();
            terminal
                .draw(|frame| render_login(frame, frame.area(), &state))
                .unwrap();
            let buffer = terminal.backend().buffer();
            assert!(buffer.content.iter().all(|cell| cell.bg == Color::Reset));
            let screen: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
            assert!(screen.contains("Login"));
            assert!(!screen.contains("secret-key"));
        }
    }
}
