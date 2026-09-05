use crate::core::Controller;
use crate::views::UiState;
use crate::views::component::Component;
use crate::views::theme;
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
        _controller: &Controller,
        state: &mut UiState,
    ) {
        let background = Paragraph::new("").style(Style::default().bg(theme::EDITOR_BG));
        frame.render_widget(background, area);

        // The `status` row is painted over by the `Status` entity (see
        // `paint_status` in `tui_root`); we only need the background to
        // cover it, which the outer `background` fill does.
        let [
            attachment_area,
            _top_padding,
            _status_area,
            _status_editor_gap,
            editor_area,
            _bottom_padding,
        ] = Layout::vertical([
            Constraint::Length(state.attachment_height()),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(area);

        // Render attachment section when there are pending images.
        if !state.attachments().is_empty() {
            let mut lines: Vec<Line<'static>> = vec![
                Line::from("\n"),
                Line::from(Span::styled(
                    "  Attachments  (esc removes last)",
                    Style::default().fg(theme::ATTACHMENT_FG).bold(),
                )),
            ];
            for attachment in state.attachments() {
                lines.push(Line::from(Span::styled(
                    format!("   - {}", attachment.name),
                    Style::default().fg(theme::ATTACHMENT_FG),
                )));
            }
            let attachments =
                Paragraph::new(Text::from(lines)).style(Style::default().bg(theme::ATTACHMENT_BG));
            frame.render_widget(attachments, attachment_area);
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
