//! Transcript layout cache and line-wrapping helpers.
//!
//! `TranscriptLayout` incrementally turns the controller's [`Entry`] list into
//! wrapped [`Line`]s, reusing unchanged prefixes across renders. The wrapping
//! helpers are pure functions over an entry plus the theme, with no dependency
//! on the `ChatHistory` component or its view state.

use std::ops::Range;

use crate::core::Entry;
use crate::views::theme;
use ratatui::buffer::CellWidth;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

#[derive(Debug, Default)]
pub(super) struct TranscriptLayout {
    width: u16,
    revision: u64,
    entries: Vec<Entry>,
    line_offsets: Vec<usize>,
    lines: Vec<Line<'static>>,
}

impl TranscriptLayout {
    pub(super) fn sync(&mut self, entries: &[Entry], revision: u64, width: u16) {
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

    pub(super) fn height(&self) -> usize {
        self.lines.len()
    }

    pub(super) fn lines(&self) -> &[Line<'static>] {
        &self.lines
    }

    pub(super) fn viewport(&self, scroll: usize, height: usize) -> Vec<Line<'static>> {
        let end = scroll.saturating_add(height).min(self.lines.len());
        self.lines[scroll.min(end)..end].to_vec()
    }
}

pub(super) fn scrollbar_position(scroll: usize, max_scroll: usize, content_height: usize) -> usize {
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
            if text.is_empty() {
                // Image-only submission: no text accompanied the attachment.
                lines.push(indented_line("(attachment)", theme::MUTED_FG));
            } else {
                for line in wrap_text(text, content_width) {
                    lines.push(background_line(
                        &line,
                        width,
                        theme::USER_FG,
                        theme::USER_BG,
                    ));
                }
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
        // Rendered as markdown so command output keeps its formatting.
        Entry::Info(text) => {
            let mut lines = vec![Line::default()];
            lines.extend(wrap_markdown(text, content_width));
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
    Line::from(vec![
        Span::styled(content, Style::default().fg(foreground).bg(background)),
        Span::styled(trailing, Style::default().bg(background)),
    ])
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
    Line::from(vec![
        Span::styled(content, Style::default().fg(foreground).bg(background)),
        Span::styled(trailing, Style::default().bg(background)),
    ])
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
    Line::from(vec![
        Span::styled(content, Style::default().fg(foreground).bg(background)),
        Span::styled(trailing, Style::default().bg(background)),
    ])
}

fn pad_line(text: &str, width: usize) -> String {
    let used_width = Line::from(text).width();
    format!("{text}{}", " ".repeat(width.saturating_sub(used_width)))
}

#[derive(Clone, Copy, Debug, Default)]
struct MarkdownStyleSheet;

impl tui_markdown::StyleSheet for MarkdownStyleSheet {
    fn heading(&self, level: u8) -> Style {
        let style = Style::default().fg(theme::HEADING_FG);
        match level {
            1..=3 => style.bold(),
            _ => style,
        }
    }

    fn code(&self) -> Style {
        Style::default().fg(theme::RESPONSE_FG).bg(Color::Reset)
    }
}

/// Wraps markdown-rendered text into lines that fit `width` display cells.
///
/// The wrapping unit is a fragment: a word plus the whitespace that follows it.
/// Lines break between fragments, so a word is only split when it is wider than
/// a whole line, and then at grapheme boundaries. Fragments and styles are kept
/// as byte ranges over a per-line buffer, so wrapping allocates once per emitted
/// span instead of once per grapheme. `tests::reference_wrap` pins the behaviour.
fn wrap_markdown(text: &str, width: usize) -> Vec<Line<'static>> {
    let options = tui_markdown::Options::new(MarkdownStyleSheet);
    let markdown = tui_markdown::from_str_with_options(text, &options);
    let response_style = Style::default().fg(theme::RESPONSE_FG);
    let available = width.saturating_sub(theme::CHAT_PADDING).max(1);
    let prefix = " ".repeat(theme::CHAT_PADDING);
    let mut lines: Vec<Line<'static>> = Vec::new();

    let mut content = String::new();
    let mut graphemes: Vec<Grapheme> = Vec::new();
    let mut runs: Vec<(usize, Style)> = Vec::new();

    for source in markdown.lines {
        content.clear();
        graphemes.clear();
        runs.clear();

        let base = response_style.patch(source.style);
        let line_bytes: usize = source.spans.iter().map(|span| span.content.len()).sum();
        content.reserve(line_bytes);
        graphemes.reserve(line_bytes);

        for span in &source.spans {
            let style = base.patch(span.style);
            if runs.last().is_none_or(|(_, run_style)| *run_style != style) {
                runs.push((content.len(), style));
            }
            for grapheme in span.styled_graphemes(Style::new()) {
                let start = content.len();
                content.push_str(grapheme.symbol);
                graphemes.push(Grapheme {
                    start,
                    end: content.len(),
                    width: usize::from(grapheme.symbol.cell_width()),
                    whitespace: grapheme.is_whitespace(),
                });
            }
        }

        let fragments = split_fragments(&graphemes, available);
        for range in first_fit(&fragments, available) {
            let fragment_count = range.len();
            let mut spans: Vec<Span<'static>> = Vec::with_capacity(fragment_count + 1);
            for (index, fragment) in fragments[range].iter().enumerate() {
                push_styled_range(&mut spans, &content, &runs, fragment.word);
                if index + 1 != fragment_count {
                    push_styled_range(&mut spans, &content, &runs, fragment.whitespace);
                }
            }
            spans.insert(0, Span::styled(prefix.clone(), base));
            lines.push(Line {
                style: source.style,
                alignment: source.alignment,
                spans,
            });
        }
    }

    if lines.is_empty() {
        lines.push(indented_line("", theme::RESPONSE_FG));
    }
    lines
}

/// One source grapheme: its byte range in the line buffer plus its display width.
#[derive(Clone, Copy, Debug)]
struct Grapheme {
    start: usize,
    end: usize,
    width: usize,
    whitespace: bool,
}

/// A wrapping unit: one word and the whitespace that follows it, as byte ranges.
#[derive(Clone, Copy, Debug, Default)]
struct Fragment {
    word: (usize, usize),
    word_width: usize,
    whitespace: (usize, usize),
    whitespace_width: usize,
}

/// Accumulates the word and whitespace ranges of one [`Fragment`].
#[derive(Default)]
struct FragmentBuilder {
    word_start: Option<usize>,
    word_end: usize,
    word_width: usize,
    whitespace_start: Option<usize>,
    whitespace_end: usize,
    whitespace_width: usize,
}

impl FragmentBuilder {
    fn push_word(&mut self, grapheme: &Grapheme) {
        self.word_start.get_or_insert(grapheme.start);
        self.word_end = grapheme.end;
        self.word_width += grapheme.width;
    }

    fn push_whitespace(&mut self, grapheme: &Grapheme) {
        self.whitespace_start.get_or_insert(grapheme.start);
        self.whitespace_end = grapheme.end;
        self.whitespace_width += grapheme.width;
    }

    fn has_word(&self) -> bool {
        self.word_start.is_some()
    }

    fn has_whitespace(&self) -> bool {
        self.whitespace_start.is_some()
    }

    fn take(&mut self) -> Fragment {
        let fragment = Fragment {
            word: (self.word_start.unwrap_or(0), self.word_end),
            word_width: self.word_width,
            whitespace: (self.whitespace_start.unwrap_or(0), self.whitespace_end),
            whitespace_width: self.whitespace_width,
        };
        *self = Self::default();
        fragment
    }
}

/// Groups graphemes into word fragments, splitting any word wider than `available`.
fn split_fragments(graphemes: &[Grapheme], available: usize) -> Vec<Fragment> {
    let mut fragments: Vec<Fragment> = Vec::new();
    let mut builder = FragmentBuilder::default();

    for grapheme in graphemes {
        if grapheme.whitespace {
            builder.push_whitespace(grapheme);
            continue;
        }
        if builder.has_whitespace() {
            fragments.push(builder.take());
        }
        builder.push_word(grapheme);
    }
    if builder.has_word() || builder.has_whitespace() {
        fragments.push(builder.take());
    }

    let mut split: Vec<Fragment> = Vec::with_capacity(fragments.len());
    for fragment in fragments {
        if fragment.word_width <= available {
            split.push(fragment);
            continue;
        }
        let mut chunk = FragmentBuilder::default();
        let word = graphemes.iter().filter(|grapheme| {
            grapheme.start >= fragment.word.0 && grapheme.start < fragment.word.1
        });
        for grapheme in word {
            if grapheme.width > 0
                && chunk.has_word()
                && chunk.word_width + grapheme.width > available
            {
                split.push(chunk.take());
            }
            chunk.push_word(grapheme);
        }
        let mut tail = chunk.take();
        tail.whitespace = fragment.whitespace;
        tail.whitespace_width = fragment.whitespace_width;
        split.push(tail);
    }
    split
}

/// Greedy first fit: returns the fragment range placed on each output line.
fn first_fit(fragments: &[Fragment], available: usize) -> Vec<Range<usize>> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut width = 0;

    for (index, fragment) in fragments.iter().enumerate() {
        if width + fragment.word_width > available && index > start {
            lines.push(start..index);
            start = index;
            width = 0;
        }
        width += fragment.word_width + fragment.whitespace_width;
    }
    lines.push(start..fragments.len());
    lines
}

/// Appends one byte range, split into spans at style boundaries.
fn push_styled_range(
    spans: &mut Vec<Span<'static>>,
    content: &str,
    runs: &[(usize, Style)],
    range: (usize, usize),
) {
    if range.0 >= range.1 {
        return;
    }
    for (index, (start, style)) in runs.iter().enumerate() {
        let stop = runs.get(index + 1).map_or(content.len(), |(next, _)| *next);
        let from = (*start).max(range.0);
        let to = stop.min(range.1);
        if from >= to {
            continue;
        }
        if let Some(span) = spans.last_mut().filter(|span| span.style == *style) {
            span.content.to_mut().push_str(&content[from..to]);
        } else {
            spans.push(Span::styled(content[from..to].to_owned(), *style));
        }
    }
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
    use ratatui::layout::Alignment;

    /// Reference implementation: the `ratatui-textwrap` wrapper this module replaced,
    /// kept as an oracle. Pinned in `[dev-dependencies]` so its behaviour cannot drift.
    fn reference_wrap(text: &str, width: usize) -> Vec<Line<'static>> {
        let options = tui_markdown::Options::new(MarkdownStyleSheet);
        let markdown = tui_markdown::from_str_with_options(text, &options);
        let response_style = Style::default().fg(theme::RESPONSE_FG);
        let available =
            u16::try_from(width.saturating_sub(theme::CHAT_PADDING).max(1)).unwrap_or(u16::MAX);
        let wrapped = ratatui_textwrap::TextWrapper::new().wrap(markdown.lines, available);
        let prefix = " ".repeat(theme::CHAT_PADDING);
        let mut lines: Vec<Line<'static>> = wrapped
            .lines
            .into_iter()
            .map(|mut line| {
                let base = response_style.patch(line.style);
                for span in &mut line.spans {
                    span.style = base.patch(span.style);
                }
                line.spans.insert(0, Span::styled(prefix.clone(), base));
                line
            })
            .collect();
        if lines.is_empty() {
            lines.push(indented_line("", theme::RESPONSE_FG));
        }
        lines
    }

    /// One wrapped line: line style, alignment and per-cell span styles.
    type RenderedLine = (Style, Option<Alignment>, Vec<(char, Style)>);

    /// Line styles, alignment and per-cell span styles, for comparison.
    fn rendered(lines: &[Line<'static>]) -> Vec<RenderedLine> {
        lines
            .iter()
            .map(|line| {
                (
                    line.style,
                    line.alignment,
                    line.spans
                        .iter()
                        .flat_map(|span| span.content.chars().map(|c| (c, span.style)))
                        .collect(),
                )
            })
            .collect()
    }

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

    #[test]
    fn wrap_markdown_breaks_at_word_boundaries() {
        let width = theme::CHAT_PADDING + 20;
        let lines = wrap_markdown("one two three four five six", width);
        let available = 20;
        for line in &lines {
            assert!(line.width() <= theme::CHAT_PADDING + available);
        }
        // No line may end mid-word: re-joining lines with a single space must
        // reproduce the original words (the wrapper trims the break space).
        let joined: Vec<String> = lines
            .iter()
            .map(|line| {
                let text: String = line
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect();
                text[theme::CHAT_PADDING..].to_string()
            })
            .collect();
        let rejoined = joined.join(" ");
        assert_eq!(
            rejoined.split_whitespace().collect::<Vec<_>>(),
            ["one", "two", "three", "four", "five", "six"]
        );
        assert_eq!(joined.len(), lines.len());
    }

    #[test]
    fn wrap_markdown_hard_splits_overlong_words() {
        let width = theme::CHAT_PADDING + 10;
        let lines = wrap_markdown("abcdefghijklmno", width);
        assert!(lines.len() > 1);
        for line in &lines {
            assert!(line.width() <= theme::CHAT_PADDING + 10);
        }
        let joined: String = lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.as_ref()))
            .collect();
        // Overlong words are split at grapheme boundaries; collapse whitespace
        // before comparing so each line's leading padding is ignored.
        assert_eq!(
            joined.split_whitespace().collect::<String>(),
            "abcdefghijklmno"
        );
    }

    #[test]
    fn wrap_markdown_preserves_styles_across_wrap_point() {
        let width = theme::CHAT_PADDING + 12;
        let lines = wrap_markdown("plain **bold text here**", width);
        let bolds: Vec<bool> = lines
            .iter()
            .map(|line| {
                line.spans.iter().any(|span| {
                    span.style
                        .add_modifier
                        .contains(ratatui::style::Modifier::BOLD)
                })
            })
            .collect();
        // The bold run continues onto the second line, so both carry a bold span.
        assert!(bolds.iter().take(2).all(|bold| *bold), "{bolds:?}");
    }

    #[test]
    fn wrap_markdown_empty_input_yields_padded_line() {
        let lines = wrap_markdown("", theme::CHAT_PADDING + 10);
        assert_eq!(lines.len(), 1);
        let text: String = lines[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(text.trim().is_empty());
    }

    #[test]
    fn wrap_markdown_matches_reference_wrapper() {
        let cases = [
            "The quick brown fox jumps over the lazy dog and keeps going for a while.",
            "supercalifragilisticexpialidociousandthensomemoreletters",
            "prefix supercalifragilisticexpialidociousandmore suffix words here",
            "See https://example.com/a/very/long/path/that/never/ends/index.html now.",
            "alpha  beta   gamma    delta",
            "    indented text keeps its leading spaces",
            "trailing spaces here   ",
            "     ",
            "",
            "\n",
            "a\n\n\n\nb",
            "First paragraph.\n\nSecond paragraph with several more words.",
            "## Heading words here\nBody after the heading with more words.",
            "- first bullet item with words\n- second bullet item with more words",
            "> quoted text that is fairly long and wraps somewhere\n> next line",
            "Use `inline code` and **bold text that wraps somewhere** here.",
            "[link](https://example.com/docs) and more text after it",
            "日本語のテキストがここにあります。これは折り返しのテストです。",
            "abc 界界 def 界 ghi jkl",
            "hello 👍 world 🎉 done now",
            "family 👨‍👩‍👧 here and more words",
            "cafe\u{301} nai\u{776}ve text here",
            "non\u{a0}breaking and zero\u{200b}width space text",
            "a\tb\tc and more text here",
            "word, word. word! word? word; word: word--",
        ];
        for text in cases {
            for width in 1..=70 {
                assert_eq!(
                    rendered(&reference_wrap(text, width)),
                    rendered(&wrap_markdown(text, width)),
                    "{text:?} at width {width}"
                );
            }
        }
    }

    #[test]
    fn wrap_markdown_matches_reference_wrapper_on_generated_inputs() {
        let tokens = [
            "a",
            "b",
            "word",
            "longwordlongwordlong",
            " ",
            "  ",
            "\n",
            "\n\n",
            "*",
            "**",
            "_",
            "#",
            "-",
            "1.",
            ">",
            "|",
            "---",
            "日",
            "👍",
            "e\u{301}",
            "\u{a0}",
            "\u{200b}",
            ",",
            "!",
            "(",
        ];
        let mut state: u64 = 0x2545_f491_4f6c_dd1d;
        let mut next = |bound: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state as usize) % bound
        };

        for _ in 0..150 {
            let mut text = String::new();
            for _ in 0..1 + next(30) {
                text.push_str(tokens[next(tokens.len())]);
            }
            for width in [3, 5, 8, 12, 20, 34, 55, 89] {
                assert_eq!(
                    rendered(&reference_wrap(&text, width)),
                    rendered(&wrap_markdown(&text, width)),
                    "{text:?} at width {width}"
                );
            }
        }
    }

    #[test]
    fn wrap_markdown_keeps_graphemes_intact() {
        let width = theme::CHAT_PADDING + 8;
        let lines = wrap_markdown("界界 e\u{301}x 👨‍👩‍👧 done", width);
        let joined: String = lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.as_ref()))
            .collect();
        assert!(joined.contains("界界"), "{joined:?}");
        assert!(joined.contains("e\u{301}"), "{joined:?}");
        assert!(joined.contains("👨‍👩‍👧"), "{joined:?}");
        for line in &lines {
            assert!(
                line.width() <= width,
                "{:?} exceeds the wrap width",
                line.width()
            );
        }
    }
}
