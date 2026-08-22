use crate::core::{CompletionState, Controller};
use crate::views::UiState;
use crate::views::component::Component;
use crate::views::theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Padding, Paragraph};

/// Fixed number of rows in the completion popup.
const POPUP_ROWS: u16 = 7;

/// Generic list popup rendered above the editor cursor. Currently used for
/// `@`-path completion; reusable for any short list anchored at the prompt.
#[derive(Debug, Default)]
pub struct PopupList;

impl PopupList {
    /// Popup area whose bottom edge sits directly above `cursor`, spanning
    /// the editor column (the frame minus the prompt gutter). Returns `None`
    /// when there is no room to show it.
    pub fn area_above_cursor(cursor: Rect, frame_area: Rect) -> Option<Rect> {
        let top = cursor.y.checked_sub(POPUP_ROWS)?;
        if top < frame_area.y || cursor.y >= frame_area.bottom() {
            return None;
        }
        let x = frame_area.x;
        Some(Rect {
            x,
            y: top,
            width: frame_area.width,
            height: POPUP_ROWS,
        })
    }
}

impl Component for PopupList {
    fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        controller: &Controller,
        _state: &mut UiState,
    ) {
        let Some(completion) = controller.completion().state() else {
            return;
        };
        let CompletionState {
            items,
            selected,
            status,
        } = completion;
        if area.is_empty() {
            return;
        }
        if !matches!(status, crate::core::CompletionStatus::Ready) {
            let message = match status {
                crate::core::CompletionStatus::Loading => "Loading…".to_owned(),
                crate::core::CompletionStatus::Error(error) => error.clone(),
                crate::core::CompletionStatus::Ready => String::new(),
            };
            frame.render_widget(
                Paragraph::new(message)
                    .style(Style::default().bg(theme::EDITOR_BG))
                    .block(Block::default().padding(Padding::new(2, 2, 1, 1))),
                area,
            );
            return;
        }
        if items.is_empty() {
            frame.render_widget(
                Paragraph::new("No matches")
                    .style(Style::default().bg(theme::EDITOR_BG))
                    .block(Block::default().padding(Padding::new(2, 2, 1, 1))),
                area,
            );
            return;
        }

        frame.render_widget(ratatui::widgets::Clear, area);
        let start = selected
            .saturating_sub(2)
            .min(items.len().saturating_sub(5));
        let end = (start + 5).min(items.len());

        let rows = items[start..end]
            .iter()
            .enumerate()
            .map(|(offset, entry)| {
                let index = start + offset;
                let (marker, marker_style) = if index == *selected {
                    ("› ", Style::default().fg(theme::PROMPT_FG))
                } else {
                    ("  ", Style::default())
                };
                let name = if entry.is_dir {
                    format!("{}/", entry.path)
                } else {
                    entry.path.clone()
                };
                let name_style = if entry.is_dir {
                    Style::default().fg(ratatui::style::Color::White)
                } else {
                    Style::default().fg(theme::EDITOR_FG)
                };
                Line::from(vec![
                    Span::styled(marker, marker_style),
                    Span::styled(name, name_style),
                ])
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(Text::from(rows))
                .style(Style::default().bg(theme::EDITOR_BG))
                .block(Block::default().padding(Padding::new(2, 2, 1, 1))),
            area,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn popup_sits_directly_above_cursor() {
        let frame = Rect::new(0, 0, 80, 24);
        let cursor = Rect::new(0, 20, 1, 1);
        let area = PopupList::area_above_cursor(cursor, frame).unwrap();
        assert_eq!(area.height, POPUP_ROWS);
        assert_eq!(area.bottom(), cursor.y);
    }

    #[test]
    fn no_room_above_cursor_means_no_popup() {
        let frame = Rect::new(0, 0, 80, 24);
        // Not enough rows above the cursor for the fixed height.
        assert!(PopupList::area_above_cursor(Rect::new(0, 3, 1, 1), frame).is_none());
        assert!(PopupList::area_above_cursor(Rect::new(0, 0, 1, 1), frame).is_none());
        // Cursor off the bottom of the frame.
        assert!(PopupList::area_above_cursor(Rect::new(0, 24, 1, 1), frame).is_none());
    }
}
