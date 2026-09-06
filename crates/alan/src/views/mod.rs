//! Ratatui adapter for Alan.
//!
//! Core state stays UI-agnostic. This module owns the prompt editor
//! ([`UiState`]) and re-exports the `tui` view components. Transcript
//! rendering and scroll/selection state live in [`ChatHistory`].

pub(crate) mod component;
mod components;
pub mod selection;
pub mod theme;

use crate::core::completion::token;
use crate::core::{Command, CompletionRequest, ImageAttachment, Poll, SlashCommand};
use base64::Engine;
pub(crate) use components::{
    ChatHistory, ChatSnapshot, Header, PopupListv2, PopupSelected, PromptEditor, Status,
    StatusSnapshot,
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Position;
use ratatui::style::Style;
use tui_textarea::{CursorMove, CursorRenderMode, TextArea, WrapMode};

/// Only one custom highlight is ever active, so its priority is arbitrary.
const COMMAND_HIGHLIGHT_PRIORITY: u8 = 1;

pub struct UiState {
    editor: TextArea<'static>,
    /// True when something changed since last draw and a redraw is needed.
    dirty: bool,
    /// Images attached to the next prompt via clipboard paste.
    attachments: Vec<ImageAttachment>,
}

impl UiState {
    pub fn new() -> Self {
        let editor = Self::new_editor();

        Self {
            editor,
            dirty: true,
            attachments: Vec::new(),
        }
    }

    pub fn handle_event(&mut self, event: Event) -> Option<Command> {
        if matches!(&event, Event::Key(key) if key.kind != KeyEventKind::Press) {
            return None;
        }
        // if let Event::Key(key) = &event
        //     && completion.is_open()
        //     && let Some(action) = PopupAction::of(*key, completion.selected_item())
        // {
        //     return self.apply_completion_popup(action, completion);
        // }
        self.handle_editor_event(event)
    }

    fn handle_editor_event(&mut self, event: Event) -> Option<Command> {
        let command = match event {
            Event::Key(key) if key.code == KeyCode::Esc && !self.attachments.is_empty() => {
                self.attachments.pop();
                self.dirty = true;
                None
            }
            Event::Key(key) if is_multiline_enter(key) => {
                self.editor.insert_newline();
                self.dirty = true;
                None
            }
            Event::Key(key) if key.code == KeyCode::Enter => self.submit_editor_or_accept(),
            Event::Key(key)
                if key.code == KeyCode::Char('c')
                    && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                Some(Command::Interrupt)
            }
            Event::Key(key)
                if key.code == KeyCode::Char('v')
                    && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                // Bracketed paste only delivers text; images never arrive as
                // an `Event::Paste`, so attaching needs an explicit trigger.
                if !self.try_clipboard_image() {
                    // No image on the clipboard: fall back to pasting text.
                    let text = arboard::Clipboard::new().and_then(|mut c| c.get_text());
                    if let Ok(text) = text
                        && !text.is_empty()
                    {
                        self.editor.insert_str(text);
                        self.dirty = true;
                    }
                }
                None
            }
            Event::Key(key)
                if key.code == KeyCode::Char('u')
                    && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.editor.delete_line_by_head();
                self.dirty = true;
                None
            }
            Event::Key(key)
                if key.code == KeyCode::Char('z')
                    && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.editor.undo();
                self.dirty = true;
                None
            }
            Event::Paste(text) => {
                self.editor.insert_str(text);
                self.dirty = true;
                None
            }
            // Escape with no pending attachments falls through to the editor,
            // which ignores it (selection-clearing now lives in `ChatHistory`).
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
        // Only the first line can be a command, so the rest of the buffer is
        // never read. More than one line is not a command at all.
        let [line] = self.editor.lines() else {
            return;
        };
        if SlashCommand::parse(line).is_none() {
            return;
        }
        // `custom_highlight` ranges are byte offsets.
        let end = line.find(char::is_whitespace).unwrap_or(line.len());
        self.editor.custom_highlight(
            ((0, 0), (0, end)),
            Style::default().fg(theme::COMMAND_FG),
            COMMAND_HIGHLIGHT_PRIORITY,
        );
    }

    /// Overwrite the completed token with `replacement`.
    pub(crate) fn insert_completion(&mut self, replacement: &str, range: std::ops::Range<usize>) {
        let separate = self.needs_separator_after(range.end);
        self.replace_range(range, replacement);
        if separate {
            self.editor.insert_str(" ");
        }

        self.dirty = true;
    }

    /// Whether byte `at` on the cursor's line is not already followed by
    /// whitespace, so an accepted completion needs one adding.
    fn needs_separator_after(&self, at: usize) -> bool {
        let (row, _) = self.editor.cursor();
        self.editor
            .lines()
            .get(row)
            .and_then(|line| line.get(at..))
            .is_none_or(|rest| !rest.starts_with(char::is_whitespace))
    }

    /// Overwrite a byte range of the cursor's line. The editor addresses text
    /// by character column, so the range is converted on the way in.
    fn replace_range(&mut self, range: std::ops::Range<usize>, text: &str) {
        let (row, _) = self.editor.cursor();
        let Some(line) = self.editor.lines().get(row) else {
            return;
        };
        // The range was measured against this line, so it fits. Bail rather
        // than panic if that ever stops being true.
        let Some(before) = line.get(..range.start) else {
            return;
        };
        let Some(replaced) = line.get(range) else {
            return;
        };
        let start_col = before.chars().count();
        let chars = replaced.chars().count();
        self.editor
            .move_cursor(CursorMove::Jump(row as u16, start_col as u16));
        self.editor.delete_str(chars);
        self.editor.insert_str(text);
    }

    /// Convert a character-column index (as reported by `TextArea::cursor`)
    /// into a byte offset within `line`, clamped to the line length.
    fn char_offset(line: &str, col: usize) -> usize {
        line.char_indices()
            .nth(col)
            .map(|(i, _)| i)
            .unwrap_or(line.len())
    }

    /// The completion request for the token under the cursor, or `None` when
    /// the cursor sits in whitespace or before a token — which closes the
    /// popup. The trigger selects the backend; the range excludes it, so the
    /// trigger survives any replacement.
    pub fn completion_request(&self) -> Option<CompletionRequest> {
        let (row, col) = self.editor.cursor();
        let line = self.editor.lines().get(row)?;
        let cursor = Self::char_offset(line, col.min(line.chars().count()));
        let token = token::at(line, cursor)?;
        Some(CompletionRequest {
            trigger: token.trigger,
            pattern: line[token.range.clone()].to_owned(),
            range: token.range,
            row,
        })
    }

    fn submit_editor_or_accept(&mut self) -> Option<Command> {
        let text = self.editor_text();
        let images = std::mem::take(&mut self.attachments);
        self.editor = Self::new_editor();
        self.dirty = true;
        Some(Command::Submit { text, images })
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
        editor.set_undo_coalescing(true);
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

    /// Returns true when a redraw is needed, then clears the flag.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// Check the system clipboard for an image and add it as an attachment.
    /// Returns true when an image was attached.
    fn try_clipboard_image(&mut self) -> bool {
        let img = match arboard::Clipboard::new().and_then(|mut clipboard| clipboard.get_image()) {
            Ok(img) => img,
            Err(error) => {
                tracing::debug!(%error, "clipboard: no readable image");
                return false;
            }
        };
        if img.width == 0 || img.height == 0 {
            tracing::debug!("clipboard: ignoring zero-size image");
            return false;
        }
        let (w, h) = (img.width, img.height);
        let raw = img.into_owned_bytes();

        let Some(rgba) = image::RgbaImage::from_raw(w as u32, h as u32, raw.into_owned()) else {
            tracing::debug!(width = w, height = h, "clipboard: invalid image data");
            return false;
        };
        let mut png_buf = std::io::Cursor::new(Vec::new());
        if let Err(error) =
            image::DynamicImage::ImageRgba8(rgba).write_to(&mut png_buf, image::ImageFormat::Png)
        {
            tracing::debug!(%error, "clipboard: PNG encoding failed");
            return false;
        }
        let data = base64::engine::general_purpose::STANDARD.encode(png_buf.get_ref());

        self.attachments.push(ImageAttachment {
            name: format!("image-{}", self.attachments.len() + 1),
            mime_type: "image/png".into(),
            base64_data: data,
        });
        tracing::debug!(
            name = self.attachments.last().unwrap().name,
            "clipboard: image attached"
        );
        self.dirty = true;
        true
    }

    pub fn attachments(&self) -> &[ImageAttachment] {
        &self.attachments
    }

    pub fn attachment_height(&self) -> u16 {
        if self.attachments.is_empty() {
            0
        } else {
            3 + self.attachments.len() as u16
        }
    }
}

#[cfg(test)]
impl UiState {
    /// Test shim for the pre-completion call signature.
    fn handle_editor_event_for_test(&mut self, event: Event) -> Option<Command> {
        self.handle_event(event)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: crossterm::event::KeyCode) -> crossterm::event::Event {
        crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
            code,
            crossterm::event::KeyModifiers::NONE,
        ))
    }

    #[test]
    fn typing_inserts_immediately_and_escape_is_inert() {
        let mut state = UiState::new();
        state.handle_editor_event_for_test(key(crossterm::event::KeyCode::Char('h')));
        state.handle_editor_event_for_test(key(crossterm::event::KeyCode::Char('i')));
        state.handle_editor_event_for_test(key(crossterm::event::KeyCode::Esc));

        assert_eq!(state.editor_text(), "hi");
    }

    /// Foreground colour of each cell in the editor's first rendered row.
    ///
    /// The widget owns the highlight list, so painting is the only way to
    /// observe it. The area allows for wrapping: a viewport too short for the
    /// cursor scrolls the first row out.
    fn rendered_row(state: &UiState, width: u16) -> Vec<Option<ratatui::style::Color>> {
        use ratatui::widgets::Widget;

        let area = ratatui::layout::Rect::new(0, 0, width, theme::EDITOR_VISIBLE_LINES);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        (&state.editor).render(area, &mut buffer);
        (0..width).map(|x| buffer[(x, 0)].fg.into()).collect()
    }

    fn type_text(state: &mut UiState, text: &str) {
        for character in text.chars() {
            state.handle_editor_event_for_test(key(crossterm::event::KeyCode::Char(character)));
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
        state.handle_editor_event_for_test(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::SHIFT,
            ),
        ));
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
        state.handle_editor_event_for_test(key(crossterm::event::KeyCode::Backspace));

        let row = rendered_row(&state, 4);
        assert!(
            row.iter().all(|fg| *fg != Some(theme::COMMAND_FG)),
            "{row:?}"
        );
    }

    #[test]
    fn bracketed_paste_inserts_multiline_text() {
        let mut state = UiState::new();
        state.handle_editor_event_for_test(crossterm::event::Event::Paste("first\nsecond".into()));

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
    fn ctrl_z_undoes_a_typing_run_and_ctrl_r_redoes_it() {
        let mut state = UiState::new();
        type_text(&mut state, "hello");
        assert_eq!(state.editor_text(), "hello");

        state.handle_editor_event_for_test(ctrl('z'));
        assert_eq!(state.editor_text(), "");

        state.handle_editor_event_for_test(ctrl('r'));
        assert_eq!(state.editor_text(), "hello");
    }

    /// Terminals send Ctrl+U for Cmd+Delete, so it must clear the line rather
    /// than undo, which is what the widget binds it to by default.
    #[test]
    fn ctrl_u_deletes_to_start_of_line() {
        let mut state = UiState::new();
        for character in "one two".chars() {
            state.handle_editor_event_for_test(key(crossterm::event::KeyCode::Char(character)));
        }

        state.handle_editor_event_for_test(ctrl('u'));

        assert_eq!(state.editor_text(), "");
    }

    #[test]
    fn prompt_grows_with_wrapped_text_up_to_the_row_limit() {
        let mut state = UiState::new();
        assert_eq!(state.editor_rows(20), 1);

        for character in "aaaaa bbbbb ccccc ddddd".chars() {
            state.handle_editor_event_for_test(key(crossterm::event::KeyCode::Char(character)));
        }

        // Wrapping at width 12 needs more rows than the single logical line.
        assert!(state.editor_rows(12) > 1);
        assert!(state.editor_rows(12) <= theme::EDITOR_VISIBLE_LINES);
    }

    #[test]
    fn shift_enter_inserts_newline_without_submitting() {
        let mut state = UiState::new();
        state.handle_editor_event_for_test(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('a'),
                crossterm::event::KeyModifiers::NONE,
            ),
        ));

        let command = state.handle_editor_event_for_test(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::SHIFT,
            ),
        ));

        assert_eq!(command, None);
        assert_eq!(state.editor_text(), "a\n");
    }

    #[test]
    fn zed_shift_enter_aliases_insert_newline() {
        let mut state = UiState::new();
        state.handle_editor_event_for_test(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('a'),
                crossterm::event::KeyModifiers::NONE,
            ),
        ));

        let command = state.handle_editor_event_for_test(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::ALT,
            ),
        ));

        assert_eq!(command, None);
        assert_eq!(state.editor_text(), "a\n");
    }

    #[test]
    fn ctrl_j_inserts_newline() {
        let mut state = UiState::new();
        let command = state.handle_editor_event_for_test(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('j'),
                crossterm::event::KeyModifiers::CONTROL,
            ),
        ));

        assert_eq!(command, None);
        assert_eq!(state.editor_text(), "\n");
    }

    #[test]
    fn ctrl_m_inserts_newline() {
        let mut state = UiState::new();
        let command = state.handle_editor_event_for_test(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('m'),
                crossterm::event::KeyModifiers::CONTROL,
            ),
        ));

        assert_eq!(command, None);
        assert_eq!(state.editor_text(), "\n");
    }

    #[test]
    fn control_enter_inserts_newline() {
        let mut state = UiState::new();
        let command = state.handle_editor_event_for_test(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::CONTROL,
            ),
        ));

        assert_eq!(command, None);
        assert_eq!(state.editor_text(), "\n");
    }

    #[test]
    fn submit_marks_ui_dirty_before_agent_response() {
        let mut state = UiState::new();
        assert!(state.take_dirty());

        state.handle_editor_event_for_test(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('h'),
                crossterm::event::KeyModifiers::NONE,
            ),
        ));
        assert!(state.take_dirty());
        assert!(!state.take_dirty());

        let command = state.handle_editor_event_for_test(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::NONE,
            ),
        ));

        assert_eq!(
            command,
            Some(Command::Submit {
                text: "h".into(),
                images: vec![]
            })
        );
        assert!(state.take_dirty());
    }

    #[test]
    fn escape_removes_last_attachment_before_clearing_input() {
        let mut state = UiState::new();
        state.editor.insert_str("hi");
        state.attachments.push(ImageAttachment {
            name: "image-1".into(),
            mime_type: "image/png".into(),
            base64_data: "aGVsbG8=".into(),
        });
        state.attachments.push(ImageAttachment {
            name: "image-2".into(),
            mime_type: "image/png".into(),
            base64_data: "d29ybGQ=".into(),
        });

        assert_eq!(
            state.handle_editor_event_for_test(key(crossterm::event::KeyCode::Esc)),
            None
        );
        assert_eq!(state.attachments.len(), 1);
        assert_eq!(state.attachments[0].name, "image-1");
        assert_eq!(state.editor_text(), "hi");

        assert_eq!(
            state.handle_editor_event_for_test(key(crossterm::event::KeyCode::Esc)),
            None
        );
        assert!(state.attachments.is_empty());
        assert_eq!(state.editor_text(), "hi");
        assert!(state.take_dirty());

        // No attachments left: Esc is ignored again, as before.
        assert_eq!(
            state.handle_editor_event_for_test(key(crossterm::event::KeyCode::Esc)),
            None
        );
        assert_eq!(state.editor_text(), "hi");
    }
}
