use crate::core::Controller;
use crate::views::UiState;
use crate::views::component::Component;
use crate::views::theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

#[derive(Debug, Default)]
pub struct Header;

impl Component for Header {
    fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        _controller: &Controller,
        _state: &mut UiState,
    ) {
        let header = Paragraph::new(Line::from(vec![Span::styled(
            " alan ",
            Style::default()
                .fg(ratatui::style::Color::Black)
                .bg(theme::PROMPT_FG)
                .add_modifier(Modifier::BOLD),
        )]));
        frame.render_widget(header, area);
    }
}
