//! Ratatui adapter for Alan.
//!
//! Core state stays UI-agnostic. This module owns the prompt editor
//! ([`UiState`]) and re-exports the `tui` view components. Transcript
//! rendering and scroll/selection state live in [`ChatHistory`].

pub(crate) mod component;
mod components;
pub mod selection;
pub mod theme;

use crate::core::{
    Accept, Action, Command, CompletionController, CompletionItem, ImageAttachment, Poll,
    SlashCommand,
};
use base64::Engine;
pub(crate) use components::{
    ChatHistory, ChatSnapshot, Footer, Header, PopupList, PopupStatus, Status, StatusSnapshot,
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

    pub fn apply(&mut self, action: Action) -> Option<Command> {
        // `apply` only serves the sparse mapped-action path plus the
        // pre-completion key shim. Main editor input flows through
        // `handle_event` into the `TextArea`, so the editor arms below are
        // unreachable in production; they stay for tests. Scroll and wheel
        // actions now belong to `ChatHistory`.
        let command = match action {
            Action::Interrupt => Some(Command::Interrupt),
            Action::TogglePlanMode => Some(Command::TogglePlanMode),
            Action::Resize => None,
            Action::Submit => {
                let images = std::mem::take(&mut self.attachments);
                Some(Command::Submit {
                    text: self.editor_text(),
                    images,
                })
            }
            Action::ClearInput => {
                // Esc first removes the newest attachment; with none pending
                // it clears the editor as before.
                if self.attachments.pop().is_some() {
                    self.dirty = true;
                    None
                } else {
                    self.editor = Self::new_editor();
                    self.dirty = true;
                    Some(Command::Cancel)
                }
            }
            Action::Backspace | Action::Insert(_) | Action::Paste(_) => None,
            Action::PasteOrAttachImage => {
                if self.try_clipboard_image() {
                    None
                } else {
                    // No image on the clipboard: fall back to pasting text.
                    match arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
                        Ok(text) if !text.is_empty() => self.apply(Action::Paste(text)),
                        _ => None,
                    }
                }
            }
            Action::ScrollUp
            | Action::ScrollDown
            | Action::MouseScrollUp
            | Action::MouseScrollDown => None,
        };
        self.dirty = true;
        command
    }

    pub fn handle_event(
        &mut self,
        event: Event,
        completion: &mut CompletionController,
    ) -> Option<Command> {
        if matches!(&event, Event::Key(key) if key.kind != KeyEventKind::Press) {
            return None;
        }
        if let Event::Key(key) = &event
            && completion.is_open()
            && let Some(action) = PopupAction::of(*key, completion.selected_item())
        {
            return self.apply_completion_popup(action, completion);
        }
        self.handle_editor_event(event, completion)
    }

    fn handle_editor_event(
        &mut self,
        event: Event,
        completion: &mut CompletionController,
    ) -> Option<Command> {
        let command = match event {
            Event::Key(key) if key.code == KeyCode::Esc && !self.attachments.is_empty() => {
                self.attachments.pop();
                self.dirty = true;
                None
            }
            Event::Key(key)
                if key.code == KeyCode::BackTab
                    || (key.code == KeyCode::Tab
                        && key.modifiers.contains(KeyModifiers::SHIFT)) =>
            {
                completion.dismiss();
                self.apply(Action::TogglePlanMode)
            }
            Event::Key(key) if is_multiline_enter(key) => {
                self.editor.insert_newline();
                self.dirty = true;
                self.sync_completion(completion);
                None
            }
            Event::Key(key) if key.code == KeyCode::Enter => {
                completion.dismiss();
                self.submit_editor_or_accept()
            }
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
                        self.sync_completion(completion);
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
                self.sync_completion(completion);
                None
            }
            Event::Key(key)
                if key.code == KeyCode::Char('z')
                    && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.editor.undo();
                self.dirty = true;
                self.sync_completion(completion);
                None
            }
            Event::Paste(text) => {
                self.editor.insert_str(text);
                self.dirty = true;
                self.sync_completion(completion);
                None
            }
            // Escape with no pending attachments falls through to the editor,
            // which ignores it (selection-clearing now lives in `ChatHistory`).
            event => {
                self.editor.input(event);
                self.dirty = true;
                self.sync_completion(completion);
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

    /// Carry out what [`PopupAction::of`] decided. Only accepting can finish
    /// the input, so only accepting can produce a command.
    fn apply_completion_popup(
        &mut self,
        action: PopupAction,
        completion: &mut CompletionController,
    ) -> Option<Command> {
        self.dirty = true;
        match action {
            PopupAction::Move(delta) => {
                completion.move_selection(delta);
                None
            }
            PopupAction::Dismiss => {
                completion.dismiss();
                None
            }
            PopupAction::Take { submit } => {
                let (item, range) = completion.accept()?;
                self.insert_completion(&item.replacement, range);
                // Taking an item can complete a command name, and the editor
                // event that would otherwise restyle the line never runs.
                self.sync_command_highlight();
                submit.then(|| self.submit_editor_or_accept()).flatten()
            }
        }
    }

    /// Overwrite the completed token with `replacement`.
    fn insert_completion(&mut self, replacement: &str, range: std::ops::Range<usize>) {
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

    /// Which backend answers, and over what text, is the controller's call.
    fn sync_completion(&mut self, completion: &mut CompletionController) {
        let (row, col) = self.editor.cursor();
        let line = self.editor.lines().get(row).map_or("", String::as_str);
        let cursor = Self::char_offset(line, col.min(line.chars().count()));
        completion.sync(line, cursor, row);
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

impl Default for UiState {
    fn default() -> Self {
        Self::new()
    }
}

/// A controller over a fixed index, so tests never touch the filesystem.
#[cfg(test)]
fn completion_with(index: &[&str]) -> CompletionController {
    use crate::core::completion::Paths;

    CompletionController::new(vec![Box::new(Paths::with_index(
        index.iter().map(|path| (*path).to_owned()).collect(),
    ))])
}

#[cfg(test)]
fn completion_with_commands() -> CompletionController {
    use crate::core::completion::Commands;

    CompletionController::new(vec![Box::new(Commands::default())])
}

#[cfg(test)]
impl UiState {
    /// Test shim for the pre-completion call signature.
    fn handle_editor_event_for_test(&mut self, event: Event) -> Option<Command> {
        let mut completion = completion_with(&[]);
        self.handle_event(event, &mut completion)
    }
}

/// What an open completion popup does with a key.
///
/// Decided before anything is mutated, so [`PopupAction::of`] returning `None`
/// is the whole answer to "the editor should see this key" — nothing further
/// down has to report back that it declined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PopupAction {
    Move(isize),
    /// Put the selected item in the buffer, then submit if `submit`.
    Take {
        submit: bool,
    },
    Dismiss,
}

impl PopupAction {
    /// `None` leaves the key to the editor.
    fn of(key: KeyEvent, selected: Option<&CompletionItem>) -> Option<Self> {
        match key.code {
            KeyCode::Up => Some(Self::Move(-1)),
            KeyCode::Down => Some(Self::Move(1)),
            KeyCode::Esc => Some(Self::Dismiss),
            KeyCode::Enter | KeyCode::Tab if key.modifiers.is_empty() => {
                let item = selected?;
                Some(Self::Take {
                    submit: key.code == KeyCode::Enter && item.accept == Accept::Complete,
                })
            }
            _ => None,
        }
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

    /// Completion syncs against the text after the paste, not before it.
    #[test]
    fn pasting_a_mention_opens_the_popup() {
        let mut state = UiState::new();
        let mut completion = completion_with(&["alpha.txt"]);

        state.handle_event(Event::Paste("@alp".into()), &mut completion);

        assert_eq!(state.editor_text(), "@alp");
        assert!(completion.is_open());
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

    /// Drive the editor with a real completion: typing `@` opens the popup
    /// and accepting replaces the token without submitting.
    #[test]
    fn at_completion_opens_accepts_and_replaces_token() {
        let mut state = UiState::new();
        let mut completion = completion_with(&["something.txt"]);

        for character in "@som".chars() {
            state.handle_event(
                key(crossterm::event::KeyCode::Char(character)),
                &mut completion,
            );
        }
        assert!(completion.is_open());

        let command = state.handle_event(key(KeyCode::Enter), &mut completion);

        assert_eq!(command, None);
        assert_eq!(state.editor_text(), "@something.txt ");
        assert!(!completion.is_open());
    }

    /// An open popup showing no matches has nothing to accept, so Enter is the
    /// editor's and submits the line.
    #[test]
    fn enter_submits_when_the_popup_has_no_matches() {
        let mut state = UiState::new();
        let mut completion = completion_with(&["alpha.txt"]);

        for character in "@zzz".chars() {
            state.handle_event(key(KeyCode::Char(character)), &mut completion);
        }
        assert!(completion.is_open());
        assert_eq!(completion.item_count(), 0);

        let command = state.handle_event(key(KeyCode::Enter), &mut completion);

        assert!(matches!(
            command,
            Some(Command::Submit { text, .. }) if text == "@zzz"
        ));
    }

    fn plain(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Only the `accept` field decides anything, so the text is left empty.
    fn item(accept: Accept) -> CompletionItem {
        CompletionItem {
            display: String::new(),
            replacement: String::new(),
            accept,
        }
    }

    /// Only Enter on a whole input submits. Tab never does, and neither does
    /// Enter on a path, which is only ever part of a prompt.
    #[test]
    fn only_enter_on_a_whole_input_submits() {
        let command = item(Accept::Complete);
        assert_eq!(
            PopupAction::of(plain(KeyCode::Enter), Some(&command)),
            Some(PopupAction::Take { submit: true })
        );
        assert_eq!(
            PopupAction::of(plain(KeyCode::Tab), Some(&command)),
            Some(PopupAction::Take { submit: false })
        );

        let path = item(Accept::Insert);
        assert_eq!(
            PopupAction::of(plain(KeyCode::Enter), Some(&path)),
            Some(PopupAction::Take { submit: false })
        );
    }

    /// Keys the popup declines stay the editor's, which is what keeps Enter
    /// submitting, Tab indenting, and Shift+Tab toggling plan mode.
    #[test]
    fn the_popup_declines_keys_it_cannot_act_on() {
        // Nothing selected leaves the accepting keys to the editor.
        assert_eq!(PopupAction::of(plain(KeyCode::Enter), None), None);
        assert_eq!(PopupAction::of(plain(KeyCode::Tab), None), None);

        let selected = item(Accept::Complete);
        let shift_tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT);
        assert_eq!(PopupAction::of(shift_tab, Some(&selected)), None);

        // Dismissing never depends on there being something to take.
        assert_eq!(
            PopupAction::of(plain(KeyCode::Esc), None),
            Some(PopupAction::Dismiss)
        );
    }

    /// A `/` opening a continuation line is prose, not a command, so the popup
    /// stays shut and the text submitted is the text that was typed.
    #[test]
    fn a_slash_on_a_later_line_is_left_alone() {
        let mut state = UiState::new();
        let mut completion = completion_with_commands();

        for character in "hello".chars() {
            state.handle_event(key(KeyCode::Char(character)), &mut completion);
        }
        state.handle_event(
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
            &mut completion,
        );
        for character in "/he".chars() {
            state.handle_event(key(KeyCode::Char(character)), &mut completion);
        }
        assert!(!completion.is_open());

        let command = state.handle_event(key(KeyCode::Enter), &mut completion);

        assert!(matches!(
            command,
            Some(Command::Submit { text, .. }) if text == "hello\n/he"
        ));
    }

    /// Picking a command is the whole input, so one Enter runs it.
    #[test]
    fn slash_completion_runs_on_a_single_enter() {
        let mut state = UiState::new();
        let mut completion = completion_with_commands();

        for character in "/he".chars() {
            state.handle_event(key(KeyCode::Char(character)), &mut completion);
        }
        assert!(completion.is_open());

        let command = state.handle_event(key(KeyCode::Enter), &mut completion);

        let Some(Command::Submit { text, .. }) = command else {
            panic!("expected a submit, got {command:?}");
        };
        // The accepted name carries a trailing separator, so it has to parse
        // with one.
        assert_eq!(SlashCommand::parse(&text), Some(SlashCommand::Help));
        assert!(!completion.is_open());
    }

    /// Tab completes the name without running it, which is what leaves room to
    /// type an argument after a command that grows one.
    #[test]
    fn tab_completes_a_command_without_running_it() {
        let mut state = UiState::new();
        let mut completion = completion_with_commands();

        for character in "/he".chars() {
            state.handle_event(key(KeyCode::Char(character)), &mut completion);
        }
        let command = state.handle_event(key(KeyCode::Tab), &mut completion);

        assert_eq!(command, None);
        assert_eq!(state.editor_text(), "/help ");
    }

    /// The `runs` flag belongs to the backend that offered the item, so a path
    /// accepted inside a command line inserts itself and nothing more.
    #[test]
    fn accepting_a_path_inside_a_command_does_not_run_the_command() {
        let mut state = UiState::new();
        let mut completion = completion_with(&["src/main.rs"]);

        for character in "/plan @src".chars() {
            state.handle_event(key(KeyCode::Char(character)), &mut completion);
        }
        assert!(completion.is_open());

        let command = state.handle_event(key(KeyCode::Enter), &mut completion);

        assert_eq!(command, None, "accepting a mention must not run /plan");
        // The line parses as `/plan`, so the flag rather than the text is what
        // keeps it from running.
        assert_eq!(state.editor_text(), "/plan @src/main.rs ");
    }

    /// `TextArea::cursor()` reports columns in characters while the line is
    /// sliced by byte offset, so a multi-byte character between `@` and the
    /// cursor must not put the two out of step.
    #[test]
    fn at_completion_after_multibyte_char_does_not_panic() {
        let mut state = UiState::new();
        let mut completion = completion_with(&["éx.txt"]);

        // `@` preceded by other text and followed by a 2-byte character,
        // then more text — a common mid-sentence use of `@` mentions.
        for character in "abc @éx".chars() {
            state.handle_event(
                key(crossterm::event::KeyCode::Char(character)),
                &mut completion,
            );
        }
        assert!(completion.is_open());

        // Accepting should replace the whole `@éx` token without panicking
        // and without eating the `abc ` that precedes it.
        let command = state.handle_event(key(KeyCode::Enter), &mut completion);

        assert_eq!(command, None);
        assert_eq!(state.editor_text(), "abc @éx.txt ");
        assert!(!completion.is_open());
    }

    #[test]
    fn at_completion_navigation_and_escape() {
        let mut state = UiState::new();
        let mut completion = completion_with(&["a.txt", "b.txt"]);

        state.handle_event(key(KeyCode::Char('@')), &mut completion);
        assert_eq!(completion.selected(), 0);

        state.handle_event(key(KeyCode::Down), &mut completion);
        assert_eq!(completion.selected(), 1);

        state.handle_event(key(KeyCode::Esc), &mut completion);
        assert!(!completion.is_open());

        // Popup closed: Enter submits again.
        let command = state.handle_event(key(KeyCode::Enter), &mut completion);
        assert_eq!(
            command,
            Some(Command::Submit {
                text: "@".into(),
                images: vec![]
            })
        );
    }

    #[test]
    fn deleting_the_at_closes_the_popup() {
        let mut state = UiState::new();
        let mut completion = completion_with(&["a.txt"]);

        state.handle_event(key(KeyCode::Char('@')), &mut completion);
        assert!(completion.is_open());

        state.handle_event(key(KeyCode::Backspace), &mut completion);
        assert!(!completion.is_open());
    }

    /// A directory is a reference in its own right: accepting one inserts it
    /// and finishes.
    #[test]
    fn accepting_a_directory_inserts_it_and_closes() {
        let mut state = UiState::new();
        let mut completion = completion_with(&["src/"]);

        state.handle_event(key(KeyCode::Char('@')), &mut completion);
        state.handle_event(key(KeyCode::Enter), &mut completion);

        assert_eq!(state.editor_text(), "@src/ ");
        assert!(!completion.is_open());
    }

    /// The separator is what keeps the next keystroke out of the token.
    #[test]
    fn typing_after_accepting_is_prose_not_another_mention() {
        let mut state = UiState::new();
        let mut completion = completion_with(&["src/"]);

        state.handle_event(key(KeyCode::Char('@')), &mut completion);
        state.handle_event(key(KeyCode::Enter), &mut completion);
        state.handle_event(key(KeyCode::Char('h')), &mut completion);

        assert_eq!(state.editor_text(), "@src/ h");
        assert!(!completion.is_open());
    }

    /// Text already follows the token, so it does not need separating twice.
    #[test]
    fn accepting_mid_sentence_does_not_double_the_space() {
        let mut state = UiState::new();
        let mut completion = completion_with(&["src/main.rs"]);

        for character in "@mai and more".chars() {
            state.handle_event(key(KeyCode::Char(character)), &mut completion);
        }
        // Back inside the `@mai` token: `@mai| and more`.
        for _ in 0.." and more".len() {
            state.handle_event(key(KeyCode::Left), &mut completion);
        }
        state.handle_event(key(KeyCode::Enter), &mut completion);

        assert_eq!(state.editor_text(), "@src/main.rs and more");
    }

    /// Accepting at `@fo|o` replaces the whole token, not the prefix before
    /// the cursor, so no trailing `o` survives.
    #[test]
    fn accepting_completion_with_cursor_inside_token_replaces_whole_token() {
        let mut state = UiState::new();
        // The path must match the typed prefix (`foo`) yet differ from it,
        // so a leftover suffix would show.
        let mut completion = completion_with(&["foobar"]);

        for character in "@foo".chars() {
            state.handle_event(
                key(crossterm::event::KeyCode::Char(character)),
                &mut completion,
            );
        }
        assert!(completion.is_open());

        // Move the cursor left twice: `@fo|o`.
        state.handle_event(key(KeyCode::Left), &mut completion);
        state.handle_event(key(KeyCode::Left), &mut completion);
        assert_eq!(state.editor_text(), "@foo");

        let command = state.handle_event(key(KeyCode::Enter), &mut completion);

        assert_eq!(command, None);
        assert_eq!(state.editor_text(), "@foobar ");
        assert!(!completion.is_open());
    }

    /// Same regression, but with a multi-byte character inside the token,
    /// where byte/character-column confusion is most likely to bite.
    #[test]
    fn accepting_completion_with_cursor_inside_multibyte_token() {
        let mut state = UiState::new();
        let mut completion = completion_with(&["éfoobar"]);

        for character in "@éfoo".chars() {
            state.handle_event(
                key(crossterm::event::KeyCode::Char(character)),
                &mut completion,
            );
        }
        assert!(completion.is_open());

        // Move the cursor left twice: `@éfo|o`.
        state.handle_event(key(KeyCode::Left), &mut completion);
        state.handle_event(key(KeyCode::Left), &mut completion);
        assert_eq!(state.editor_text(), "@éfoo");

        let command = state.handle_event(key(KeyCode::Enter), &mut completion);

        assert_eq!(command, None);
        assert_eq!(state.editor_text(), "@éfoobar ");
        assert!(!completion.is_open());
    }

    /// Accepting at the end of the token (the common path) still works.
    #[test]
    fn accepting_completion_at_token_end_still_replaces() {
        let mut state = UiState::new();
        let mut completion = completion_with(&["foobar"]);

        for character in "@foo".chars() {
            state.handle_event(
                key(crossterm::event::KeyCode::Char(character)),
                &mut completion,
            );
        }

        let command = state.handle_event(key(KeyCode::Enter), &mut completion);

        assert_eq!(command, None);
        assert_eq!(state.editor_text(), "@foobar ");
        assert!(!completion.is_open());
    }
}
