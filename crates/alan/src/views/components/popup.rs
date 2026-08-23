use crate::core::{CompletionStatus, Controller};
use crate::views::UiState;
use crate::views::component::Component;
use crate::views::theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Padding, Paragraph};

/// Fixed height of the completion popup, including its padding.
const POPUP_ROWS: u16 = 7;
/// Candidates visible inside that height.
const VISIBLE_ROWS: usize = 5;

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
        let completion = controller.completion();
        if !completion.is_open() || area.is_empty() {
            return;
        }
        let message = match completion.status() {
            CompletionStatus::Loading => Some("Loading…".to_owned()),
            CompletionStatus::Error(error) => Some(error),
            CompletionStatus::Ready if completion.item_count() == 0 => {
                Some("No matches".to_owned())
            }
            CompletionStatus::Ready => None,
        };
        if let Some(message) = message {
            frame.render_widget(
                Paragraph::new(message)
                    .style(Style::default().bg(theme::EDITOR_BG))
                    .block(Block::default().padding(Padding::new(2, 2, 1, 1))),
                area,
            );
            return;
        }

        frame.render_widget(ratatui::widgets::Clear, area);
        let selected = completion.selected();
        let start = selected
            .saturating_sub(2)
            .min(completion.item_count().saturating_sub(VISIBLE_ROWS));

        let rows = completion
            .items(start, VISIBLE_ROWS)
            .iter()
            .enumerate()
            .map(|(offset, item)| {
                let (marker, marker_style) = if start + offset == selected {
                    ("› ", Style::default().fg(theme::PROMPT_FG))
                } else {
                    ("  ", Style::default())
                };
                // Directories carry a trailing `/`.
                let label_style = if item.display.ends_with('/') {
                    Style::default().fg(ratatui::style::Color::White)
                } else {
                    Style::default().fg(theme::EDITOR_FG)
                };
                let mut spans = vec![
                    Span::styled(marker, marker_style),
                    Span::styled(item.display.clone(), label_style),
                ];
                if let Some(description) = &item.description {
                    spans.push(Span::styled(
                        format!("  {description}"),
                        Style::default().fg(theme::MUTED_FG),
                    ));
                }
                Line::from(spans)
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
