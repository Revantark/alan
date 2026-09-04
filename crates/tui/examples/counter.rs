//! Counter example demonstrating explicit communication:
//!
//! ```text
//! keypress -> Action -> focused Counter -> confirmation overlay
//! overlay -> typed ConfirmResult event -> Counter -> notify/observe -> redraw
//! ```
//!
//! The confirmation result is an occurrence, so it uses a typed event rather
//! than a global message bus. The root observes each counter's state and reads
//! it again to derive the status text.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use std::time::Duration;
use tui::context::Context;
use tui::entity::Entity;
use tui::keymap::KeyMapper;
use tui::{ActionStatus, Component, InputContext, RenderContext, Runtime, Subscription, TaskError};

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

#[derive(Debug)]
enum ConfirmResult {
    Accepted { delta: i32 },
    Cancelled,
}

struct AppKeyMapper;
impl KeyMapper<Action> for AppKeyMapper {
    fn map(&self, event: &crossterm::event::Event, _: &InputContext) -> Option<Action> {
        use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
        let Event::Key(key) = event else { return None };
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
    confirmation: Option<Subscription>,
}

impl Counter {
    fn new(label: &'static str) -> Self {
        Self {
            label,
            value: 0,
            confirmation: None,
        }
    }

    fn open_confirmation(&mut self, cx: &mut Context<'_, Self, Action>, delta: i32) {
        let overlay = cx.open_overlay(ConfirmOverlay { delta });
        self.confirmation = Some(cx.subscribe::<ConfirmResult, _, _>(
            overlay,
            |result, counter, _, cx| {
                match result {
                    ConfirmResult::Accepted { delta } => {
                        counter.value = (counter.value as i32 + delta).max(0) as u32;
                    }
                    ConfirmResult::Cancelled => {}
                }
                counter.confirmation = None;
                cx.notify();
            },
        ));
    }
}

impl Component<Action> for Counter {
    fn handle_action(
        &mut self,
        action: &Action,
        cx: &mut Context<'_, Self, Action>,
    ) -> ActionStatus {
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

    fn render(&self, frame: &mut Frame, area: Rect, cx: &RenderContext<'_, Action>) {
        let style = if cx.is_focused() {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!("{}: ", self.label),
                    Style::default().add_modifier(Modifier::DIM),
                ),
                Span::styled(self.value.to_string(), style),
            ])),
            area,
        );
    }
}

struct Status {
    text: String,
}

impl Component<Action> for Status {
    fn render(&self, frame: &mut Frame, area: Rect, _: &RenderContext<'_, Action>) {
        frame.render_widget(Paragraph::new(self.text.clone()), area);
    }
}

struct ConfirmOverlay {
    delta: i32,
}

impl Component<Action> for ConfirmOverlay {
    fn handle_action(
        &mut self,
        action: &Action,
        cx: &mut Context<'_, Self, Action>,
    ) -> ActionStatus {
        match action {
            Action::ConfirmChange => {
                cx.emit(ConfirmResult::Accepted { delta: self.delta });
                cx.close_overlay();
                ActionStatus::Handled
            }
            Action::CancelChange | Action::Quit => {
                cx.emit(ConfirmResult::Cancelled);
                cx.close_overlay();
                ActionStatus::Handled
            }
            _ => ActionStatus::Handled,
        }
    }

    fn render(&self, frame: &mut Frame, area: Rect, _: &RenderContext<'_, Action>) {
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
        frame.render_widget(Clear, popup);
        let block = Block::default().borders(Borders::ALL).title(" confirm ");
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        frame.render_widget(Paragraph::new(text), inner);
    }
}

struct Root {
    children: Option<Vec<Entity<Counter>>>,
    status: Option<Entity<Status>>,
    observations: Vec<Subscription>,
}

impl Root {
    fn new() -> Self {
        Self {
            children: None,
            status: None,
            observations: Vec::new(),
        }
    }
}

impl Component<Action> for Root {
    fn init(&mut self, cx: &mut Context<'_, Self, Action>) {
        let first = cx.insert(Counter::new("first"));
        let second = cx.insert(Counter::new("second"));
        let status = cx.insert(Status {
            text: "tab: focus | +/-: counter (asks y/n) | s: save | q: quit".to_owned(),
        });
        cx.focus_entity(first);
        cx.focus_order([first.id(), second.id()]);
        self.children = Some(vec![first, second]);
        self.status = Some(status);
        for counter in [first, second] {
            self.observations
                .push(cx.observe(counter, move |root, source, cx| {
                    let Some(value) = cx.read(source, |counter| counter.value) else {
                        return;
                    };
                    let _ = cx.update(status, |status| {
                        status.text = format!(
                            "{}: {value}",
                            if value == 0 { "ready" } else { "confirmed" }
                        )
                    });
                    root_status_notify(root, cx);
                }));
        }
    }

    fn handle_action(
        &mut self,
        action: &Action,
        cx: &mut Context<'_, Self, Action>,
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
                cx.spawn(
                    async {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        Ok::<_, TaskError>(())
                    },
                    |result, root, cx| {
                        let _ = result;
                        if let Some(status) = root.status {
                            let _ = cx.update(status, |status| status.text = "saved".to_owned());
                        }
                        cx.notify();
                    },
                );
                ActionStatus::Handled
            }
            _ => ActionStatus::Continue,
        }
    }

    fn render(&self, frame: &mut Frame, area: Rect, cx: &RenderContext<'_, Action>) {
        let Some(children) = &self.children else {
            return;
        };
        let rows = layout_rows(area, children.len() + 1);
        for (i, child) in children.iter().enumerate() {
            cx.render_entity(*child, frame, rows[i]);
        }
        if let Some(status) = self.status {
            cx.render_entity(status, frame, rows[rows.len() - 1]);
        }
    }
}

fn root_status_notify(_root: &mut Root, cx: &mut Context<'_, Root, Action>) {
    cx.notify();
}

fn layout_rows(area: Rect, count: usize) -> Vec<Rect> {
    (0..count)
        .map(|i| Rect {
            x: area.x,
            y: area.y + i as u16,
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
