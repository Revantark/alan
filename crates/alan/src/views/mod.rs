//! Ratatui adapter for Alan.
//!
//! Core state stays UI-agnostic. This module owns ratatui-facing editor and
//! scroll state so another frontend can map its own events to [`Action`].

use crate::core::{Action, Command, Controller, Entry, LoginState, Overlay, Poll};
use providers::AuthPrompt;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};

const PROMPT_FG: Color = Color::Cyan;
const USER_FG: Color = Color::White;
const USER_BG: Color = Color::Rgb(42, 48, 58);
const RESPONSE_FG: Color = Color::White;
const MUTED_FG: Color = Color::DarkGray;
const EDITOR_BG: Color = Color::Rgb(28, 32, 39);
const CHAT_PADDING: usize = 3;
const EDITOR_FG: Color = Color::White;

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

pub fn draw(frame: &mut Frame, controller: &Controller, state: &mut UiState) {
    if controller.overlay() == Overlay::Login {
        // Login owns entire visible interaction surface. Skip normal editor
        // rendering so its cursor cannot leak into the modal.
        frame.render_widget(ratatui::widgets::Clear, frame.area());
        draw_login(frame, controller, state);
        return;
    }

    let [header_area, chat_area, footer_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(5),
    ])
    .areas(frame.area());

    draw_header(frame, header_area);
    draw_chat(frame, chat_area, controller, state);
    draw_footer(frame, footer_area, controller, state);
}

fn draw_header(frame: &mut Frame, area: Rect) {
    let header = Paragraph::new(Line::from(vec![Span::styled(
        " alan ",
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )]));
    frame.render_widget(header, area);
}

fn draw_chat(frame: &mut Frame, area: Rect, controller: &Controller, state: &mut UiState) {
    let [content_area, scrollbar_area] =
        Layout::horizontal([Constraint::Min(1), Constraint::Length(1)]).areas(area);
    let transcript = build_transcript(controller.chat(), content_area.width.max(1));
    let content_height = transcript.height();
    let viewport_height = usize::from(content_area.height.max(1));
    let scroll = state.sync_scroll(content_height, viewport_height);
    let paragraph_scroll = scroll.min(usize::from(u16::MAX)) as u16;
    let chat = Paragraph::new(transcript).scroll((paragraph_scroll, 0));
    frame.render_widget(chat, content_area);

    if state.max_scroll > 0 {
        let scrollbar_position = scrollbar_position(scroll, state.max_scroll, content_height);
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

fn draw_login(frame: &mut Frame, controller: &Controller, ui: &UiState) {
    let state = controller.login_state();
    if !state.is_open() {
        return;
    }

    let area = centered_rect(70, 60, frame.area());
    frame.render_widget(ratatui::widgets::Clear, area);
    frame.render_widget(
        Paragraph::new("").style(Style::default().bg(EDITOR_BG)),
        area,
    );

    let [content_area, shortcuts_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area.inner(
            ratatui::layout::Margin {
                horizontal: 2,
                vertical: 1,
            },
        ));

    match state {
        LoginState::Selecting {
            providers,
            selected,
        } => {
            let items = providers
                .iter()
                .enumerate()
                .map(|(index, provider)| {
                    let marker = if index == *selected { "› " } else { "  " };
                    Line::from(vec![
                        Span::styled(marker, Style::default().fg(PROMPT_FG)),
                        Span::styled(provider.name.clone(), Style::default().fg(EDITOR_FG)),
                    ])
                })
                .collect::<Vec<_>>();
            frame.render_widget(Paragraph::new(Text::from(items)), content_area);
            draw_shortcuts(
                frame,
                shortcuts_area,
                "↑↓ select · Enter confirm · Esc cancel",
            );
        }
        LoginState::Prompting { prompt, .. } => {
            let secret = matches!(prompt, AuthPrompt::Secret { .. });
            let value = if secret {
                "•".repeat(ui.input.chars().count())
            } else {
                ui.input.clone()
            };
            let prompt_line = Line::from(prompt_message(prompt));
            let input_line = Line::from(vec![
                Span::styled("› ", Style::default().fg(PROMPT_FG)),
                Span::styled(value.clone(), Style::default().fg(EDITOR_FG)),
            ]);
            let content = Text::from(vec![prompt_line, Line::default(), input_line]);
            frame.render_widget(Paragraph::new(content), content_area);
            draw_shortcuts(frame, shortcuts_area, "Enter submit · Esc cancel");

            let input_width = Line::from(value.as_str()).width() as u16;
            let cursor_x = content_area
                .x
                .saturating_add(2)
                .saturating_add(input_width)
                .min(content_area.right().saturating_sub(1));
            frame.set_cursor_position((cursor_x, content_area.y + 2));
        }
        LoginState::Validating { message, .. } => {
            frame.render_widget(Paragraph::new(message.as_str()), content_area);
            draw_shortcuts(frame, shortcuts_area, "Esc cancel");
        }
        LoginState::Success { provider } => {
            frame.render_widget(
                Paragraph::new(format!("Logged in to {}", provider.0)),
                content_area,
            );
            draw_shortcuts(frame, shortcuts_area, "Esc close");
        }
        LoginState::Error(message) => {
            frame.render_widget(
                Paragraph::new(message.as_str()).style(Style::default().fg(Color::Red)),
                content_area,
            );
            draw_shortcuts(frame, shortcuts_area, "Esc close");
        }
        LoginState::Closed => {}
    }
}

fn draw_shortcuts(frame: &mut Frame, area: Rect, text: &str) {
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            text,
            Style::default().fg(MUTED_FG),
        ))),
        area,
    );
}

fn prompt_message(prompt: &AuthPrompt) -> String {
    match prompt {
        AuthPrompt::Secret { message }
        | AuthPrompt::Text { message }
        | AuthPrompt::ManualCode { message } => message.clone(),
        AuthPrompt::Select { message, .. } => message.clone(),
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let horizontal: [Rect; 3] = Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .areas(area);
    let vertical: [Rect; 3] = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .areas(horizontal[1]);
    vertical[1]
}

fn draw_footer(frame: &mut Frame, area: Rect, controller: &Controller, state: &UiState) {
    let background = Paragraph::new("").style(Style::default().bg(EDITOR_BG));
    frame.render_widget(background, area);

    let [_top_padding, status_area, _status_editor_gap, editor_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);

    let status = if controller.is_busy() {
        Line::from(vec![
            Span::styled("  ● thinking", Style::default().italic().fg(Color::Yellow)),
            Span::styled("  Esc clear · Ctrl-C stop", Style::default().fg(MUTED_FG)),
        ])
    } else {
        Line::from(vec![
            Span::styled("  ● idle", Style::default().fg(Color::Green)),
            Span::styled(
                "  Enter send · PageUp/PageDown scroll · Ctrl-C quit",
                Style::default().fg(MUTED_FG),
            ),
        ])
    };
    frame.render_widget(
        Paragraph::new(status).style(Style::default().bg(EDITOR_BG)),
        status_area,
    );

    // Keep editor single-line. Show newest input suffix when text exceeds
    // available width so cursor remains at insertion point.
    let prompt_width = Line::from("  › ").width() as usize;
    let available_width = usize::from(editor_area.width).saturating_sub(prompt_width);
    let secret_input = matches!(
        controller.login_state(),
        LoginState::Prompting {
            prompt: AuthPrompt::Secret { .. },
            ..
        }
    );
    let editor_input = if secret_input {
        "•".repeat(state.input.chars().count())
    } else {
        state.input.clone()
    };
    let visible_input = visible_suffix(&editor_input, available_width);
    let input_line = Line::from(vec![
        Span::styled("  › ", Style::default().fg(PROMPT_FG)),
        Span::styled(visible_input.clone(), Style::default().fg(EDITOR_FG)),
    ]);
    frame.render_widget(
        Paragraph::new(Text::from(input_line)).style(Style::default().fg(EDITOR_FG).bg(EDITOR_BG)),
        editor_area,
    );

    // Cursor points at next insertion cell. Include full prompt width and
    // rendered text width, not byte or character count.
    let input_width = Line::from(visible_input.as_str()).width() as u16;
    let cursor_x = editor_area
        .x
        .saturating_add(prompt_width as u16)
        .saturating_add(input_width)
        .min(editor_area.right().saturating_sub(1));
    frame.set_cursor_position((cursor_x, editor_area.y));
}

fn visible_suffix(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }

    let mut width = 0;
    let mut start = text.len();
    for (index, character) in text.char_indices().rev() {
        let character_width = Line::from(character.to_string()).width();
        if width + character_width > max_width {
            break;
        }
        width += character_width;
        start = index;
    }
    text[start..].to_owned()
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
    let content_width = width.saturating_sub(CHAT_PADDING * 2).max(1);
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
            lines.push(background_line("", width, USER_FG, USER_BG));
            for line in wrap_text(text, content_width) {
                lines.push(background_line(&line, width, USER_FG, USER_BG));
            }
            lines.push(background_line("", width, USER_FG, USER_BG));
            lines
        }
        Entry::Response(text) => {
            let mut lines = vec![Line::default()];
            if text.is_empty() {
                lines.push(indented_line("(empty response)", MUTED_FG));
            } else {
                for line in wrap_text(text, content_width) {
                    lines.push(indented_line(&line, RESPONSE_FG));
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
    let content = format!("{}{}", " ".repeat(CHAT_PADDING), text);
    let used_width = Line::from(content.as_str()).width();
    let trailing = " ".repeat(width.saturating_sub(used_width));
    Line::from(Span::styled(
        format!("{content}{trailing}"),
        Style::default().fg(foreground).bg(background),
    ))
}

fn indented_line(text: &str, foreground: Color) -> Line<'static> {
    Line::from(Span::styled(
        format!("{}{}", " ".repeat(CHAT_PADDING), text),
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
    fn input_suffix_uses_display_width() {
        assert_eq!(visible_suffix("abcdef", 3), "def");
        assert_eq!(visible_suffix("界界界", 4), "界界");
        assert_eq!(visible_suffix("abcdef", 0), "");
    }
}
