use crate::core::{Controller, LoginState};
use crate::views::UiState;
use crate::views::component::Component;
use crate::views::theme;
use providers::AuthPrompt;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::prelude::Widget;
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;

#[derive(Debug, Default)]
pub struct Footer;

impl Component for Footer {
    fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        controller: &Controller,
        state: &mut UiState,
    ) {
        let background = Paragraph::new("").style(Style::default().bg(theme::EDITOR_BG));
        frame.render_widget(background, area);

        let [
            _top_padding,
            status_area,
            _status_editor_gap,
            editor_area,
            _bottom_padding,
        ] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(area);

        let (indicator, indicator_style, shortcuts) = if controller.is_busy() {
            (
                "  ● thinking",
                Style::default().italic().fg(ratatui::style::Color::Yellow),
                "· Ctrl-C stop",
            )
        } else {
            (
                "  ● idle",
                Style::default().fg(ratatui::style::Color::Green),
                "  Enter send · Ctrl-C quit",
            )
        };
        let mut status_spans = vec![
            Span::styled(indicator, indicator_style),
            Span::styled(shortcuts, Style::default().fg(theme::MUTED_FG)),
        ];
        if controller.plan_mode() {
            status_spans.push(Span::styled(
                " · Plan mode",
                Style::default().fg(ratatui::style::Color::White),
            ));
        }
        let status = Line::from(status_spans);
        frame.render_widget(
            Paragraph::new(status).style(Style::default().bg(theme::EDITOR_BG)),
            status_area,
        );

        let secret_input = matches!(
            controller.login_state(),
            LoginState::Prompting {
                prompt: AuthPrompt::Secret { .. },
                ..
            }
        );
        if secret_input {
            let value = "•".repeat(state.input().chars().count());
            let input_line = Line::from(vec![
                Span::styled("  › ", Style::default().fg(theme::PROMPT_FG)),
                Span::styled(value.clone(), Style::default().fg(theme::EDITOR_FG)),
            ]);
            frame.render_widget(
                Paragraph::new(Text::from(input_line))
                    .style(Style::default().fg(theme::EDITOR_FG).bg(theme::EDITOR_BG)),
                editor_area,
            );
            let cursor_x = editor_area
                .x
                .saturating_add(4)
                .saturating_add(value.chars().count() as u16)
                .min(editor_area.right().saturating_sub(1));
            frame.set_cursor_position((cursor_x, editor_area.y));
            return;
        }

        let [prompt_area, input_area] =
            Layout::horizontal([Constraint::Length(theme::PROMPT_GUTTER), Constraint::Min(1)])
                .areas(editor_area);
        frame.render_widget(
            Paragraph::new("  › ")
                .style(Style::default().fg(theme::PROMPT_FG).bg(theme::EDITOR_BG)),
            prompt_area,
        );
        state.editor().render(input_area, frame.buffer_mut());
        if let Some(position) = state.cursor_screen_position() {
            frame.set_cursor_position(position);
        }
    }
}
