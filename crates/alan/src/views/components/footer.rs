use crate::core::{Controller, LoginState};
use crate::views::UiState;
use crate::views::component::Component;
use crate::views::theme;
use providers::AuthPrompt;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
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

        let [_top_padding, status_area, _status_editor_gap, editor_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(area);

        let status = if controller.is_busy() {
            Line::from(vec![
                Span::styled(
                    "  ● thinking",
                    Style::default().italic().fg(ratatui::style::Color::Yellow),
                ),
                Span::styled(
                    "  Esc clear · Ctrl-C stop",
                    Style::default().fg(theme::MUTED_FG),
                ),
            ])
        } else {
            Line::from(vec![
                Span::styled(
                    "  ● idle",
                    Style::default().fg(ratatui::style::Color::Green),
                ),
                Span::styled(
                    "  Enter send · PageUp/PageDown scroll · Ctrl-C quit",
                    Style::default().fg(theme::MUTED_FG),
                ),
            ])
        };
        frame.render_widget(
            Paragraph::new(status).style(Style::default().bg(theme::EDITOR_BG)),
            status_area,
        );

        let prompt_width = Line::from("  › ").width() as usize;
        let available_width = usize::from(editor_area.width).saturating_sub(prompt_width);
        let secret_input = matches!(
            controller.login_state(),
            LoginState::Prompting {
                prompt: AuthPrompt::Secret { .. },
                ..
            }
        );
        let editor_input = if secret_input {
            "•".repeat(state.input().chars().count())
        } else {
            state.input().to_owned()
        };
        let visible_input = visible_suffix(&editor_input, available_width);
        let input_line = Line::from(vec![
            Span::styled("  › ", Style::default().fg(theme::PROMPT_FG)),
            Span::styled(visible_input.clone(), Style::default().fg(theme::EDITOR_FG)),
        ]);
        frame.render_widget(
            Paragraph::new(Text::from(input_line))
                .style(Style::default().fg(theme::EDITOR_FG).bg(theme::EDITOR_BG)),
            editor_area,
        );

        let input_width = Line::from(visible_input.as_str()).width() as u16;
        let cursor_x = editor_area
            .x
            .saturating_add(prompt_width as u16)
            .saturating_add(input_width)
            .min(editor_area.right().saturating_sub(1));
        frame.set_cursor_position((cursor_x, editor_area.y));
    }
}

fn visible_suffix(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }

    let mut width = 0;
    let mut start = text.len();
    for (index, character) in text.char_indices().rev() {
        let character_width = Line::from(character.to_string()).width();
        if width + character_width > max_width {
            break;
        }
        width += character_width;
        start = index;
    }
    text[start..].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_suffix_uses_display_width() {
        assert_eq!(visible_suffix("abcdef", 3), "def");
        assert_eq!(visible_suffix("界界界", 4), "界界");
        assert_eq!(visible_suffix("abcdef", 0), "");
    }
}
