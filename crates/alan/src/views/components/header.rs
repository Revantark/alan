use crate::views::theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use tui::{Component, RenderContext};

#[derive(Debug, Default)]
pub struct Header;

impl<A: 'static> Component<A> for Header {
    fn render(&self, frame: &mut Frame, area: Rect, _cx: &RenderContext<'_, A>) {
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
