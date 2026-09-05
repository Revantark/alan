//! The chat transcript as a self-contained `tui` component.
//!
//! Owns the incremental wrap cache, scroll state, wheel coalescing, and
//! selection. Rendered from a [`ChatSnapshot`] pushed by the parent via
//! `cx.update`; mouse / wheel / PageUp / PageDown input is routed to it by the
//! parent and handled here.

use crate::core::Entry;
use crate::root::AlanAction;
use crate::views::selection;
use crate::views::selection::{Selection, TextPosition};
use crate::views::theme;
use crossterm::event::Event;
use crossterm::event::{KeyCode, KeyEventKind, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};
use std::cell::RefCell;
use std::time::Instant;
use tui::component::{ActionStatus, Component, RenderContext};
use tui::context::Context;
use unicode_width::UnicodeWidthChar;

/// Lines moved per mouse-wheel notch. Crossterm reports the wheel as discrete
/// notches with no pressure data, so one line per notch keeps a single tick
/// precise; bursts are rate-limited by the flush cap instead.
const WHEEL_LINES_PER_NOTCH: isize = 1;

/// Hard bound on accumulated wheel notches. A trackpad swipe can queue hundreds
/// of events; without a bound the tail keeps scrolling long after the fingers stop.
const MAX_PENDING_WHEEL: isize = 48;

/// Plain-data view of the transcript. The parent builds this from
/// `ChatController` each tick and pushes it down with `cx.update`, so
/// `ChatHistory` never names a core controller type.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatSnapshot {
    pub entries: Vec<Entry>,
    pub revision: u64,
}

/// The chat transcript area: owns the incremental wrap cache, scroll state,
/// wheel coalescing, and selection.
///
/// Render-mutable state (the layout cache and scroll position) sits behind a
/// `RefCell` because `render` is `&self` by framework contract yet must sync
/// the wrap cache against the width it is given. Input handlers run with
/// `&mut self` and borrow through the same `RefCell`.
#[derive(Debug, Default)]
pub struct ChatHistory {
    view: RefCell<View>,
}

#[derive(Debug)]
struct View {
    layout: TranscriptLayout,
    snap: Option<ChatSnapshot>,
    /// Current rendered top line.
    scroll_offset: usize,
    /// Desired top line. Kept equal to `scroll_offset` for immediate input response.
    scroll_target: usize,
    /// Keep viewport pinned to newest content while true.
    follow_output: bool,
    /// Last rendered viewport height. Input events use it before the next
    /// render clamps it again.
    viewport_height: usize,
    max_scroll: usize,
    /// Last rendered content area of the chat (inner, excluding the scrollbar).
    chat_area: Rect,
    /// Active text selection in the transcript.
    selection: Option<Selection>,
    /// Wheel notches accumulated since the last flush. Trackpad swipes emit
    /// hundreds of events; they are coalesced and applied once per tick
    /// instead of one redraw per event.
    pending_wheel: isize,
    /// Last click timestamp and position for double-click detection.
    last_click: Option<(Instant, u16, u16)>,
}

impl Default for View {
    fn default() -> Self {
        Self {
            layout: TranscriptLayout::default(),
            snap: None,
            scroll_offset: 0,
            scroll_target: 0,
            // The transcript starts pinned to the newest content.
            follow_output: true,
            viewport_height: 0,
            max_scroll: 0,
            chat_area: Rect::default(),
            selection: None,
            pending_wheel: 0,
            last_click: None,
        }
    }
}

impl ChatHistory {
    pub fn set(&mut self, snap: ChatSnapshot) {
        self.view.borrow_mut().snap = Some(snap);
    }

    /// Whether the entity already holds a snapshot with this revision.
    pub fn matches_revision(&self, revision: u64) -> bool {
        self.view
            .borrow()
            .snap
            .as_ref()
            .map_or(false, |s| s.revision == revision)
    }

    /// Whether wheel notches are queued and need a flush this tick.
    pub fn has_pending_wheel(&self) -> bool {
        self.view.borrow().pending_wheel != 0
    }

    /// Keep bottom-follow state synchronized and apply queued wheel notches.
    /// Called from the parent's 16ms poll tick when wheel notches are pending;
    /// bottom-follow itself is synchronized by `sync_scroll` during render.
    pub fn tick(&mut self) {
        self.view.borrow_mut().flush_wheel();
    }

    /// Drop queued wheel momentum, e.g. when the user starts typing.
    pub fn cancel_wheel(&mut self) {
        self.view.borrow_mut().pending_wheel = 0;
    }

    /// Re-enable bottom-following, snapping the viewport to the newest content
    /// on the next render. Called by the parent when a prompt is submitted.
    pub fn resume_follow(&mut self) {
        let mut view = self.view.borrow_mut();
        view.follow_output = true;
        view.scroll_target = view.max_scroll;
    }

    /// Whether there is a non-empty selection active.
    pub fn has_active_selection(&self) -> bool {
        self.view
            .borrow()
            .selection
            .as_ref()
            .is_some_and(|s| !s.is_empty())
    }

    /// Clear the active selection (used by Esc in the parent).
    pub fn clear_selection(&mut self) {
        self.view.borrow_mut().selection = None;
    }

    // --- Inherent state mutators, shared by `handle_action` and the tests. ---

    fn push_wheel(&mut self, delta: isize) {
        let mut view = self.view.borrow_mut();
        view.pending_wheel =
            (view.pending_wheel + delta).clamp(-MAX_PENDING_WHEEL, MAX_PENDING_WHEEL);
    }

    fn scroll_by(&mut self, delta: isize) -> bool {
        self.view.borrow_mut().scroll_by(delta)
    }

    #[cfg(test)]
    fn flush_wheel(&mut self) -> bool {
        self.view.borrow_mut().flush_wheel()
    }
}

impl View {
    fn scroll_by(&mut self, delta: isize) -> bool {
        let current = if self.follow_output {
            self.max_scroll
        } else {
            self.scroll_target
        };
        let target = if delta.is_negative() {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current.saturating_add(delta as usize).min(self.max_scroll)
        };
        if target == self.scroll_offset && target == self.scroll_target {
            return false;
        }
        self.scroll_target = target;
        self.scroll_offset = self.scroll_target;
        self.follow_output = self.scroll_offset == self.max_scroll;
        true
    }

    /// Synchronize scroll bounds with the current content/viewport size.
    /// Called during render.
    fn sync_scroll(&mut self, content_height: usize, viewport_height: usize) -> usize {
        self.viewport_height = viewport_height;
        self.max_scroll = content_height.saturating_sub(viewport_height);
        if self.follow_output {
            self.scroll_offset = self.max_scroll;
            self.scroll_target = self.max_scroll;
        } else {
            self.scroll_target = self.scroll_target.min(self.max_scroll);
            self.scroll_offset = self.scroll_offset.min(self.max_scroll);
            if self.scroll_offset == self.scroll_target && self.scroll_target == self.max_scroll {
                self.follow_output = true;
            }
        }
        self.scroll_offset
    }

    /// Apply queued wheel notches, capped per flush so one swipe can't fling
    /// across the whole transcript. Returns true if the viewport moved.
    fn flush_wheel(&mut self) -> bool {
        let pending = std::mem::take(&mut self.pending_wheel);
        if pending == 0 {
            return false;
        }
        let cap = self.viewport_height.clamp(1, 12) as isize;
        let (now, rest) = if pending.abs() > cap {
            (pending.signum() * cap, pending - pending.signum() * cap)
        } else {
            (pending, 0)
        };
        if self.scroll_by(now) {
            self.pending_wheel = rest.clamp(-MAX_PENDING_WHEEL, MAX_PENDING_WHEEL);
            true
        } else {
            false
        }
    }

    fn handle_mouse(&mut self, mouse: &MouseEvent) -> bool {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if self.is_mouse_in_chat(mouse.column, mouse.row) {
                    let now = Instant::now();
                    let is_double_click = self.last_click.is_some_and(|(t, c, r)| {
                        c == mouse.column
                            && r == mouse.row
                            && now.duration_since(t).as_millis() <= 500
                    });

                    if let Some(pos) = self.screen_to_text_pos(mouse.column, mouse.row) {
                        let lines = self.layout.lines();
                        if is_double_click && pos.line < lines.len() {
                            let (start_col, end_col) =
                                selection::find_word_bounds_at(&lines[pos.line], pos.col);
                            let sel = Selection::new_word(pos, start_col, end_col);
                            self.selection = Some(sel);
                            self.last_click = None;
                            self.copy_selection();
                        } else {
                            self.selection = Some(Selection::new(pos));
                            self.last_click = Some((now, mouse.column, mouse.row));
                        }
                        true
                    } else {
                        false
                    }
                } else if self.selection.is_some() {
                    self.selection = None;
                    true
                } else {
                    false
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let is_dragging = self.selection.as_ref().is_some_and(|s| s.is_dragging);
                if is_dragging {
                    if mouse.row < self.chat_area.top() {
                        self.scroll_by(-1);
                    } else if mouse.row >= self.chat_area.bottom() {
                        self.scroll_by(1);
                    }

                    let pos = self.screen_to_text_pos(mouse.column, mouse.row);
                    if let (Some(sel), Some(pos)) = (&mut self.selection, pos) {
                        sel.update_cursor(pos, self.layout.lines());
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(sel) = &mut self.selection {
                    sel.is_dragging = false;
                    if sel.is_empty() {
                        self.selection = None;
                    } else {
                        self.copy_selection();
                    }
                    true
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    fn is_mouse_in_chat(&self, column: u16, row: u16) -> bool {
        column >= self.chat_area.left()
            && column < self.chat_area.right()
            && row >= self.chat_area.top()
            && row < self.chat_area.bottom()
    }

    fn screen_to_text_pos(&self, column: u16, row: u16) -> Option<TextPosition> {
        let rel_row = row.saturating_sub(self.chat_area.top()) as usize;
        let line = self.scroll_offset.saturating_add(rel_row);
        let col = column.saturating_sub(self.chat_area.left()) as usize;
        Some(TextPosition::new(line, col))
    }

    fn copy_selection(&mut self) {
        let Some(sel) = &self.selection else {
            return;
        };
        let text = selection::extract_selected_text(self.layout.lines(), sel);
        if !text.is_empty()
            && let Ok(mut clipboard) = arboard::Clipboard::new()
        {
            let _ = clipboard.set_text(text);
        }
    }
}

impl Component<AlanAction> for ChatHistory {
    fn handle_action(
        &mut self,
        action: &AlanAction,
        cx: &mut Context<'_, Self, AlanAction>,
    ) -> ActionStatus {
        match action {
            // Wheel notches are coalesced; the 16ms poll tick flushes them.
            AlanAction::MouseScrollUp => {
                self.push_wheel(-WHEEL_LINES_PER_NOTCH);
                ActionStatus::Handled
            }
            AlanAction::MouseScrollDown => {
                self.push_wheel(WHEEL_LINES_PER_NOTCH);
                ActionStatus::Handled
            }
            AlanAction::Raw(event) => match event {
                Event::Key(key)
                    if key.kind == KeyEventKind::Press
                        && matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) =>
                {
                    let delta = match key.code {
                        KeyCode::PageUp => -(self.view.borrow().viewport_height.max(1) as isize),
                        _ => self.view.borrow().viewport_height.max(1) as isize,
                    };
                    if self.scroll_by(delta) {
                        cx.notify();
                    }
                    ActionStatus::Handled
                }
                Event::Mouse(mouse) => {
                    let changed = self.handle_mouse_action(mouse);
                    if changed {
                        cx.notify();
                    }
                    ActionStatus::Handled
                }
                _ => ActionStatus::Continue,
            },
            _ => ActionStatus::Continue,
        }
    }

    fn render(&self, frame: &mut Frame, area: Rect, _cx: &RenderContext<'_, AlanAction>) {
        let mut view = self.view.borrow_mut();
        let Some(snap) = view.snap.clone() else {
            return;
        };

        let [content_area, scrollbar_area] =
            Layout::horizontal([Constraint::Min(1), Constraint::Length(1)]).areas(area);
        view.chat_area = content_area;
        view.layout
            .sync(&snap.entries, snap.revision, content_area.width.max(1));

        let content_height = view.layout.height();
        let viewport_height = usize::from(content_area.height.max(1));
        let scroll = view.sync_scroll(content_height, viewport_height);

        let viewport_lines = view.layout.viewport(scroll, viewport_height);
        let highlighted_lines = selection::apply_selection_to_lines(
            &viewport_lines,
            scroll,
            view.selection.as_ref(),
            theme::SELECTION_BG,
            theme::SELECTION_FG,
        );

        frame.render_widget(Paragraph::new(Text::from(highlighted_lines)), content_area);

        if view.max_scroll > 0 {
            let scrollbar_position = scrollbar_position(scroll, view.max_scroll, content_height);
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

impl ChatHistory {
    /// Drive the transcript mouse state from a raw mouse event. Returns true if
    /// the viewport or selection changed and a redraw is needed.
    fn handle_mouse_action(&mut self, mouse: &MouseEvent) -> bool {
        self.view.borrow_mut().handle_mouse(mouse)
    }
}

// --- Layout cache and line-wrapping helpers (moved from the old `chat.rs`). ---

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

    fn height(&self) -> usize {
        self.lines.len()
    }

    fn lines(&self) -> &[Line<'static>] {
        &self.lines
    }

    fn viewport(&self, scroll: usize, height: usize) -> Vec<Line<'static>> {
        let end = scroll.saturating_add(height).min(self.lines.len());
        self.lines[scroll.min(end)..end].to_vec()
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

    // --- Migrated from the old `chat.rs` (layout / wrapping), verbatim. ---

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

    // --- Migrated from `UiState` (scroll / follow / wheel), adapted to the
    // `RefCell<View>` storage. ---

    fn view<'a>(state: &'a ChatHistory) -> std::cell::Ref<'a, View> {
        state.view.borrow()
    }

    fn view_mut<'a>(state: &'a mut ChatHistory) -> std::cell::RefMut<'a, View> {
        state.view.borrow_mut()
    }

    #[test]
    fn follows_bottom_until_user_scrolls_up() {
        let mut state = ChatHistory::default();
        assert_eq!(view_mut(&mut state).sync_scroll(100, 20), 80);

        view_mut(&mut state).scroll_by(-5);
        assert_eq!(view(&state).scroll_target, 75);
        assert_eq!(view(&state).scroll_offset, 75);
        assert!(!view(&state).follow_output);
        state.tick();
        assert_eq!(view(&state).scroll_offset, 75);

        assert_eq!(view_mut(&mut state).sync_scroll(120, 20), 75);
        assert!(!view(&state).follow_output);
    }

    #[test]
    fn scrolling_to_bottom_restores_follow_mode() {
        let mut state = ChatHistory::default();
        view_mut(&mut state).sync_scroll(100, 20);
        view_mut(&mut state).scroll_by(-10);
        view_mut(&mut state).scroll_by(10);

        assert_eq!(view(&state).scroll_target, 80);
        assert!(view(&state).follow_output);
        state.tick();
        assert!(view(&state).follow_output);
        assert_eq!(view_mut(&mut state).sync_scroll(120, 20), 100);
    }

    #[test]
    fn content_shrink_clamps_manual_scroll() {
        let mut state = ChatHistory::default();
        view_mut(&mut state).sync_scroll(100, 20);
        view_mut(&mut state).scroll_by(-10);

        assert_eq!(view_mut(&mut state).sync_scroll(30, 20), 10);
        assert_eq!(view(&state).scroll_target, 10);
        assert!(view(&state).follow_output);
    }

    #[test]
    fn scrolling_updates_rendered_offset_immediately() {
        let mut state = ChatHistory::default();
        view_mut(&mut state).sync_scroll(100, 20);
        view_mut(&mut state).scroll_by(-10);

        assert_eq!(view(&state).scroll_offset, 70);
        assert_eq!(view(&state).scroll_target, 70);
        assert!(!view(&state).follow_output);
    }

    #[test]
    fn scroll_at_bottom_is_a_no_op() {
        let mut state = ChatHistory::default();
        view_mut(&mut state).sync_scroll(100, 20);
        // At the bottom already: scrolling down moves nothing.
        assert!(!view_mut(&mut state).scroll_by(10));
        assert_eq!(view(&state).scroll_offset, 80);
    }

    #[test]
    fn wheel_notches_coalesce_and_cap_per_flush() {
        let mut state = ChatHistory::default();
        view_mut(&mut state).sync_scroll(200, 20);
        view_mut(&mut state).scroll_by(-100);
        assert_eq!(view(&state).scroll_offset, 80);

        // A 100-notch swipe queues without moving the viewport: no redraw per
        // event.
        for _ in 0..100 {
            state.push_wheel(WHEEL_LINES_PER_NOTCH);
        }
        assert_eq!(view(&state).pending_wheel, MAX_PENDING_WHEEL);

        // One flush moves at most a viewport-capped step.
        assert!(state.flush_wheel());
        assert!(view(&state).scroll_offset > 80);
        assert!(view(&state).scroll_offset <= 80 + 12);
    }

    #[test]
    fn cancel_wheel_drops_queued_momentum() {
        let mut state = ChatHistory::default();
        view_mut(&mut state).sync_scroll(200, 20);
        view_mut(&mut state).scroll_by(-100);
        state.push_wheel(WHEEL_LINES_PER_NOTCH);
        assert_eq!(view(&state).pending_wheel, WHEEL_LINES_PER_NOTCH);

        state.cancel_wheel();
        assert_eq!(view(&state).pending_wheel, 0);
        assert!(!state.flush_wheel());
    }

    #[test]
    fn resume_follow_snaps_to_bottom() {
        let mut state = ChatHistory::default();
        view_mut(&mut state).sync_scroll(100, 20);
        view_mut(&mut state).scroll_by(-10);
        assert!(!view(&state).follow_output);

        state.resume_follow();
        assert!(view(&state).follow_output);
        // The next render's `sync_scroll` snaps the offset to the bottom.
        assert_eq!(view_mut(&mut state).sync_scroll(100, 20), 80);
    }

    #[test]
    fn clear_selection_clears_active_selection() {
        let mut state = ChatHistory::default();
        let mut sel = Selection::new(TextPosition::new(0, 0));
        sel.cursor = TextPosition::new(0, 3);
        view_mut(&mut state).selection = Some(sel);
        assert!(state.has_active_selection());

        state.clear_selection();
        assert!(!state.has_active_selection());
    }
}
