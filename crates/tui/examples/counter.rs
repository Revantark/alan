//! Counter example demonstrating component-owned behavior:
//!
//! ```text
//! init -> first frame
//! key press -> KeyMapper -> Action -> focused Counter
//! -> Counter opens a confirmation overlay
//! -> overlay sends ApplyChange to Counter or closes as cancelled
//! -> Counter owns its state change -> root updates status
//! ```
//!
//! Two counters live in the entity store; the root inserts them in `init`,
//! so the first frame already shows content. `+`/`-` are handled by the
//! focused counter, which opens its own confirmation overlay. The overlay
//! sends the confirmed change directly back to that counter, so the root does
//! not know about the modal lifecycle or the counter's private state. `s`
//! spawns a simulated save task whose result is delivered to the root.

use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use tui::context::Context;
use tui::entity::Entity;
use tui::keymap::KeyMapper;
use tui::{ActionStatus, Component, FocusHandle, FocusScope, InputContext, RenderContext, Runtime};

/// Semantic user input for this application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Quit,
    Increment,
    Decrement,
    Save,
    ConfirmChange,
    CancelChange,
    FocusNext,
}

/// Messages used by the example components.
#[derive(Debug)]
enum Message {
    /// Root-owned save task result.
    Saved(Result<(), String>),
    /// Confirmation overlay -> counter.
    ApplyChange { delta: i32 },
    /// Counter -> root: the value changed.
    Changed { value: u32 },
    /// Root -> Status.
    SetStatus(String),
    /// Confirmation overlay -> root.
    ChangeCancelled,
}

struct AppKeyMapper;

impl KeyMapper<Action> for AppKeyMapper {
    fn map(&self, event: &crossterm::event::Event, _context: &InputContext) -> Option<Action> {
        use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
        let Event::Key(key) = event else {
            return None;
        };
        if key.kind != KeyEventKind::Press {
            return None;
        }
        match key.code {
            KeyCode::Char('q') if key.modifiers == KeyModifiers::NONE => Some(Action::Quit),
            KeyCode::Char('+') | KeyCode::Char('=') => Some(Action::Increment),
            KeyCode::Char('-') => Some(Action::Decrement),
            KeyCode::Char('s') => Some(Action::Save),
            KeyCode::Char('y') | KeyCode::Enter => Some(Action::ConfirmChange),
            KeyCode::Char('n') | KeyCode::Esc => Some(Action::CancelChange),
            KeyCode::Tab => Some(Action::FocusNext),
            _ => None,
        }
    }
}

#[derive(Debug)]
struct Counter {
    label: &'static str,
    value: u32,
    handle: FocusHandle,
}

impl Counter {
    fn new(label: &'static str, scope: &mut FocusScope) -> Self {
        Self {
            label,
            value: 0,
            handle: scope.handle(),
        }
    }

    fn open_confirmation(&mut self, cx: &mut Context<'_, Self, Action, Message>, delta: i32) {
        cx.open_overlay(ConfirmOverlay {
            counter: cx.entity(),
            delta,
        });
    }

    fn apply(&mut self, delta: i32) -> u32 {
        self.value = (self.value as i32 + delta).max(0) as u32;
        self.value
    }
}
impl Component<Action, Message> for Counter {
    fn init(&mut self, cx: &mut Context<'_, Self, Action, Message>) {
        // Bind eagerly so routed input reaches this counter even before it
        // has ever handled an action.
        cx.bind_focus(self.handle);
    }

    fn handle_message(&mut self, message: Message, cx: &mut Context<'_, Self, Action, Message>) {
        if let Message::ApplyChange { delta } = message {
            self.apply(delta);
            cx.notify();
            cx.emit(Message::Changed { value: self.value });
        }
    }

    fn handle_action(
        &mut self,
        action: &Action,
        cx: &mut Context<'_, Self, Action, Message>,
    ) -> ActionStatus {
        // Claim routed input on first contact; binding is idempotent.
        cx.bind_focus(self.handle);
        match action {
            Action::Increment => {
                self.open_confirmation(cx, 1);
                ActionStatus::Handled
            }
            Action::Decrement if self.value > 0 => {
                self.open_confirmation(cx, -1);
                ActionStatus::Handled
            }
            _ => ActionStatus::Continue,
        }
    }

    fn render(&self, frame: &mut Frame, area: Rect, _cx: &RenderContext<'_, Action, Message>) {
        let line = Line::from(vec![
            Span::styled(
                format!("{}: ", self.label),
                Style::default().add_modifier(Modifier::DIM),
            ),
            Span::raw(self.value.to_string()),
        ]);
        frame.render_widget(Paragraph::new(line), area);
    }
}

struct Status {
    text: String,
}

impl Component<Action, Message> for Status {
    fn handle_message(&mut self, message: Message, cx: &mut Context<'_, Self, Action, Message>) {
        if let Message::SetStatus(text) = message {
            self.text = text;
            cx.notify();
        }
    }

    fn render(&self, frame: &mut Frame, area: Rect, _cx: &RenderContext<'_, Action, Message>) {
        frame.render_widget(Paragraph::new(self.text.clone()), area);
    }
}

/// Modal confirmation overlay. While open it captures all input; `y`
/// confirms the pending change, `n`/Esc dismisses it, and the overlay sends
/// the command directly to its originating counter.
struct ConfirmOverlay {
    counter: Entity<Counter>,
    delta: i32,
}

impl Component<Action, Message> for ConfirmOverlay {
    fn handle_action(
        &mut self,
        action: &Action,
        cx: &mut Context<'_, Self, Action, Message>,
    ) -> ActionStatus {
        match action {
            Action::ConfirmChange => {
                cx.send(self.counter, Message::ApplyChange { delta: self.delta });
                cx.close_overlay();
                ActionStatus::Handled
            }
            Action::CancelChange | Action::Quit => {
                cx.close_overlay();
                cx.emit(Message::ChangeCancelled);
                ActionStatus::Handled
            }
            // Modal boundary: swallow everything else while open.
            _ => ActionStatus::Handled,
        }
    }

    fn render(&self, frame: &mut Frame, area: Rect, _cx: &RenderContext<'_, Action, Message>) {
        let verb = if self.delta > 0 {
            "increment"
        } else {
            "decrement"
        };

        let text = Line::from(vec![
            Span::raw(format!("{verb} the counter? ")),
            Span::styled("[y]es", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" / "),
            Span::styled("[n]o", Style::default().add_modifier(Modifier::BOLD)),
        ]);

        let width = text.width() as u16 + 4;
        let height = 3;

        let popup = Rect {
            x: area.x + area.width.saturating_sub(width) / 2,
            y: area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        };

        // Erase what is behind the popup.
        frame.render_widget(Clear, popup);

        let block = Block::default().borders(Borders::ALL).title(" confirm ");

        let inner = block.inner(popup);

        frame.render_widget(block, popup);
        frame.render_widget(Paragraph::new(text), inner);
    }
}

/// Root component: owns child entities, routes actions, interprets messages.
struct Root {
    scope: FocusScope,
    children: Option<Vec<Entity<Counter>>>,
    status: Option<Entity<Status>>,
}

impl Root {
    fn new() -> Self {
        Self {
            scope: FocusScope::new(),
            children: None,
            status: None,
        }
    }

    fn status_text() -> String {
        "tab: focus | +/-: counter (asks y/n) | s: save | q: quit".to_owned()
    }
}

impl Component<Action, Message> for Root {
    fn init(&mut self, cx: &mut Context<'_, Self, Action, Message>) {
        // Insert children before the first frame; their handles are stable
        // afterwards.
        let mut scope = self.scope.clone();
        let first_counter = Counter::new("first", &mut scope);
        let second_counter = Counter::new("second", &mut scope);
        let first_handle = first_counter.handle;
        let first = cx.insert(first_counter);
        let second = cx.insert(second_counter);
        let status = cx.insert(Status {
            text: Self::status_text(),
        });
        cx.register_scope(scope);
        // Focus the first counter so routed input reaches it.
        cx.focus(first_handle);
        self.children = Some(vec![first, second]);
        self.status = Some(status);
    }

    fn handle_action(
        &mut self,
        action: &Action,
        cx: &mut Context<'_, Self, Action, Message>,
    ) -> ActionStatus {
        match action {
            Action::Quit => {
                cx.quit();
                ActionStatus::Handled
            }
            Action::FocusNext => {
                cx.focus_next();
                ActionStatus::Handled
            }
            Action::Save => {
                cx.spawn(async {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    Ok(Message::Saved(Ok(())))
                });
                ActionStatus::Handled
            }
            // The runtime already sent the action to the focused counter.
            // Never broadcast an unhandled action to sibling counters.
            Action::Increment | Action::Decrement => ActionStatus::Continue,
            // Confirmation actions belong to the active overlay. If there is
            // no overlay, there is nothing for the root to handle.
            _ => ActionStatus::Continue,
        }
    }

    /// Root receives meaningful state changes and task results. UI-local modal
    /// flow stays inside Counter and ConfirmOverlay.
    fn handle_message(&mut self, message: Message, cx: &mut Context<'_, Self, Action, Message>) {
        let text = match message {
            Message::Changed { value } => format!("confirmed: {value}"),
            Message::ChangeCancelled => "change cancelled, focus restored".to_owned(),
            Message::Saved(result) => match result {
                Ok(()) => "saved".to_owned(),
                Err(error) => format!("save failed: {error}"),
            },
            // These messages are owned by other components and are never
            // delivered to the root in normal operation.
            Message::ApplyChange { .. } | Message::SetStatus(_) => return,
        };
        if let Some(status) = self.status {
            cx.send(status, Message::SetStatus(text));
        }
        cx.notify();
    }

    fn render(&self, frame: &mut Frame, area: Rect, cx: &RenderContext<'_, Action, Message>) {
        let Some(children) = &self.children else {
            return;
        };
        let rows = layout_rows(area, children.len() + 1);
        for (index, child) in children.iter().enumerate() {
            cx.render_entity(*child, frame, rows[index]);
        }
        if let Some(status) = &self.status {
            cx.render_entity(*status, frame, rows[rows.len() - 1]);
        }
    }
}
fn layout_rows(area: Rect, count: usize) -> Vec<Rect> {
    (0..count)
        .map(|index| Rect {
            x: area.x,
            y: area.y + index as u16,
            width: area.width,
            height: 1,
        })
        .collect()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            Runtime::builder(Root::new())
                .key_mapper(AppKeyMapper)
                .tick_rate(Duration::from_millis(50))
                .build()
                .run()
                .await
        })?;
    Ok(())
}
