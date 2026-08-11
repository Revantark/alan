//! Ratatui adapter for Alan.
//!
//! Core state stays UI-agnostic. This module owns ratatui-facing editor and
//! scroll state so another frontend can map its own events to [`Action`].

mod component;
mod components;
mod theme;

use crate::core::{Action, Command, Controller, Overlay, Poll};
use components::{Chat, Footer, Header, LoginOverlay};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};

#[derive(Debug)]
pub struct UiState {
    input: String,
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
        Self {
            input: String::new(),
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
            Constraint::Length(5),
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
}
