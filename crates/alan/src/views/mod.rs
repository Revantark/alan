//! Ratatui adapter for Alan.
//!
//! Core state stays UI-agnostic. This module owns ratatui-facing editor and
//! scroll state so another frontend can map its own events to [`Action`].

use crate::core::{Action, Controller, Entry};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Wrap};

const PROMPT_FG: Color = Color::Cyan;
const USER_FG: Color = Color::White;
const USER_BG: Color = Color::Rgb(42, 48, 58);
const RESPONSE_FG: Color = Color::White;
const MUTED_FG: Color = Color::DarkGray;
const EDITOR_BG: Color = Color::Rgb(28, 32, 39);
const CHAT_PADDING: usize = 3;
const EDITOR_FG: Color = Color::White;

#[derive(Debug, Default)]
pub struct UiState {
    input: String,
    scroll: u16,
    follow_output: bool,
}

impl UiState {
    pub fn new() -> Self {
        Self {
            follow_output: true,
            ..Self::default()
        }
    }

    pub fn apply(&mut self, action: Action, controller: &mut Controller) -> bool {
        match action {
            Action::Quit => !controller.is_busy(),
            Action::Submit => {
                if !controller.is_busy() {
                    let text = std::mem::take(&mut self.input);
                    controller.submit(text);
                }
                false
            }
            Action::ClearInput => {
                self.input.clear();
                false
            }
            Action::Backspace => {
                self.input.pop();
                false
            }
            Action::Insert(c) => {
                self.input.push(c);
                false
            }
            Action::ScrollUp => {
                self.follow_output = false;
                self.scroll = self.scroll.saturating_sub(10);
                false
            }
            Action::ScrollDown => {
                self.follow_output = false;
                self.scroll = self.scroll.saturating_add(10);
                false
            }
        }
    }

    pub fn on_poll(&mut self, poll: crate::core::Poll) {
        if matches!(poll, crate::core::Poll::Reply | crate::core::Poll::Error) {
            self.follow_output = true;
        }
    }
}

pub fn draw(frame: &mut Frame, controller: &Controller, state: &UiState) {
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

fn draw_chat(frame: &mut Frame, area: Rect, controller: &Controller, state: &UiState) {
    let transcript = build_transcript(controller.chat(), area.width.max(1));
    let scroll = chat_scroll(&transcript, area.height.max(1), state);
    let chat = Paragraph::new(transcript).scroll((scroll, 0));
    frame.render_widget(chat, area);
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
            Span::styled("  Esc clear · Ctrl-C quit", Style::default().fg(MUTED_FG)),
        ])
    } else {
        Line::from(vec![
            Span::styled("  ● idle", Style::default().fg(Color::Green)),
            Span::styled(
                "  Enter send · PageUp/PageDown scroll · q quit",
                Style::default().fg(MUTED_FG),
            ),
        ])
    };
    frame.render_widget(
        Paragraph::new(status).style(Style::default().bg(EDITOR_BG)),
        status_area,
    );

    let input_line = if state.input.is_empty() {
        Line::from(vec![Span::styled("  › ", Style::default().fg(PROMPT_FG))])
    } else {
        Line::from(vec![
            Span::styled("  › ", Style::default().fg(PROMPT_FG)),
            Span::styled(state.input.clone(), Style::default().fg(EDITOR_FG)),
        ])
    };
    frame.render_widget(
        Paragraph::new(Text::from(input_line))
            .style(Style::default().fg(EDITOR_FG).bg(EDITOR_BG))
            .wrap(Wrap { trim: false }),
        editor_area,
    );

    // Cursor points at next insertion cell. Include full prompt width and
    // rendered text width, not character count.
    let prompt_width = Line::from("  › ").width() as u16;
    let input_width = Line::from(state.input.as_str()).width() as u16;
    let cursor_x = editor_area
        .x
        .saturating_add(prompt_width)
        .saturating_add(input_width)
        .min(editor_area.right().saturating_sub(1));
    frame.set_cursor_position((cursor_x, editor_area.y));
}

fn build_transcript(entries: &[Entry], width: u16) -> Text<'static> {
    let width = usize::from(width.max(1));
    let content_width = width.saturating_sub(CHAT_PADDING * 2).max(1);
    let mut lines = Vec::new();

    for entry in entries {
        match entry {
            Entry::Prompt(text) => {
                // User messages get one contiguous, padded background block.
                lines.push(background_line("", width, USER_FG, USER_BG));
                for line in wrap_text(text, content_width) {
                    lines.push(background_line(&format!("{line}"), width, USER_FG, USER_BG));
                }
                lines.push(background_line("", width, USER_FG, USER_BG));
            }
            Entry::Response(text) => {
                // Assistant text starts at same content column as user text,
                // but stays visually quiet and background-free.
                lines.push(Line::default());
                if text.is_empty() {
                    lines.push(indented_line("(empty response)", MUTED_FG));
                } else {
                    for line in wrap_text(text, content_width) {
                        lines.push(indented_line(&line, RESPONSE_FG));
                    }
                }
                lines.push(Line::default());
            }
            Entry::Error(text) => {
                for line in wrap_text(text, content_width) {
                    lines.push(indented_line(&line, Color::Red));
                }
                lines.push(Line::default());
            }
        }
    }
    Text::from(lines)
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
        for character in source_line.chars() {
            let mut candidate = current.clone();
            candidate.push(character);
            if !current.is_empty() && Line::from(candidate.as_str()).width() > width {
                wrapped.push(current);
                current = character.to_string();
            } else {
                current = candidate;
            }
        }
        wrapped.push(current);
    }
    if wrapped.is_empty() {
        wrapped.push(String::new());
    }
    wrapped
}

fn chat_scroll(text: &Text<'_>, height: u16, state: &UiState) -> u16 {
    if !state.follow_output {
        return state.scroll;
    }
    (text.height() as u16).saturating_sub(height)
}
