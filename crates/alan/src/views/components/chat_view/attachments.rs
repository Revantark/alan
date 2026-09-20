use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::views::theme;
use alan_tui::RenderContext;
use alan_tui::entity::Entity;

use super::super::editor::PromptEditor;

pub(crate) fn render_attachments(
    frame: &mut Frame,
    area: Rect,
    editor: Entity<PromptEditor>,
    cx: &RenderContext<'_, '_, crate::root::AlanAction>,
) {
    if area.height == 0 {
        return;
    }
    let Some(names) = cx.read(editor, |e| {
        e.attachments()
            .iter()
            .map(|attachment| attachment.name.clone())
            .collect::<Vec<_>>()
    }) else {
        return;
    };
    if names.is_empty() {
        return;
    }

    let mut lines: Vec<Line<'static>> = vec![
        Line::from("\n"),
        Line::from(Span::styled(
            "  Attachments  (esc removes last)",
            Style::default().fg(theme::ATTACHMENT_FG).bold(),
        )),
    ];
    for name in names {
        lines.push(Line::from(Span::styled(
            format!("   - {name}"),
            Style::default().fg(theme::ATTACHMENT_FG),
        )));
    }
    let attachments = Paragraph::new(ratatui::text::Text::from(lines))
        .style(Style::default().bg(theme::ATTACHMENT_BG));
    frame.render_widget(attachments, area);
}
