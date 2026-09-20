use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::views::theme;

/// Rows of the steering band shown while a steering prompt is queued.
pub(crate) const STEER_BAND_HEIGHT: u16 = 3;

pub(crate) fn truncate_single_line(text: &str, max_width: usize) -> String {
    let text = text.lines().next().unwrap_or(text);
    if Line::from(text).width() <= max_width {
        return text.to_owned();
    }
    let budget = max_width.saturating_sub(1);
    let mut out = String::new();
    for ch in text.chars() {
        let w = ch.width().unwrap_or(0);
        if out.width() + w > budget {
            break;
        }
        out.push(ch);
    }
    format!("{out}…")
}

pub(crate) fn render_steering(frame: &mut Frame, area: Rect, text: Option<&str>) {
    let Some(text) = text else {
        return;
    };
    if area.height == 0 || area.width == 0 {
        return;
    }
    let content_width = area
        .width
        .saturating_sub((theme::CHAT_PADDING * 2) as u16)
        .max(1) as usize;
    let line = truncate_single_line(text, content_width);
    let style = Style::default().fg(theme::STEER_FG);
    let [top, middle, _bottom] = ratatui::layout::Layout::vertical([
        ratatui::layout::Constraint::Length(1),
        ratatui::layout::Constraint::Length(1),
        ratatui::layout::Constraint::Length(1),
    ])
    .areas(area);
    frame.render_widget(Paragraph::new(""), top);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(
                "{}{} {}",
                " ".repeat(theme::CHAT_PADDING),
                theme::STEER_MARKER,
                line
            ),
            style,
        ))),
        middle,
    );
    frame.render_widget(Paragraph::new(""), _bottom);
}
