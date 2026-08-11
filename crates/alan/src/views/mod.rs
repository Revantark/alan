//! Ratatui adapter for Alan.
//!
//! Core state stays UI-agnostic. This module owns ratatui-facing editor and
//! scroll state so another frontend can map its own events to [`Action`].

mod component;
mod components;
mod theme;

use crate::core::{Action, Command, Controller, Overlay, Poll};
use components::{Chat, Footer, Header, LoginOverlay};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use edtui::{EditorEventHandler, EditorMode, EditorState, Lines};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position};

pub struct UiState {
    /// Login prompt input. Main editor uses [`EditorState`].
    input: String,
    editor: EditorState,
    editor_events: EditorEventHandler,
    /// Current rendered top line.
    scroll_offset: usize,
    /// Desired top line. Scroll input changes this immediately; animation
    /// moves `scroll_offset` toward it on render ticks.
    scroll_target: usize,
    /// Keep viewport pinned to newest content while true.
    follow_output: bool,
    /// Last rendered viewport/content bounds. Input events use these bounds
    /// before next render clamps them again.
    viewport_height: usize,
    max_scroll: usize,
    /// True when something changed since last draw and a redraw is needed.
    dirty: bool,
}

impl UiState {
    pub fn new() -> Self {
        let editor = Self::new_editor();

        Self {
            input: String::new(),
            editor,
            editor_events: EditorEventHandler::vim_mode(),
            scroll_offset: 0,
            scroll_target: 0,
            follow_output: true,
            viewport_height: 0,
            max_scroll: 0,
            dirty: true,
        }
    }

    pub fn apply(&mut self, action: Action, login_selection_active: bool) -> Option<Command> {
        let command = match action {
            Action::Interrupt => {
                self.input.clear();
                Some(Command::Interrupt)
            }
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

    /// Handle primary editor events and app-level shortcuts.
    pub fn handle_editor_event(&mut self, event: Event) -> Option<Command> {
        let command = match event {
            Event::Key(key) if key.kind != KeyEventKind::Press => None,
            Event::Key(key)
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && key.code == KeyCode::Char('c') =>
            {
                self.apply(Action::Interrupt, false)
            }
            Event::Key(key) if is_multiline_enter(key) => {
                if self.editor.mode != EditorMode::Insert {
                    self.editor.mode = EditorMode::Insert;
                }
                self.editor.execute(edtui::actions::LineBreak(1));
                self.dirty = true;
                None
            }
            Event::Key(key) if key.code == KeyCode::Enter => self.submit_editor_or_accept(),
            Event::Key(key) if key.code == KeyCode::Esc => {
                self.editor_events.on_event(event, &mut self.editor);
                self.dirty = true;
                None
            }
            Event::Key(key)
                if key.code == KeyCode::Char('/')
                    && key.modifiers == KeyModifiers::NONE
                    && self.editor.mode == EditorMode::Normal =>
            {
                self.editor.mode = EditorMode::Insert;
                self.editor_events.on_event(event, &mut self.editor);
                self.dirty = true;
                None
            }
            Event::Key(key) if key.code == KeyCode::PageUp => self.apply(Action::ScrollUp, false),
            Event::Key(key) if key.code == KeyCode::PageDown => {
                self.apply(Action::ScrollDown, false)
            }
            Event::Mouse(mouse) if mouse.kind == MouseEventKind::ScrollUp => {
                self.apply(Action::MouseScrollUp, false)
            }
            Event::Mouse(mouse) if mouse.kind == MouseEventKind::ScrollDown => {
                self.apply(Action::MouseScrollDown, false)
            }
            event => {
                self.editor_events.on_event(event, &mut self.editor);
                self.dirty = true;
                None
            }
        };
        command
    }

    fn submit_editor_or_accept(&mut self) -> Option<Command> {
        self.follow_output = true;
        self.scroll_target = self.max_scroll;
        let text = self.editor_text();
        self.editor = Self::new_editor();
        self.dirty = true;
        Some(Command::Submit(text))
    }

    fn new_editor() -> EditorState {
        EditorState::new(Lines::from(""))
    }

    fn editor_text(&self) -> String {
        self.editor.lines.to_string()
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
        self.follow_output = false;
        self.dirty = true;
    }

    /// Advance scroll animation by one render tick.
    pub fn tick(&mut self) {
        if self.follow_output {
            self.scroll_offset = self.max_scroll;
            self.scroll_target = self.max_scroll;
            return;
        }
        if self.scroll_offset == self.scroll_target {
            if self.scroll_target == self.max_scroll {
                self.follow_output = true;
            }
            return;
        }

        let distance = self.scroll_offset.abs_diff(self.scroll_target);
        let step = (distance / 3).clamp(1, 6);
        if self.scroll_offset < self.scroll_target {
            self.scroll_offset = (self.scroll_offset + step).min(self.scroll_target);
        } else {
            self.scroll_offset = self.scroll_offset.saturating_sub(step);
        }

        if self.scroll_offset == self.scroll_target && self.scroll_target == self.max_scroll {
            self.follow_output = true;
        }
        self.dirty = true;
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

    pub(super) fn editor(&mut self) -> &mut EditorState {
        &mut self.editor
    }

    pub(super) fn editor_mode(&self) -> EditorMode {
        self.editor.mode
    }

    pub(super) fn editor_line_count(&self) -> u16 {
        self.editor
            .lines
            .len()
            .clamp(1, usize::from(theme::EDITOR_VISIBLE_LINES)) as u16
    }

    pub(super) fn cursor_screen_position(&self) -> Option<Position> {
        self.editor.cursor_screen_position()
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

        let [header_area, chat_area, footer_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(4 + state.editor_line_count()),
        ])
        .areas(frame.area());

        self.header.render(frame, header_area, controller, state);
        self.chat.render(frame, chat_area, controller, state);
        self.footer.render(frame, footer_area, controller, state);
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
        assert_eq!(state.scroll_offset, 80);
        assert!(!state.follow_output);
        state.tick();
        assert!(state.scroll_offset < 80);

        assert_eq!(state.sync_scroll(120, 20), 79);
        assert!(!state.follow_output);
    }

    #[test]
    fn scrolling_to_bottom_restores_follow_mode() {
        let mut state = UiState::new();
        state.sync_scroll(100, 20);
        state.scroll_by(-10);
        state.scroll_by(10);

        assert_eq!(state.scroll_target, 80);
        assert!(!state.follow_output);
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
    fn scroll_animation_reaches_target_without_overshooting() {
        let mut state = UiState::new();
        state.sync_scroll(100, 20);
        state.scroll_by(-10);

        for _ in 0..20 {
            state.tick();
        }

        assert_eq!(state.scroll_offset, 70);
        assert_eq!(state.scroll_target, 70);
        assert!(!state.follow_output);
    }

    #[test]
    fn vim_escape_returns_to_normal_without_clearing_text() {
        let mut state = UiState::new();
        state.handle_editor_event(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('i'),
                crossterm::event::KeyModifiers::NONE,
            ),
        ));
        state.handle_editor_event(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('x'),
                crossterm::event::KeyModifiers::NONE,
            ),
        ));
        state.handle_editor_event(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Esc,
                crossterm::event::KeyModifiers::NONE,
            ),
        ));

        assert_eq!(state.editor_text(), "x");
        assert_eq!(state.editor_mode(), EditorMode::Normal);
    }

    #[test]
    fn shift_enter_inserts_newline_without_submitting() {
        let mut state = UiState::new();
        state.handle_editor_event(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('i'),
                crossterm::event::KeyModifiers::NONE,
            ),
        ));
        state.handle_editor_event(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('a'),
                crossterm::event::KeyModifiers::NONE,
            ),
        ));

        let command = state.handle_editor_event(crossterm::event::Event::Key(
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
        state.handle_editor_event(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('i'),
                crossterm::event::KeyModifiers::NONE,
            ),
        ));
        state.handle_editor_event(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('a'),
                crossterm::event::KeyModifiers::NONE,
            ),
        ));

        let command = state.handle_editor_event(crossterm::event::Event::Key(
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
        state.handle_editor_event(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('i'),
                crossterm::event::KeyModifiers::NONE,
            ),
        ));
        let command = state.handle_editor_event(crossterm::event::Event::Key(
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
        state.handle_editor_event(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('i'),
                crossterm::event::KeyModifiers::NONE,
            ),
        ));
        let command = state.handle_editor_event(crossterm::event::Event::Key(
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
        state.handle_editor_event(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('i'),
                crossterm::event::KeyModifiers::NONE,
            ),
        ));
        let command = state.handle_editor_event(crossterm::event::Event::Key(
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

        state.handle_editor_event(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('i'),
                crossterm::event::KeyModifiers::NONE,
            ),
        ));
        state.handle_editor_event(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('h'),
                crossterm::event::KeyModifiers::NONE,
            ),
        ));
        assert!(state.take_dirty());
        assert!(!state.take_dirty());

        let command = state.handle_editor_event(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::NONE,
            ),
        ));

        assert_eq!(command, Some(Command::Submit("h".into())));
        assert!(state.take_dirty());
    }
}
