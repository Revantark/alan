//! Greet: type into a `tui-textarea` input box, press Enter, and the app
//! greets you and echoes what you typed.
//!
//! The editor is configured the same way alan configures its prompt
//! (`UiState::new_editor`): word wrap, hidden in-widget cursor, and the real
//! terminal cursor positioned from `rendered_cursor_position`.

use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph, Widget};
use tui_textarea::{CursorRenderMode, TextArea, WrapMode};

use tui::context::Context;
use tui::{ActionStatus, Component, PassthroughMapper, RenderContext, Runtime};

struct Greet {
    messages: Vec<String>,
    editor: TextArea<'static>,
}

impl Greet {
    /// Same configuration as alan's `UiState::new_editor`.
    fn new_editor() -> TextArea<'static> {
        let mut editor = TextArea::default();
        editor.set_style(Style::default());
        editor.set_cursor_line_style(Style::default());
        editor.set_wrap_mode(WrapMode::WordOrGlyph);
        editor.set_cursor_render_mode(CursorRenderMode::Hidden);
        editor.set_min_rows(1);
        editor.set_placeholder_text("Type your name and press Enter");
        editor
    }
}

impl Component<Event> for Greet {
    fn handle_action(&mut self, event: &Event, cx: &mut Context<'_, Self, Event>) -> ActionStatus {
        let Event::Key(key) = event else {
            return ActionStatus::Handled;
        };
        if key.kind != KeyEventKind::Press {
            return ActionStatus::Handled;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                cx.quit();
            }
            // Plain Enter submits; Shift/Ctrl/Alt+Enter still inserts a
            // newline, matching alan's multiline-enter handling.
            (KeyCode::Enter, modifiers)
                if !modifiers.intersects(
                    KeyModifiers::SHIFT | KeyModifiers::CONTROL | KeyModifiers::ALT,
                ) =>
            {
                let name = self.editor.lines().join("\n");
                if !name.trim().is_empty() {
                    self.messages.push(format!("Hello, {name}!"));
                    self.messages.push(format!("You said: {name}"));
                    cx.notify();
                }
                // alan recreates the editor on submit rather than clearing it.
                self.editor = Self::new_editor();
            }
            // Everything else goes straight to the editor, like alan's
            // `handle_editor_event` fallthrough.
            _ => {
                self.editor.input(event.clone());
                cx.notify();
            }
        }
        ActionStatus::Handled
    }

    fn render(&self, frame: &mut Frame, area: Rect, _cx: &RenderContext<'_, Event>) {
        let [header, messages, input] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(3),
        ])
        .areas(area);

        frame.render_widget(
            Paragraph::new("type your name, Enter to greet · Esc / Ctrl-C to quit"),
            header,
        );
        let lines = self
            .messages
            .iter()
            .map(|message| Line::from(message.clone()))
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(lines), messages);

        let block = Block::default().borders(Borders::ALL).title(" input ");
        frame.render_widget(block, input);
        let inner = Rect {
            x: input.x + 1,
            y: input.y + 1,
            width: input.width.saturating_sub(2),
            height: 1,
        };
        // `Widget` is implemented for `&TextArea`, as footer.rs relies on.
        (&self.editor).render(inner, frame.buffer_mut());
        // Hidden-cursor mode: put the terminal's real cursor where the editor
        // thinks it is.
        if let Some(position) = self.editor.rendered_cursor_position() {
            frame.set_cursor_position(position);
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            Runtime::builder(Greet {
                messages: Vec::new(),
                editor: Greet::new_editor(),
            })
            .key_mapper(PassthroughMapper)
            .tick_rate(Duration::from_millis(50))
            .build()
            .run()
            .await
        })?;
    Ok(())
}
