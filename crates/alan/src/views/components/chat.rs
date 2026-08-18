use crate::core::{Controller, Entry};
use crate::views::UiState;
use crate::views::component::Component;
use crate::views::theme;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};
use unicode_width::UnicodeWidthChar;

#[derive(Debug, Default)]
pub struct Chat {
    layout: TranscriptLayout,
}

#[derive(Debug, Default)]
struct TranscriptLayout {
    width: u16,
    revision: u64,
    entries: Vec<Entry>,
    line_offsets: Vec<usize>,
    lines: Vec<Line<'static>>,
}

impl TranscriptLayout {
    fn sync(&mut self, entries: &[Entry], revision: u64, width: u16) {
        if self.width == width && self.revision == revision {
            return;
        }
        if self.width != width {
            self.rebuild(entries, revision, width);
            return;
        }

        let unchanged = self
            .entries
            .iter()
            .zip(entries)
            .take_while(|(cached, entry)| cached == entry)
            .count();
        if unchanged == self.entries.len() && unchanged == entries.len() {
            self.revision = revision;
            return;
        }

        let line_start = self.line_offsets[unchanged];
        self.entries.truncate(unchanged);
        self.line_offsets.truncate(unchanged + 1);
        self.lines.truncate(line_start);
        self.append(&entries[unchanged..], width);
        self.revision = revision;
    }

    fn rebuild(&mut self, entries: &[Entry], revision: u64, width: u16) {
        self.width = width;
        self.revision = revision;
        self.entries.clear();
        self.line_offsets.clear();
        self.line_offsets.push(0);
        self.lines.clear();
        self.append(entries, width);
    }

    fn append(&mut self, entries: &[Entry], width: u16) {
        let width = usize::from(width.max(1));
        let content_width = width.saturating_sub(theme::CHAT_PADDING * 2).max(1);
        for entry in entries {
            self.lines.extend(wrap_entry(entry, width, content_width));
            self.entries.push(entry.clone());
            self.line_offsets.push(self.lines.len());
        }
    }

    pub(crate) fn height(&self) -> usize {
        self.lines.len()
    }

    pub(crate) fn lines(&self) -> &[Line<'static>] {
        &self.lines
    }

    pub(crate) fn viewport(&self, scroll: usize, height: usize) -> Vec<Line<'static>> {
        let end = scroll.saturating_add(height).min(self.lines.len());
        self.lines[scroll.min(end)..end].to_vec()
    }
}

impl Chat {
    pub fn lines(&self) -> &[Line<'static>] {
        self.layout.lines()
    }
}

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
        self.layout.sync(
            controller.chat(),
            controller.chat_revision(),
            content_area.width.max(1),
        );

        let content_height = self.layout.height();
        let viewport_height = usize::from(content_area.height.max(1));
        let scroll = state.sync_scroll(content_height, viewport_height);
        state.set_chat_area(content_area);

        let viewport_lines = self.layout.viewport(scroll, viewport_height);
        let highlighted_lines = crate::views::selection::apply_selection_to_lines(
            &viewport_lines,
            scroll,
            state.selection(),
            theme::SELECTION_BG,
            theme::SELECTION_FG,
        );

        let chat = Paragraph::new(Text::from(highlighted_lines));
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
                lines.extend(wrap_markdown(text, content_width));
            }
            lines.push(Line::default());
            lines
        }
        Entry::Reasoning(text) => {
            let mut lines = vec![Line::default()];
            if text.is_empty() {
                lines.push(indented_line("(reasoning)", theme::MUTED_FG));
            } else {
                for line in wrap_text(text, content_width) {
                    lines.push(indented_line(&line, theme::REASONING_FG));
                }
            }
            lines.push(Line::default());
            lines
        }
        Entry::ToolCall {
            name,
            arguments,
            output,
            status,
            ..
        } => {
            let (background, foreground) = match status {
                crate::core::chat::ToolStatus::Running => (theme::TOOL_BG, theme::TOOL_FG),
                crate::core::chat::ToolStatus::Completed => {
                    (theme::TOOL_DONE_BG, theme::TOOL_DONE_FG)
                }
                crate::core::chat::ToolStatus::Failed(_) => {
                    (theme::TOOL_ERROR_BG, theme::TOOL_ERROR_FG)
                }
            };
            let mut lines = vec![tool_line("", width, foreground, background)];
            let argument_summary = compact_arguments(name, arguments);

            if name == "bash" {
                if !argument_summary.is_empty() {
                    for line in wrap_text(&argument_summary, content_width.saturating_sub(2)) {
                        lines.push(tool_detail_line(&line, width, foreground, background));
                    }
                }
            } else {
                let status_color = match status {
                    crate::core::chat::ToolStatus::Running => Color::Yellow,
                    crate::core::chat::ToolStatus::Completed => foreground,
                    crate::core::chat::ToolStatus::Failed(_) => foreground,
                };
                lines.push(tool_header(name, width, status_color, background));
                if !argument_summary.is_empty() {
                    for line in wrap_text(&argument_summary, content_width.saturating_sub(2)) {
                        lines.push(tool_detail_line(&line, width, foreground, background));
                    }
                }
            }

            match status {
                crate::core::chat::ToolStatus::Failed(error) => {
                    for line in wrap_text(error, content_width.saturating_sub(2)) {
                        lines.push(tool_detail_line(&line, width, foreground, background));
                    }
                }
                _ if output.is_empty() => {
                    lines.push(tool_detail_line(
                        "(no output)",
                        width,
                        foreground,
                        background,
                    ));
                }
                _ => {
                    for line in wrap_text(output, content_width.saturating_sub(2)) {
                        lines.push(tool_detail_line(&line, width, foreground, background));
                    }
                }
            }
            lines.push(tool_line("", width, foreground, background));
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
        Style::default().fg(foreground),
    ))
}

fn compact_arguments(name: &str, arguments: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return arguments.to_owned();
    };

    match name {
        "bash" => value
            .get("command")
            .and_then(serde_json::Value::as_str)
            .map(|command| format!("$ {command}"))
            .unwrap_or_default(),
        "read" | "write" => value
            .get("path")
            .and_then(serde_json::Value::as_str)
            .map(|path| format!("{name}: {path}"))
            .unwrap_or_default(),
        _ => value.to_string(),
    }
}

fn tool_header(name: &str, width: usize, foreground: Color, background: Color) -> Line<'static> {
    let content = format!("{}▸ {name}", " ".repeat(theme::CHAT_PADDING));
    Line::from(Span::styled(
        pad_line(&content, width),
        Style::default().fg(foreground).bg(background).bold(),
    ))
}

fn tool_line(text: &str, width: usize, foreground: Color, background: Color) -> Line<'static> {
    let content = format!("{}{}", " ".repeat(theme::CHAT_PADDING), text);
    let used_width = Line::from(content.as_str()).width();
    let trailing = " ".repeat(width.saturating_sub(used_width));
    Line::from(Span::styled(
        format!("{content}{trailing}"),
        Style::default().fg(foreground).bg(background),
    ))
}

fn tool_detail_line(
    text: &str,
    width: usize,
    foreground: Color,
    background: Color,
) -> Line<'static> {
    let content = format!("{}{}{}", " ".repeat(theme::CHAT_PADDING), "  ", text);
    let used_width = Line::from(content.as_str()).width();
    let trailing = " ".repeat(width.saturating_sub(used_width));
    Line::from(Span::styled(
        format!("{content}{trailing}"),
        Style::default().fg(foreground).bg(background),
    ))
}

fn pad_line(text: &str, width: usize) -> String {
    let used_width = Line::from(text).width();
    format!("{text}{}", " ".repeat(width.saturating_sub(used_width)))
}

#[derive(Clone, Copy, Debug, Default)]
struct MarkdownStyleSheet;

impl tui_markdown::StyleSheet for MarkdownStyleSheet {
    fn heading(&self, level: u8) -> Style {
        tui_markdown::DefaultStyleSheet.heading(level).bold()
    }

    fn code(&self) -> Style {
        Style::default().fg(theme::RESPONSE_FG).bg(Color::Reset)
    }
}

fn wrap_markdown(text: &str, width: usize) -> Vec<Line<'static>> {
    let options = tui_markdown::Options::new(MarkdownStyleSheet);
    let markdown = tui_markdown::from_str_with_options(text, &options);
    let mut lines = Vec::new();
    let prefix = " ".repeat(theme::CHAT_PADDING);
    let available = width.saturating_sub(theme::CHAT_PADDING).max(1);

    for source in markdown.lines {
        let mut current = Line::from(Span::styled(
            prefix.clone(),
            Style::default().fg(theme::RESPONSE_FG),
        ));
        let mut current_width = theme::CHAT_PADDING;

        for span in source.spans {
            let style = span.style;
            for character in span.content.chars() {
                let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
                if current_width > theme::CHAT_PADDING
                    && current_width - theme::CHAT_PADDING + character_width > available
                {
                    lines.push(current);
                    current = Line::from(Span::styled(
                        prefix.clone(),
                        Style::default().fg(theme::RESPONSE_FG),
                    ));
                    current_width = theme::CHAT_PADDING;
                }
                current.push_span(Span::styled(
                    character.to_string(),
                    Style::default().fg(theme::RESPONSE_FG).patch(style),
                ));
                current_width += character_width;
            }
        }
        lines.push(current);
    }

    if lines.is_empty() {
        lines.push(indented_line("", theme::RESPONSE_FG));
    }
    lines
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
            let character_width = character.width().unwrap_or(0);
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
    use crate::core::chat::ToolStatus;

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

    #[test]
    fn transcript_layout_reuses_unchanged_entries() {
        let entries = vec![
            Entry::Prompt("first".into()),
            Entry::ToolCall {
                id: "call-1".into(),
                name: "bash".into(),
                arguments: r#"{\"command\":\"echo first\"}"#.into(),
                output: "first output".into(),
                status: ToolStatus::Completed,
            },
        ];
        let mut layout = TranscriptLayout::default();
        layout.sync(&entries, 1, 80);
        let cached_first = layout.lines[..layout.line_offsets[1]].to_vec();

        let mut changed = entries.clone();
        changed.push(Entry::Response("second".into()));
        layout.sync(&changed, 2, 80);

        assert_eq!(
            &layout.lines[..layout.line_offsets[1]],
            cached_first.as_slice()
        );
        assert_eq!(layout.entries, changed);
    }

    #[test]
    fn transcript_layout_rebuilds_on_width_change() {
        let entries = vec![Entry::Response("abcdefgh".into())];
        let mut layout = TranscriptLayout::default();
        layout.sync(&entries, 1, 12);
        let narrow_height = layout.height();
        layout.sync(&entries, 1, 80);

        assert!(layout.height() < narrow_height);
    }
}
