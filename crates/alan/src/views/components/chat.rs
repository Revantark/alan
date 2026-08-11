use crate::core::{Controller, Entry};
use crate::views::UiState;
use crate::views::component::Component;
use crate::views::theme;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};

#[derive(Debug, Default)]
pub struct Chat;

impl Component for Chat {
    fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        controller: &Controller,
        state: &mut UiState,
    ) {
        let [content_area, scrollbar_area] =
            Layout::horizontal([Constraint::Min(1), Constraint::Length(1)]).areas(area);
        let transcript = build_transcript(controller.chat(), content_area.width.max(1));
        let content_height = transcript.height();
        let viewport_height = usize::from(content_area.height.max(1));
        let scroll = state.sync_scroll(content_height, viewport_height);
        let paragraph_scroll = scroll.min(usize::from(u16::MAX)) as u16;
        let chat = Paragraph::new(transcript).scroll((paragraph_scroll, 0));
        frame.render_widget(chat, content_area);

        if state.max_scroll() > 0 {
            let scrollbar_position = scrollbar_position(scroll, state.max_scroll(), content_height);
            let mut scrollbar = ScrollbarState::new(content_height)
                .viewport_content_length(viewport_height)
                .position(scrollbar_position);
            let scrollbar_widget = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(None)
                .thumb_symbol("▌")
                .thumb_style(Style::default().fg(Color::DarkGray));
            frame.render_stateful_widget(scrollbar_widget, scrollbar_area, &mut scrollbar);
        }
    }
}

fn scrollbar_position(scroll: usize, max_scroll: usize, content_height: usize) -> usize {
    if max_scroll == 0 || content_height <= 1 {
        return 0;
    }
    scroll
        .saturating_mul(content_height - 1)
        .saturating_add(max_scroll / 2)
        / max_scroll
}

fn build_transcript(entries: &[Entry], width: u16) -> Text<'static> {
    let width = usize::from(width.max(1));
    let content_width = width.saturating_sub(theme::CHAT_PADDING * 2).max(1);
    let mut lines = Vec::new();

    for entry in entries {
        lines.extend(wrap_entry(entry, width, content_width));
    }
    Text::from(lines)
}

fn wrap_entry(entry: &Entry, width: usize, content_width: usize) -> Vec<Line<'static>> {
    match entry {
        Entry::Prompt(text) => {
            let mut lines = Vec::new();
            lines.push(background_line("", width, theme::USER_FG, theme::USER_BG));
            for line in wrap_text(text, content_width) {
                lines.push(background_line(
                    &line,
                    width,
                    theme::USER_FG,
                    theme::USER_BG,
                ));
            }
            lines.push(background_line("", width, theme::USER_FG, theme::USER_BG));
            lines
        }
        Entry::Response(text) => {
            let mut lines = vec![Line::default()];
            if text.is_empty() {
                lines.push(indented_line("(empty response)", theme::MUTED_FG));
            } else {
                for line in wrap_text(text, content_width) {
                    lines.push(indented_line(&line, theme::RESPONSE_FG));
                }
            }
            lines.push(Line::default());
            lines
        }
        Entry::Error(text) => {
            let mut lines = vec![Line::default()];
            for line in wrap_text(text, content_width) {
                lines.push(indented_line(&line, Color::Red));
            }
            lines.push(Line::default());
            lines
        }
    }
}

fn background_line(
    text: &str,
    width: usize,
    foreground: Color,
    background: Color,
) -> Line<'static> {
    let content = format!("{}{}", " ".repeat(theme::CHAT_PADDING), text);
    let used_width = Line::from(content.as_str()).width();
    let trailing = " ".repeat(width.saturating_sub(used_width));
    Line::from(Span::styled(
        format!("{content}{trailing}"),
        Style::default().fg(foreground).bg(background),
    ))
}

fn indented_line(text: &str, foreground: Color) -> Line<'static> {
    Line::from(Span::styled(
        format!("{}{}", " ".repeat(theme::CHAT_PADDING), text),
        Style::default().fg(foreground).bold(),
    ))
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let mut wrapped = Vec::new();
    for source_line in text.lines() {
        if source_line.is_empty() {
            wrapped.push(String::new());
            continue;
        }

        let mut current = String::new();
        let mut current_width = 0;
        for character in source_line.chars() {
            let character_width = Line::from(character.to_string()).width();
            if current_width > 0 && current_width + character_width > width {
                wrapped.push(std::mem::take(&mut current));
                current_width = 0;
            }
            current.push(character);
            current_width += character_width;
        }
        wrapped.push(current);
    }
    if wrapped.is_empty() {
        wrapped.push(String::new());
    }
    wrapped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_scroll_endpoints_to_scrollbar_endpoints() {
        assert_eq!(scrollbar_position(0, 80, 100), 0);
        assert_eq!(scrollbar_position(40, 80, 100), 50);
        assert_eq!(scrollbar_position(80, 80, 100), 99);
    }

    #[test]
    fn wraps_by_display_width_without_splitting_utf8() {
        assert_eq!(wrap_text("abcdef", 3), ["abc", "def"]);
        assert_eq!(wrap_text("界界界", 4), ["界界", "界"]);
    }
}
