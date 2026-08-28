use crate::core::{CompletionItem, CompletionStatus, Controller};
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

/// Generic list popup rendered above the prompt. Currently used for
/// `@`-path completion; reusable for any short list anchored at the prompt.
#[derive(Debug, Default)]
pub struct PopupList;

impl PopupList {
    /// Popup area sitting directly above `prompt`, spanning the frame width.
    ///
    /// Anchored to the prompt rather than the cursor so it never covers the
    /// status line, which is what describes the keys the popup has taken.
    /// Returns `None` when there is no room above.
    pub fn area_above(prompt: Rect, frame_area: Rect) -> Option<Rect> {
        let top = prompt.y.checked_sub(POPUP_ROWS)?;
        if top < frame_area.y || prompt.y > frame_area.bottom() {
            return None;
        }
        Some(Rect {
            x: frame_area.x,
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

        let lines = completion
            .items(start, VISIBLE_ROWS)
            .iter()
            .enumerate()
            .map(|(offset, item)| item_line(item, start + offset == selected))
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(Text::from(lines))
                .style(Style::default().bg(theme::EDITOR_BG))
                .block(Block::default().padding(Padding::new(2, 2, 1, 1))),
            area,
        );
    }
}

fn item_line(item: &CompletionItem, is_selected: bool) -> Line<'static> {
    let (marker, label) = if is_selected {
        ("› ", theme::SELECTION_FG)
    } else {
        ("  ", theme::EDITOR_FG)
    };

    Line::from(vec![
        Span::styled(marker, Style::default().fg(theme::PROMPT_FG)),
        Span::styled(item.display.clone(), Style::default().fg(label)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Anchoring above the prompt is what keeps the status line, which sits
    /// inside the prompt area, out from under the popup.
    #[test]
    fn popup_sits_directly_above_the_prompt() {
        let frame = Rect::new(0, 0, 80, 24);
        let prompt = Rect::new(0, 16, 80, 8);

        let area = PopupList::area_above(prompt, frame).unwrap();

        assert_eq!(area.height, POPUP_ROWS);
        assert_eq!(area.bottom(), prompt.y);
        assert!(area.bottom() <= prompt.y, "overlaps the prompt");
    }

    #[test]
    fn no_room_above_the_prompt_means_no_popup() {
        let frame = Rect::new(0, 0, 80, 24);
        // Not enough rows above the prompt for the fixed height.
        assert!(PopupList::area_above(Rect::new(0, 3, 80, 8), frame).is_none());
        assert!(PopupList::area_above(Rect::new(0, 0, 80, 8), frame).is_none());
        // Prompt off the bottom of the frame.
        assert!(PopupList::area_above(Rect::new(0, 25, 80, 8), frame).is_none());
    }
}
