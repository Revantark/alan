use super::centered_rect;
use crate::core::{Controller, LoginState};
use crate::views::UiState;
use crate::views::component::Component;
use crate::views::theme;
use providers::AuthPrompt;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;

#[derive(Debug, Default)]
pub struct LoginOverlay;

impl Component for LoginOverlay {
    fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        controller: &Controller,
        state: &mut UiState,
    ) {
        let login_state = controller.login_state();
        if !login_state.is_open() {
            return;
        }

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

        match login_state {
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
                    "•".repeat(state.input().chars().count())
                } else {
                    state.input().to_owned()
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
            LoginState::Closed => {}
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

fn prompt_message(prompt: &AuthPrompt) -> String {
    match prompt {
        AuthPrompt::Secret { message }
        | AuthPrompt::Text { message }
        | AuthPrompt::ManualCode { message } => message.clone(),
        AuthPrompt::Select { message, .. } => message.clone(),
    }
}
