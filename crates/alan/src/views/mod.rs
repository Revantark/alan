//! Ratatui adapter for Alan.
//!
//! Core state stays UI-agnostic. This module owns ratatui-facing editor and
//! scroll state so another frontend can map its own events to [`Action`].

mod component;
mod components;
pub mod selection;
mod theme;

use crate::core::{Action, Command, Controller, Overlay, Poll, SlashCommand};
use components::{Chat, Footer, Header, LoginOverlay};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::Style;
use selection::{Selection, TextPosition};
use tui_textarea::{CursorRenderMode, TextArea, WrapMode};

use std::time::Instant;

/// Only one custom highlight is ever active, so its priority is arbitrary.
const COMMAND_HIGHLIGHT_PRIORITY: u8 = 1;

pub struct UiState {
    /// Login prompt input. Main editor uses [`TextArea`].
    input: String,
    editor: TextArea<'static>,
    /// Current rendered top line.
    scroll_offset: usize,
    /// Desired top line. Kept equal to `scroll_offset` for immediate input response.
    scroll_target: usize,
    /// Keep viewport pinned to newest content while true.
    follow_output: bool,
    /// Last rendered viewport/content bounds. Input events use these bounds
    /// before next render clamps them again.
    viewport_height: usize,
    max_scroll: usize,
    /// Last rendered content area of chat
    chat_area: Rect,
    /// Active text selection in transcript
    selection: Option<Selection>,
    /// Last click timestamp and position for double-click detection
    last_click: Option<(Instant, u16, u16)>,
    /// True when something changed since last draw and a redraw is needed.
    dirty: bool,
}

impl UiState {
    pub fn new() -> Self {
        let editor = Self::new_editor();

        Self {
            input: String::new(),
            editor,
            scroll_offset: 0,
            scroll_target: 0,
            follow_output: true,
            viewport_height: 0,
            max_scroll: 0,
            chat_area: Rect::default(),
            selection: None,
            last_click: None,
            dirty: true,
        }
    }

    pub fn apply(&mut self, action: Action, login_selection_active: bool) -> Option<Command> {
        let command = match action {
            Action::Interrupt => {
                self.input.clear();
                Some(Command::Interrupt)
            }
            Action::TogglePlanMode => Some(Command::TogglePlanMode),
            Action::Resize => None,
            Action::Submit => {
                self.follow_output = true;
                self.scroll_target = self.max_scroll;
                Some(Command::Submit(std::mem::take(&mut self.input)))
            }
            Action::ClearInput => {
                self.input.clear();
                Some(Command::Cancel)
            }
            Action::Backspace => {
                self.input.pop();
                None
            }
            Action::Insert(character) => {
                self.input.push(character);
                None
            }
            Action::Paste(text) => {
                self.input.push_str(&text);
                None
            }
            Action::ScrollUp => {
                if login_selection_active {
                    Some(Command::MoveLoginSelection(-1))
                } else {
                    self.scroll_by(-(self.viewport_height.max(1) as isize));
                    None
                }
            }
            Action::ScrollDown => {
                if login_selection_active {
                    Some(Command::MoveLoginSelection(1))
                } else {
                    self.scroll_by(self.viewport_height.max(1) as isize);
                    None
                }
            }
            Action::MouseScrollUp => {
                self.scroll_by(-3);
                None
            }
            Action::MouseScrollDown => {
                self.scroll_by(3);
                None
            }
        };
        self.dirty = true;
        command
    }

    pub fn handle_editor_event(
        &mut self,
        event: Event,
        rendered_lines: &[ratatui::text::Line<'static>],
    ) -> Option<Command> {
        let command = match event {
            Event::Key(key) if key.kind != KeyEventKind::Press => None,
            // Clear selection on Escape
            Event::Key(key) if key.code == KeyCode::Esc && self.has_active_selection() => {
                self.selection = None;
                self.dirty = true;
                None
            }
            Event::Key(key)
                if key.code == KeyCode::BackTab
                    || (key.code == KeyCode::Tab
                        && key.modifiers.contains(KeyModifiers::SHIFT)) =>
            {
                self.apply(Action::TogglePlanMode, false)
            }
            Event::Key(key) if is_multiline_enter(key) => {
                self.editor.insert_newline();
                self.dirty = true;
                None
            }
            Event::Key(key) if key.code == KeyCode::Enter => self.submit_editor_or_accept(),
            Event::Key(key) if key.code == KeyCode::PageUp => self.apply(Action::ScrollUp, false),
            Event::Key(key) if key.code == KeyCode::PageDown => {
                self.apply(Action::ScrollDown, false)
            }
            Event::Key(key)
                if key.code == KeyCode::Char('c')
                    && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                Some(Command::Interrupt)
            }
            // Terminals send Ctrl+U for Cmd+Delete, but the widget binds it to
            // undo, so Cmd+Delete would undo instead of clearing the line. Its
            // keymap is a match arm rather than a table, so interception is the
            // only way to change it.
            Event::Key(key)
                if key.code == KeyCode::Char('u')
                    && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.editor.delete_line_by_head();
                self.dirty = true;
                None
            }
            // Undo lost its binding to the line clear above, so it moves here.
            // Terminals cannot report Cmd, so Cmd+Z is not available. Redo
            // stays on the widget's native Ctrl+R.
            Event::Key(key)
                if key.code == KeyCode::Char('z')
                    && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.editor.undo();
                self.dirty = true;
                None
            }
            Event::Mouse(mouse) => self.handle_mouse_event(mouse, rendered_lines),
            // Bracketed paste arrives as its own event. The widget's crossterm
            // conversion drops it, so insert the text directly.
            Event::Paste(text) => {
                self.editor.insert_str(text);
                self.dirty = true;
                None
            }
            event => {
                self.editor.input(event);
                self.dirty = true;
                None
            }
        };
        self.sync_command_highlight();
        command
    }

    fn sync_command_highlight(&mut self) {
        // Highlights accumulate, so the previous one has to go first.
        self.editor.clear_custom_highlight();
        let text = self.editor_text();
        if SlashCommand::parse(&text).is_none() {
            return;
        }
        // `custom_highlight` ranges are byte offsets.
        let end = text.find(char::is_whitespace).unwrap_or(text.len());
        self.editor.custom_highlight(
            ((0, 0), (0, end)),
            Style::default().fg(theme::COMMAND_FG),
            COMMAND_HIGHLIGHT_PRIORITY,
        );
    }

    fn handle_mouse_event(
        &mut self,
        mouse: MouseEvent,
        rendered_lines: &[ratatui::text::Line<'static>],
    ) -> Option<Command> {
        match mouse.kind {
            MouseEventKind::ScrollUp => self.apply(Action::MouseScrollUp, false),
            MouseEventKind::ScrollDown => self.apply(Action::MouseScrollDown, false),
            MouseEventKind::Down(MouseButton::Left) => {
                if self.is_mouse_in_chat(mouse.column, mouse.row) {
                    let now = Instant::now();
                    let is_double_click = self.last_click.is_some_and(|(t, c, r)| {
                        c == mouse.column
                            && r == mouse.row
                            && now.duration_since(t).as_millis() <= 500
                    });

                    if let Some(pos) = self.screen_to_text_pos(mouse.column, mouse.row) {
                        if is_double_click && pos.line < rendered_lines.len() {
                            let (start_col, end_col) =
                                selection::find_word_bounds_at(&rendered_lines[pos.line], pos.col);
                            let sel = Selection::new_word(pos, start_col, end_col);
                            self.selection = Some(sel);
                            self.last_click = None;
                            self.copy_selection(rendered_lines);
                        } else {
                            self.selection = Some(Selection::new(pos));
                            self.last_click = Some((now, mouse.column, mouse.row));
                        }
                        self.dirty = true;
                    }
                } else if self.selection.is_some() {
                    self.selection = None;
                    self.dirty = true;
                }
                None
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
                        sel.update_cursor(pos, rendered_lines);
                        self.dirty = true;
                    }
                }
                None
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(sel) = &mut self.selection {
                    sel.is_dragging = false;
                    if sel.is_empty() {
                        self.selection = None;
                    } else {
                        // Auto-copy on selection mouse release
                        self.copy_selection(rendered_lines);
                    }
                    self.dirty = true;
                }
                None
            }
            _ => None,
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

    pub fn has_active_selection(&self) -> bool {
        self.selection.as_ref().is_some_and(|s| !s.is_empty())
    }

    pub fn copy_selection(&mut self, lines: &[ratatui::text::Line<'static>]) {
        let Some(sel) = &self.selection else {
            return;
        };
        let text = selection::extract_selected_text(lines, sel);
        if !text.is_empty()
            && let Ok(mut clipboard) = arboard::Clipboard::new()
        {
            let _ = clipboard.set_text(text);
        }
    }

    pub fn selection(&self) -> Option<&Selection> {
        self.selection.as_ref()
    }

    pub fn set_chat_area(&mut self, area: Rect) {
        self.chat_area = area;
    }

    fn submit_editor_or_accept(&mut self) -> Option<Command> {
        self.follow_output = true;
        self.scroll_target = self.max_scroll;
        let text = self.editor_text();
        self.editor = Self::new_editor();
        self.dirty = true;
        Some(Command::Submit(text))
    }

    /// The prompt soft-wraps at word boundaries and grows up to
    /// [`theme::EDITOR_VISIBLE_LINES`] rows. The terminal owns the cursor, so
    /// the widget does not paint one of its own.
    fn new_editor() -> TextArea<'static> {
        let mut editor = TextArea::default();
        editor.set_style(Style::default().fg(theme::EDITOR_FG).bg(theme::EDITOR_BG));
        editor.set_cursor_line_style(Style::default());
        editor.set_wrap_mode(WrapMode::WordOrGlyph);
        editor.set_cursor_render_mode(CursorRenderMode::Hidden);
        editor.set_min_rows(1);
        editor.set_max_rows(theme::EDITOR_VISIBLE_LINES);
        editor
    }

    fn editor_text(&self) -> String {
        self.editor.lines().join("\n")
    }

    /// Consume a poll outcome. Manual scroll position survives streamed text.
    pub fn on_poll(&mut self, poll: Poll) {
        if !matches!(poll, Poll::Idle) {
            self.dirty = true;
        }
    }

    fn scroll_by(&mut self, delta: isize) {
        let current = if self.follow_output {
            self.max_scroll
        } else {
            self.scroll_target
        };
        self.scroll_target = if delta.is_negative() {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current.saturating_add(delta as usize).min(self.max_scroll)
        };
        self.scroll_offset = self.scroll_target;
        self.follow_output = self.scroll_offset == self.max_scroll;
        self.dirty = true;
    }

    /// Keep bottom-follow state synchronized on render ticks.
    pub fn tick(&mut self) {
        if self.follow_output {
            self.scroll_offset = self.max_scroll;
            self.scroll_target = self.max_scroll;
        }
    }

    pub(super) fn sync_scroll(&mut self, content_height: usize, viewport_height: usize) -> usize {
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

    pub(super) fn input(&self) -> &str {
        &self.input
    }

    pub(super) fn editor(&self) -> &TextArea<'static> {
        &self.editor
    }

    /// Rows the prompt needs at `width`, accounting for soft wrapping.
    ///
    /// Wrapped text occupies more rows than it has lines, so the footer cannot
    /// be sized from the line count alone.
    pub(super) fn editor_rows(&mut self, width: u16) -> u16 {
        self.editor.measure(width.max(1)).preferred_rows
    }

    pub(super) fn cursor_screen_position(&self) -> Option<Position> {
        self.editor.rendered_cursor_position()
    }

    pub(super) fn max_scroll(&self) -> usize {
        self.max_scroll
    }

    /// Returns true when a redraw is needed, then clears the flag.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }
}

impl Default for UiState {
    fn default() -> Self {
        Self::new()
    }
}

fn is_multiline_enter(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('\n' | '\r'))
        || (matches!(key.code, KeyCode::Char('j' | 'm'))
            && key.modifiers.contains(KeyModifiers::CONTROL))
        || (key.code == KeyCode::Enter
            && (key.modifiers.contains(KeyModifiers::SHIFT)
                || key.modifiers.contains(KeyModifiers::CONTROL)
                || key.modifiers.contains(KeyModifiers::ALT)))
}

#[derive(Debug, Default)]
pub struct AppView {
    header: Header,
    chat: Chat,
    footer: Footer,
    login: LoginOverlay,
}

impl AppView {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn render(&mut self, frame: &mut Frame, controller: &Controller, state: &mut UiState) {
        use component::Component;

        if controller.overlay() == Overlay::Login {
            frame.render_widget(ratatui::widgets::Clear, frame.area());
            self.login.render(frame, frame.area(), controller, state);
            return;
        }

        // Measure against the width the editor actually gets, not the frame's.
        let editor_width = frame.area().width.saturating_sub(theme::PROMPT_GUTTER);
        let [header_area, chat_area, footer_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(4 + state.editor_rows(editor_width)),
        ])
        .areas(frame.area());

        self.header.render(frame, header_area, controller, state);
        self.chat.render(frame, chat_area, controller, state);
        self.footer.render(frame, footer_area, controller, state);
    }

    pub fn lines(&self) -> &[ratatui::text::Line<'static>] {
        self.chat.lines()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follows_bottom_until_user_scrolls_up() {
        let mut state = UiState::new();
        assert_eq!(state.sync_scroll(100, 20), 80);

        state.scroll_by(-5);
        assert_eq!(state.scroll_target, 75);
        assert_eq!(state.scroll_offset, 75);
        assert!(!state.follow_output);
        state.tick();
        assert_eq!(state.scroll_offset, 75);

        assert_eq!(state.sync_scroll(120, 20), 75);
        assert!(!state.follow_output);
    }

    #[test]
    fn scrolling_to_bottom_restores_follow_mode() {
        let mut state = UiState::new();
        state.sync_scroll(100, 20);
        state.scroll_by(-10);
        state.scroll_by(10);

        assert_eq!(state.scroll_target, 80);
        assert!(state.follow_output);
        state.tick();
        assert!(state.follow_output);
        assert_eq!(state.sync_scroll(120, 20), 100);
    }

    #[test]
    fn content_shrink_clamps_manual_scroll() {
        let mut state = UiState::new();
        state.sync_scroll(100, 20);
        state.scroll_by(-10);

        assert_eq!(state.sync_scroll(30, 20), 10);
        assert_eq!(state.scroll_target, 10);
        assert!(state.follow_output);
    }

    #[test]
    fn scrolling_updates_rendered_offset_immediately() {
        let mut state = UiState::new();
        state.sync_scroll(100, 20);
        state.scroll_by(-10);

        assert_eq!(state.scroll_offset, 70);
        assert_eq!(state.scroll_target, 70);
        assert!(!state.follow_output);
    }

    fn key(code: crossterm::event::KeyCode) -> crossterm::event::Event {
        crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
            code,
            crossterm::event::KeyModifiers::NONE,
        ))
    }

    #[test]
    fn typing_inserts_immediately_and_escape_is_inert() {
        let mut state = UiState::new();
        state.handle_editor_event(key(crossterm::event::KeyCode::Char('h')), &[]);
        state.handle_editor_event(key(crossterm::event::KeyCode::Char('i')), &[]);
        state.handle_editor_event(key(crossterm::event::KeyCode::Esc), &[]);

        assert_eq!(state.editor_text(), "hi");
    }

    /// Foreground colour of each cell in the editor's first rendered row.
    ///
    /// The widget owns the highlight list, so painting is the only way to
    /// observe it. The area allows for wrapping: a viewport too short for the
    /// cursor scrolls the first row out.
    fn rendered_row(state: &UiState, width: u16) -> Vec<Option<ratatui::style::Color>> {
        use ratatui::widgets::Widget;

        let area = Rect::new(0, 0, width, theme::EDITOR_VISIBLE_LINES);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        (&state.editor).render(area, &mut buffer);
        (0..width).map(|x| buffer[(x, 0)].fg.into()).collect()
    }

    fn type_text(state: &mut UiState, text: &str) {
        for character in text.chars() {
            state.handle_editor_event(key(crossterm::event::KeyCode::Char(character)), &[]);
        }
    }

    #[test]
    fn known_command_is_highlighted_up_to_its_first_space() {
        let mut state = UiState::new();
        type_text(&mut state, "/plan now");

        let row = rendered_row(&state, 9);
        assert!(
            row[..5].iter().all(|fg| *fg == Some(theme::COMMAND_FG)),
            "{row:?}"
        );
        assert!(
            row[5..].iter().all(|fg| *fg != Some(theme::COMMAND_FG)),
            "{row:?}"
        );
    }

    /// The near misses are a leading space and a second line.
    #[test]
    fn highlight_matches_the_controller_on_near_misses() {
        let mut state = UiState::new();
        type_text(&mut state, " /plan");
        let row = rendered_row(&state, 6);
        assert!(
            row.iter().all(|fg| *fg != Some(theme::COMMAND_FG)),
            "leading space highlighted: {row:?}"
        );

        let mut state = UiState::new();
        type_text(&mut state, "/plan");
        state.handle_editor_event(
            crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::SHIFT,
            )),
            &[],
        );
        type_text(&mut state, "and this");
        let row = rendered_row(&state, 5);
        assert!(
            row.iter().all(|fg| *fg != Some(theme::COMMAND_FG)),
            "multiline highlighted: {row:?}"
        );
    }

    #[test]
    fn unknown_command_and_plain_text_are_not_highlighted() {
        for text in ["/pln", "plan"] {
            let mut state = UiState::new();
            type_text(&mut state, text);

            let row = rendered_row(&state, 4);
            assert!(
                row.iter().all(|fg| *fg != Some(theme::COMMAND_FG)),
                "{text} highlighted: {row:?}"
            );
        }
    }

    /// The highlight range is in bytes.
    #[test]
    fn highlight_survives_a_wide_character_after_the_command() {
        let mut state = UiState::new();
        type_text(&mut state, "/plan 日本");

        let row = rendered_row(&state, 20);
        assert!(
            row[..5].iter().all(|fg| *fg == Some(theme::COMMAND_FG)),
            "{row:?}"
        );
        assert!(
            row[5..].iter().all(|fg| *fg != Some(theme::COMMAND_FG)),
            "{row:?}"
        );
    }

    /// The highlight is clipped per wrapped row.
    #[test]
    fn highlight_survives_a_wrapped_argument() {
        let mut state = UiState::new();
        type_text(&mut state, "/plan aaaa bbbb cccc dddd");

        let row = rendered_row(&state, 12);
        assert!(
            row[..5].iter().all(|fg| *fg == Some(theme::COMMAND_FG)),
            "{row:?}"
        );
        assert!(
            row[5..].iter().all(|fg| *fg != Some(theme::COMMAND_FG)),
            "{row:?}"
        );
    }

    /// Highlights accumulate in the widget.
    #[test]
    fn highlight_clears_when_the_command_is_edited_away() {
        let mut state = UiState::new();
        type_text(&mut state, "/plan");
        state.handle_editor_event(key(crossterm::event::KeyCode::Backspace), &[]);

        let row = rendered_row(&state, 4);
        assert!(
            row.iter().all(|fg| *fg != Some(theme::COMMAND_FG)),
            "{row:?}"
        );
    }

    #[test]
    fn bracketed_paste_inserts_multiline_text() {
        let mut state = UiState::new();
        state.handle_editor_event(crossterm::event::Event::Paste("first\nsecond".into()), &[]);

        assert_eq!(state.editor_text(), "first\nsecond");
    }

    fn ctrl(code: char) -> crossterm::event::Event {
        crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char(code),
            crossterm::event::KeyModifiers::CONTROL,
        ))
    }

    /// Ctrl+U was the widget's undo binding before it was taken for the line
    /// clear, so undo and redo have to keep working from their new homes.
    #[test]
    fn ctrl_z_undoes_and_ctrl_r_redoes() {
        let mut state = UiState::new();
        for character in "hello".chars() {
            state.handle_editor_event(key(crossterm::event::KeyCode::Char(character)), &[]);
        }
        assert_eq!(state.editor_text(), "hello");

        state.handle_editor_event(ctrl('z'), &[]);
        assert_ne!(state.editor_text(), "hello");

        state.handle_editor_event(ctrl('r'), &[]);
        assert_eq!(state.editor_text(), "hello");
    }

    /// Terminals send Ctrl+U for Cmd+Delete, so it must clear the line rather
    /// than undo, which is what the widget binds it to by default.
    #[test]
    fn ctrl_u_deletes_to_start_of_line() {
        let mut state = UiState::new();
        for character in "one two".chars() {
            state.handle_editor_event(key(crossterm::event::KeyCode::Char(character)), &[]);
        }

        state.handle_editor_event(ctrl('u'), &[]);

        assert_eq!(state.editor_text(), "");
    }

    #[test]
    fn prompt_grows_with_wrapped_text_up_to_the_row_limit() {
        let mut state = UiState::new();
        assert_eq!(state.editor_rows(20), 1);

        for character in "aaaaa bbbbb ccccc ddddd".chars() {
            state.handle_editor_event(key(crossterm::event::KeyCode::Char(character)), &[]);
        }

        // Wrapping at width 12 needs more rows than the single logical line.
        assert!(state.editor_rows(12) > 1);
        assert!(state.editor_rows(12) <= theme::EDITOR_VISIBLE_LINES);
    }

    #[test]
    fn shift_enter_inserts_newline_without_submitting() {
        let mut state = UiState::new();
        state.handle_editor_event(
            crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('a'),
                crossterm::event::KeyModifiers::NONE,
            )),
            &[],
        );

        let command = state.handle_editor_event(
            crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::SHIFT,
            )),
            &[],
        );

        assert_eq!(command, None);
        assert_eq!(state.editor_text(), "a\n");
    }

    #[test]
    fn zed_shift_enter_aliases_insert_newline() {
        let mut state = UiState::new();
        state.handle_editor_event(
            crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('a'),
                crossterm::event::KeyModifiers::NONE,
            )),
            &[],
        );

        let command = state.handle_editor_event(
            crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::ALT,
            )),
            &[],
        );

        assert_eq!(command, None);
        assert_eq!(state.editor_text(), "a\n");
    }

    #[test]
    fn ctrl_j_inserts_newline() {
        let mut state = UiState::new();
        let command = state.handle_editor_event(
            crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('j'),
                crossterm::event::KeyModifiers::CONTROL,
            )),
            &[],
        );

        assert_eq!(command, None);
        assert_eq!(state.editor_text(), "\n");
    }

    #[test]
    fn ctrl_m_inserts_newline() {
        let mut state = UiState::new();
        let command = state.handle_editor_event(
            crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('m'),
                crossterm::event::KeyModifiers::CONTROL,
            )),
            &[],
        );

        assert_eq!(command, None);
        assert_eq!(state.editor_text(), "\n");
    }

    #[test]
    fn control_enter_inserts_newline() {
        let mut state = UiState::new();
        let command = state.handle_editor_event(
            crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::CONTROL,
            )),
            &[],
        );

        assert_eq!(command, None);
        assert_eq!(state.editor_text(), "\n");
    }

    #[test]
    fn submit_marks_ui_dirty_before_agent_response() {
        let mut state = UiState::new();
        assert!(state.take_dirty());

        state.handle_editor_event(
            crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('h'),
                crossterm::event::KeyModifiers::NONE,
            )),
            &[],
        );
        assert!(state.take_dirty());
        assert!(!state.take_dirty());

        let command = state.handle_editor_event(
            crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::NONE,
            )),
            &[],
        );

        assert_eq!(command, Some(Command::Submit("h".into())));
        assert!(state.take_dirty());
    }
}
