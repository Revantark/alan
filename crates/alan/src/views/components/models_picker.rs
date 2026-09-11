use crate::root::AlanAction;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, Padding, Paragraph},
};
use tui::context::Context;
use tui::{ActionStatus, Component, RenderContext};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchListEvent {
    Chosen(usize),
    Cancelled,
}

pub struct SearchListOverlay {
    title: String,
    items: Vec<String>,
    filtered: Vec<usize>,
    query: String,
    selected: usize,
}

impl SearchListOverlay {
    pub fn new(title: impl Into<String>, items: Vec<String>) -> Self {
        let filtered = (0..items.len()).collect();
        Self {
            title: title.into(),
            items,
            filtered,
            query: String::new(),
            selected: 0,
        }
    }

    pub fn set_items(&mut self, items: Vec<String>) {
        self.items = items;
        self.refresh();
    }

    fn refresh(&mut self) {
        let query = self.query.to_lowercase();
        self.filtered = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| query.is_empty() || item.to_lowercase().contains(&query))
            .map(|(index, _)| index)
            .take(200)
            .collect();
        self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
    }

    fn move_selection(&mut self, delta: isize) {
        if self.filtered.is_empty() {
            return;
        }
        let max = self.filtered.len() as isize - 1;
        self.selected = (self.selected as isize + delta).clamp(0, max) as usize;
    }
}

impl Component<AlanAction> for SearchListOverlay {
    fn handle_action(
        &mut self,
        action: &AlanAction,
        cx: &mut Context<'_, Self, AlanAction>,
    ) -> ActionStatus {
        if matches!(action, &AlanAction::Resize) {
            cx.notify();
            return ActionStatus::Handled;
        }
        let AlanAction::Raw(Event::Key(key)) = action else {
            return ActionStatus::Handled;
        };
        if key.kind != KeyEventKind::Press {
            return ActionStatus::Handled;
        }
        match key.code {
            KeyCode::Esc => {
                cx.emit(SearchListEvent::Cancelled);
                cx.close_overlay();
            }
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),
            KeyCode::Enter | KeyCode::Tab if key.modifiers.is_empty() => {
                if let Some(index) = self.filtered.get(self.selected).copied() {
                    cx.emit(SearchListEvent::Chosen(index));
                    cx.close_overlay();
                }
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.refresh();
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.query.push(c);
                self.refresh();
            }
            _ => return ActionStatus::Handled,
        }
        cx.notify();
        ActionStatus::Handled
    }

    fn render(&self, frame: &mut Frame, area: Rect, _: &RenderContext<'_, AlanAction>) {
        let width = area.width.saturating_sub(8).min(100);
        let height = area.height.clamp(1, 12);
        let popup = Rect {
            x: area.x + area.width.saturating_sub(width) / 2,
            y: area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        };
        frame.render_widget(Clear, popup);
        let body = if self.filtered.is_empty() {
            "No matches".to_owned()
        } else {
            self.filtered
                .iter()
                .skip(self.selected.saturating_sub(5))
                .take(height.saturating_sub(2) as usize)
                .enumerate()
                .map(|(offset, index)| {
                    format!(
                        "{}{}",
                        if offset == self.selected.min(5) {
                            "› "
                        } else {
                            "  "
                        },
                        self.items[*index]
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let mut lines = vec![
            Line::from(vec![
                Span::styled(
                    format!("  {}", self.title),
                    Style::default().add_modifier(Modifier::BOLD | Modifier::ITALIC),
                ),
                Span::raw(format!(": {}", self.query)),
            ]),
            Line::from(""),
        ];

        lines.extend(body.lines().map(Line::from));
        frame.render_widget(
            Paragraph::new(lines)
                .style(Style::default().bg(crate::views::theme::EDITOR_BG))
                .block(Block::default().padding(Padding {
                    left: 2,
                    right: 2,
                    top: 1,
                    bottom: 1,
                })),
            popup,
        );
    }
}

pub type ModelPick = SearchListEvent;
pub type ModelsPicker = SearchListOverlay;
