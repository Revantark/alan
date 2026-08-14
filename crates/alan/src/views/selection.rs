use ratatui::style::Color;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

/// Position in the chat transcript: line index and display column offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TextPosition {
    pub line: usize,
    pub col: usize,
}

impl TextPosition {
    pub fn new(line: usize, col: usize) -> Self {
        Self { line, col }
    }
}

/// Selection granularity mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SelectionMode {
    #[default]
    Character,
    Word,
}

/// Represents the active selection range in the transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: TextPosition,
    pub cursor: TextPosition,
    pub mode: SelectionMode,
    /// The initial word boundary anchor when starting a word-based selection.
    pub word_anchor: Option<(usize, usize)>,
    pub is_dragging: bool,
}

impl Selection {
    pub fn new(anchor: TextPosition) -> Self {
        Self {
            anchor,
            cursor: anchor,
            mode: SelectionMode::Character,
            word_anchor: None,
            is_dragging: true,
        }
    }

    pub fn new_word(pos: TextPosition, start_col: usize, end_col: usize) -> Self {
        Self {
            anchor: TextPosition::new(pos.line, start_col),
            cursor: TextPosition::new(pos.line, end_col),
            mode: SelectionMode::Word,
            word_anchor: Some((start_col, end_col)),
            is_dragging: true,
        }
    }

    pub fn update_cursor(&mut self, pos: TextPosition, lines: &[Line<'static>]) {
        if self.mode == SelectionMode::Word {
            let Some((anchor_start, anchor_end)) = self.word_anchor else {
                self.cursor = pos;
                return;
            };

            let line_idx = pos.line.min(lines.len().saturating_sub(1));
            let (target_start, target_end) = if line_idx < lines.len() {
                find_word_bounds_at(&lines[line_idx], pos.col)
            } else {
                (pos.col, pos.col)
            };

            if pos.line < self.anchor.line
                || (pos.line == self.anchor.line && pos.col < anchor_start)
            {
                // Dragging before the anchor word: anchor the right end, cursor to the left word start
                self.anchor = TextPosition::new(self.anchor.line, anchor_end);
                self.cursor = TextPosition::new(pos.line, target_start);
            } else {
                // Dragging after the anchor word: anchor the left start, cursor to the right word end
                self.anchor = TextPosition::new(self.anchor.line, anchor_start);
                self.cursor = TextPosition::new(pos.line, target_end);
            }
        } else {
            self.cursor = pos;
        }
    }

    pub fn start(&self) -> TextPosition {
        self.anchor.min(self.cursor)
    }

    pub fn end(&self) -> TextPosition {
        self.anchor.max(self.cursor)
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.cursor
    }
}

/// Applies selection highlight to a slice of transcript lines rendered in the viewport.
pub fn apply_selection_to_lines(
    lines: &[Line<'static>],
    scroll_offset: usize,
    selection: Option<&Selection>,
    selection_bg: Color,
    selection_fg: Color,
) -> Vec<Line<'static>> {
    let Some(sel) = selection else {
        return lines.to_vec();
    };

    if sel.is_empty() {
        return lines.to_vec();
    }

    let sel_start = sel.start();
    let sel_end = sel.end();

    lines
        .iter()
        .enumerate()
        .map(|(rel_idx, line)| {
            let line_idx = scroll_offset + rel_idx;
            if line_idx < sel_start.line || line_idx > sel_end.line {
                return line.clone();
            }

            let col_start = if line_idx == sel_start.line {
                sel_start.col
            } else {
                0
            };
            let col_end = if line_idx == sel_end.line {
                sel_end.col
            } else {
                usize::MAX
            };

            highlight_line(line, col_start, col_end, selection_bg, selection_fg)
        })
        .collect()
}

/// Highlights a single Line between `col_start` and `col_end` display column widths.
fn highlight_line(
    line: &Line<'static>,
    col_start: usize,
    col_end: usize,
    selection_bg: Color,
    selection_fg: Color,
) -> Line<'static> {
    if col_start >= col_end {
        return line.clone();
    }

    let mut current_col = 0;
    let mut new_spans = Vec::new();

    for span in &line.spans {
        let span_text = &span.content;
        let mut span_col = current_col;

        let mut before_text = String::new();
        let mut selected_text = String::new();
        let mut after_text = String::new();

        for ch in span_text.chars() {
            let ch_w = ch.width().unwrap_or(0);
            let ch_end = span_col + ch_w;

            if ch_end <= col_start {
                before_text.push(ch);
            } else if span_col >= col_end {
                after_text.push(ch);
            } else {
                selected_text.push(ch);
            }
            span_col += ch_w;
        }

        if !before_text.is_empty() {
            new_spans.push(Span::styled(before_text, span.style));
        }
        if !selected_text.is_empty() {
            let mut style = span.style;
            style = style.bg(selection_bg).fg(selection_fg);
            new_spans.push(Span::styled(selected_text, style));
        }
        if !after_text.is_empty() {
            new_spans.push(Span::styled(after_text, span.style));
        }

        current_col = span_col;
    }

    Line::from(new_spans)
}

/// Extracts selected text as a string from transcript lines, trimming padding if needed.
pub fn extract_selected_text(lines: &[Line<'static>], selection: &Selection) -> String {
    if selection.is_empty() {
        return String::new();
    }

    let start = selection.start();
    let end = selection.end();

    let mut result_lines = Vec::new();

    for line_idx in start.line..=end.line {
        if line_idx >= lines.len() {
            break;
        }

        let line = &lines[line_idx];
        let line_text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();

        let col_start = if line_idx == start.line { start.col } else { 0 };
        let col_end = if line_idx == end.line {
            end.col
        } else {
            usize::MAX
        };

        let mut current_col = 0;
        let mut selected_chars = String::new();

        for ch in line_text.chars() {
            let ch_w = ch.width().unwrap_or(0);
            let ch_end = current_col + ch_w;

            if ch_end > col_start && current_col < col_end {
                selected_chars.push(ch);
            }
            current_col += ch_w;
        }

        result_lines.push(selected_chars.trim_end().to_string());
    }

    result_lines.join("\n")
}

/// Finds the start and end display columns of the word at `col` on `line`.
pub fn find_word_bounds_at(line: &Line<'static>, col: usize) -> (usize, usize) {
    let line_text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    if line_text.is_empty() {
        return (0, 0);
    }

    // Collect chars with their start and end display columns
    let mut char_spans = Vec::new();
    let mut current_col = 0;
    for ch in line_text.chars() {
        let ch_w = ch.width().unwrap_or(0);
        let ch_end = current_col + ch_w;
        char_spans.push((ch, current_col, ch_end));
        current_col = ch_end;
    }

    // Find the character index corresponding to `col`
    let target_idx = char_spans
        .iter()
        .position(|&(_, start, end)| col >= start && col < end)
        .or_else(|| {
            if col >= current_col && !char_spans.is_empty() {
                Some(char_spans.len() - 1)
            } else {
                None
            }
        });

    let Some(target_idx) = target_idx else {
        return (0, 0);
    };

    let is_word_char = |c: char| c.is_alphanumeric() || c == '_';
    let target_is_word = is_word_char(char_spans[target_idx].0);

    // Expand left
    let mut start_idx = target_idx;
    while start_idx > 0 {
        let prev_ch = char_spans[start_idx - 1].0;
        if is_word_char(prev_ch) == target_is_word && !prev_ch.is_whitespace() {
            start_idx -= 1;
        } else {
            break;
        }
    }

    // Expand right
    let mut end_idx = target_idx;
    while end_idx + 1 < char_spans.len() {
        let next_ch = char_spans[end_idx + 1].0;
        if is_word_char(next_ch) == target_is_word && !next_ch.is_whitespace() {
            end_idx += 1;
        } else {
            break;
        }
    }

    let start_col = char_spans[start_idx].1;
    let end_col = char_spans[end_idx].2;
    (start_col, end_col)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_text_position_ordering() {
        let p1 = TextPosition::new(0, 5);
        let p2 = TextPosition::new(1, 2);
        let p3 = TextPosition::new(1, 4);

        assert!(p1 < p2);
        assert!(p2 < p3);
    }

    #[test]
    fn test_extract_selected_text() {
        let lines = vec![
            Line::from("Hello World"),
            Line::from("Second Line of Text"),
            Line::from("Third Line"),
        ];

        let selection = Selection {
            anchor: TextPosition::new(0, 6),
            cursor: TextPosition::new(1, 11),
            mode: SelectionMode::Character,
            word_anchor: None,
            is_dragging: false,
        };

        let extracted = extract_selected_text(&lines, &selection);
        assert_eq!(extracted, "World\nSecond Line");
    }

    #[test]
    fn test_highlight_line() {
        let line = Line::from(vec![Span::raw("Hello "), Span::raw("World!")]);
        let highlighted = highlight_line(&line, 3, 8, Color::Blue, Color::White);
        assert_eq!(highlighted.spans.len(), 4); // "Hel", "lo ", "Wor", "ld!"
    }

    #[test]
    fn test_highlight_line_multibyte_cjk() {
        let line = Line::from(vec![Span::raw("你好世界")]);
        // "你好" takes 4 display columns (2 each).
        let highlighted = highlight_line(&line, 2, 6, Color::Blue, Color::White);
        let texts: Vec<&str> = highlighted
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(texts, vec!["你", "好世", "界"]);
    }

    #[test]
    fn test_double_click_word_drag_expansion() {
        let lines = vec![Line::from(vec![Span::raw("first second third fourth")])];

        // Double click on "second" (col 7)
        let (start, end) = find_word_bounds_at(&lines[0], 7);
        assert_eq!((start, end), (6, 12));
        let mut sel = Selection::new_word(TextPosition::new(0, 7), start, end);

        // Drag right to "third" (col 15)
        sel.update_cursor(TextPosition::new(0, 15), &lines);
        assert_eq!(sel.start(), TextPosition::new(0, 6));
        assert_eq!(sel.end(), TextPosition::new(0, 18));
        assert_eq!(extract_selected_text(&lines, &sel), "second third");

        // Drag left to "first" (col 2)
        sel.update_cursor(TextPosition::new(0, 2), &lines);
        assert_eq!(sel.start(), TextPosition::new(0, 0));
        assert_eq!(sel.end(), TextPosition::new(0, 12));
        assert_eq!(extract_selected_text(&lines, &sel), "first second");
    }
}
