//! Generic selection popup, v2.
//!
//! This owns its own selection and key handling. It is a real `tui`
//! component — not a render-only view — so it can be focused and receive
//! keys directly. On accept or dismiss it emits a typed event; the caller
//! subscribes and applies the side effect. The event type is local to this
//! module and is never an `AlanAction` variant.
//!
//! The popup is generic in shape: it shows a non-selectable `message` line
//! (loading, no matches, error) or, when that is `None`, a scrollable list of
//! items with a moving selection. Any caller — path completion, model
//! picking, command palettes — uses the same widget.

use crate::views::theme;
use crossterm::event::{KeyCode, KeyEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Padding, Paragraph};
use tui::context::Context;
use tui::{ActionStatus, Component, RenderContext};

use crate::root::AlanAction;

const VISIBLE_ROWS: usize = 5;
const CONTENT_PADDING: Padding = Padding::new(2, 2, 1, 1);

/// Emitted when the user accepts an item or dismisses the popup. The caller
/// decides what to do with the index; the popup never knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PopupSelected {
    Accept { index: usize },
    Dismiss,
}

/// A generic, focusable selection popup.
#[derive(Debug, Default)]
pub struct PopupListv2 {
    open: bool,
    /// Non-selectable state line. When `Some`, it replaces the item list
    /// (loading, no matches, error). When `None`, the items render.
    message: Option<String>,
    items: Vec<String>,
    selected: usize,
}

impl PopupListv2 {
    /// Replace the popup's contents. Resets selection to zero only when the item
    /// set actually changes, so typing narrows from the top while navigating
    /// (and the 16ms tick) leaves the cursor where the user put it.
    pub fn set(&mut self, open: bool, message: Option<String>, items: Vec<String>) {
        let items_changed = items != self.items;
        self.open = open;
        self.message = message;
        if items_changed {
            self.selected = 0;
        }
        self.items = items;
    }

    /// Whether the popup already holds this snapshot, so a redundant push can
    /// be skipped — keeping the 16ms tick quiet, as the old snapshot diff did.
    pub fn matches(&self, open: bool, message: Option<&str>, items: &[String]) -> bool {
        self.open == open && self.message.as_deref() == message && self.items == items
    }

    /// Move the selection by `delta`, clamped to the item bounds.
    fn move_selection(&mut self, delta: isize) {
        if self.items.is_empty() {
            return;
        }
        let max = self.items.len() as isize - 1;
        self.selected = (self.selected as isize + delta).clamp(0, max) as usize;
    }

    /// Return the area directly above `prompt`, or `None` when it does not fit.
    pub fn area_above(prompt: Rect, frame_area: Rect, rows: u16) -> Option<Rect> {
        let height = rows.saturating_add(CONTENT_PADDING.top + CONTENT_PADDING.bottom);
        let top = prompt.y.checked_sub(height)?;
        if top < frame_area.y || prompt.y > frame_area.bottom() {
            return None;
        }
        Some(Rect {
            x: frame_area.x,
            y: top,
            width: frame_area.width,
            height,
        })
    }

    fn render_into(&self, frame: &mut Frame, area: Rect) {
        if area.is_empty() || !self.open {
            return;
        }

        if let Some(message) = &self.message {
            frame.render_widget(
                Paragraph::new(message.clone())
                    .style(Style::default().bg(theme::EDITOR_BG))
                    .block(Block::default().padding(CONTENT_PADDING)),
                area,
            );
            return;
        }

        frame.render_widget(ratatui::widgets::Clear, area);
        let selected = self.selected.min(self.items.len().saturating_sub(1));
        let start = selected
            .saturating_sub(2)
            .min(self.items.len().saturating_sub(VISIBLE_ROWS));
        let lines = self
            .items
            .iter()
            .skip(start)
            .take(VISIBLE_ROWS)
            .enumerate()
            .map(|(offset, item)| item_line(item, start + offset == selected))
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(Text::from(lines))
                .style(Style::default().bg(theme::EDITOR_BG))
                .block(Block::default().padding(CONTENT_PADDING)),
            area,
        );
    }
}

impl Component<AlanAction> for PopupListv2 {
    fn render(&self, frame: &mut Frame, area: Rect, _: &RenderContext<'_, AlanAction>) {
        self.render_into(frame, area);
    }

    fn handle_action(
        &mut self,
        action: &AlanAction,
        cx: &mut Context<'_, Self, AlanAction>,
    ) -> ActionStatus {
        if !self.open {
            return ActionStatus::Continue;
        }

        // Only key presses are navigation; release events and everything else
        // fall through to the editor.
        let AlanAction::Raw(crossterm::event::Event::Key(key)) = action else {
            return ActionStatus::Continue;
        };
        if key.kind != KeyEventKind::Press {
            return ActionStatus::Continue;
        }

        match key.code {
            KeyCode::Up => {
                self.move_selection(-1);
                cx.notify();
                ActionStatus::Handled
            }
            KeyCode::Down => {
                self.move_selection(1);
                cx.notify();
                ActionStatus::Handled
            }
            KeyCode::Esc => {
                self.open = false;
                cx.emit(PopupSelected::Dismiss);
                ActionStatus::Handled
            }
            KeyCode::Enter | KeyCode::Tab if key.modifiers.is_empty() => {
                if self.items.get(self.selected).is_some() {
                    self.open = false;
                    cx.emit(PopupSelected::Accept {
                        index: self.selected,
                    });
                }
                ActionStatus::Handled
            }
            // Typing, backspace, cursor movement, paste: editor input.
            // Returning `Continue` lets the framework walk up to the parent
            // (root), which feeds the prompt editor.
            _ => ActionStatus::Continue,
        }
    }
}

fn item_line(display: &str, selected: bool) -> Line<'static> {
    let (marker, color) = if selected {
        ("› ", theme::SELECTION_FG)
    } else {
        ("  ", theme::EDITOR_FG)
    };
    Line::from(vec![
        Span::styled(marker.to_owned(), Style::default().fg(theme::PROMPT_FG)),
        Span::styled(display.to_owned(), Style::default().fg(color)),
    ])
}
