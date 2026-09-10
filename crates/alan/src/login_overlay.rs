//! Login overlay owning the provider authentication flow.
use crate::root::AlanAction;
use crate::views::theme;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use providers::{AuthMethod, AuthResult, CredentialStore, ProviderId, ProviderRegistry};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;
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
                        if let Err(e) = credentials.put(provider.id(), credential).await {
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
            LoginState::Prompting { input, .. } => {
                let value = "•".repeat(input.chars().count());
                let prompt_line = Line::from("Enter API key");
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
            LoginState::Success => {
                frame.render_widget(Paragraph::new(String::from("Logged in")), content_area);
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

fn draw_shortcuts(frame: &mut Frame, area: Rect, text: &str) {
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            text,
            Style::default().fg(theme::MUTED_FG),
        ))),
        area,
    );
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
