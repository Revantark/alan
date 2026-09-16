use crate::{root::AlanAction, views::theme};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::Alignment,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Paragraph},
};
use tui::context::Context;
use tui::{ActionStatus, Component, RenderContext};
use unicode_width::UnicodeWidthChar;

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
            KeyCode::Char('p') if key.modifiers == KeyModifiers::CONTROL => self.move_selection(-1),
            KeyCode::Char('n') if key.modifiers == KeyModifiers::CONTROL => self.move_selection(1),
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
        self.render_picker(frame, area);
    }
}

impl SearchListOverlay {
    fn render_picker(&self, frame: &mut Frame, area: Rect) {
        let width = area.width.saturating_sub(4).min(88);
        let height = area.height.saturating_sub(2).min(18);
        if width < 12 || height < 6 {
            return;
        }
        let popup = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        // frame.render_widget(Clear, popup);
        let accent = Style::default().fg(theme::PROMPT_FG);
        let muted = Style::default().fg(theme::TOOL_FG);
        let title = format!(" {}", self.title);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(accent)
            .style(Style::default().bg(Color::Reset).fg(theme::USER_FG))
            .title(Line::from(title).style(accent))
            .title_alignment(Alignment::Center)
            .title_bottom(
                Line::from(if width >= 60 {
                    " ↑↓ / C-n C-p move   Enter select   Esc close "
                } else if width >= 38 {
                    " ↑↓ move · Enter select · Esc close "
                } else {
                    " Enter / Esc "
                })
                .style(muted),
            );
        let inner = block.inner(popup);

        frame.render_widget(block, popup);
        let search = Rect::new(inner.x + 1, inner.y, inner.width.saturating_sub(2), 1);
        // Keep the end of the query and the cursor visible, including wide characters.
        let available = search.width.saturating_sub(3) as usize;
        let mut used = 0;
        let suffix: String = self
            .query
            .chars()
            .rev()
            .take_while(|c| {
                let width = c.width().unwrap_or(0);
                if used + width > available {
                    return false;
                }
                used += width;
                true
            })
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let prompt = if self.query.is_empty() {
            Line::from(vec![
                Span::styled("❯ ", accent),
                Span::styled("Search models…", muted),
            ])
        } else {
            Line::from(vec![Span::styled("❯ ", accent), Span::raw(suffix)])
        };
        frame.render_widget(Paragraph::new(prompt), search);
        frame.set_cursor_position((search.x + 2 + used as u16, search.y));
        frame.render_widget(
            Paragraph::new("─".repeat(inner.width as usize)).style(muted),
            Rect::new(inner.x, inner.y + 1, inner.width, 1),
        );
        let rows = inner.height.saturating_sub(3) as usize;
        let start = self
            .selected
            .saturating_sub(rows / 2)
            .min(self.filtered.len().saturating_sub(rows));
        let list_area = Rect::new(inner.x, inner.y + 2, inner.width, rows as u16);
        if self.filtered.is_empty() {
            frame.render_widget(
                Paragraph::new("  No matching models").style(muted),
                list_area,
            );
        } else {
            let lines: Vec<Line> = self
                .filtered
                .iter()
                .skip(start)
                .take(rows)
                .enumerate()
                .map(|(offset, index)| {
                    let selected = start + offset == self.selected;
                    let style = if selected {
                        Style::default()
                            .bg(theme::SELECTION_BG)
                            .fg(theme::SELECTION_FG)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(theme::EDITOR_FG)
                    };
                    let prefix = if selected { ">  " } else { "   " };
                    let current_len = (prefix.chars().count() + self.items[*index].chars().count()) as u16;
                    let padding_len = width.saturating_sub(current_len) as usize;
                    let padding = " ".repeat(padding_len);

                    Line::from(vec![
                        Span::styled(prefix, accent),
                        Span::raw(self.items[*index].as_str()),
                        Span::styled(padding, style),
                    ])
                    .style(style)
                })
                .collect();
            frame.render_widget(Paragraph::new(lines), list_area);
        }
        // frame.render_widget(
        //     Paragraph::new(" ".repeat(inner.width as usize)).style(muted),
        //     Rect::new(inner.x, inner.bottom() - 2, inner.width, 1),
        // );
    }
}

pub type ModelPick = SearchListEvent;
pub type ModelsPicker = SearchListOverlay;

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn selected_model_remains_visible_at_every_terminal_size() {
        let mut picker = ModelsPicker::new(
            "Models",
            (0..250).map(|i| format!("provider/model-{i:03}")).collect(),
        );
        picker.refresh();
        assert_eq!(picker.filtered.len(), 250);
        picker.selected = 249;
        for (width, height) in [(100, 30), (45, 12), (20, 8), (8, 3)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| picker.render_picker(frame, frame.area()))
                .unwrap();
            let buffer = terminal.backend().buffer();
            if width >= 20 {
                assert!(
                    buffer
                        .content
                        .iter()
                        .any(|cell| cell.bg == theme::SELECTION_BG)
                );
                let screen: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
                if width >= 45 {
                    assert!(screen.contains("250/250"));
                    assert!(screen.contains("provider/model-249"));
                }
            }
        }
    }

    #[test]
    fn empty_results_and_wide_search_render_without_overflow() {
        let mut picker = ModelsPicker::new("Models", vec!["openai/example".into()]);
        picker.query = "界".repeat(80);
        picker.refresh();
        let mut terminal = Terminal::new(TestBackend::new(45, 12)).unwrap();
        terminal
            .draw(|frame| picker.render_picker(frame, frame.area()))
            .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(screen.contains("No matching models"));
        assert!(screen.contains("0/0"));
    }
}
