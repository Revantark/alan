use crate::core::{CompletionItem, CompletionStatus, Controller};
use crate::views::UiState;
use crate::views::component::Component;
use crate::views::theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Padding, Paragraph};

/// Candidates shown at once, past which the list scrolls.
const VISIBLE_ROWS: usize = 5;

/// Blank space the popup's block keeps around its content
const CONTENT_PADDING: Padding = Padding::new(2, 2, 1, 1);

/// Generic list popup rendered above the prompt. Currently used for
/// `@`-path completion; reusable for any short list anchored at the prompt.
#[derive(Debug, Default)]
pub struct PopupList;

impl PopupList {
    /// The status shown in place of candidates, if any.
    fn message(status: CompletionStatus, item_count: usize) -> Option<String> {
        match status {
            CompletionStatus::Loading => Some("Loading…".to_owned()),
            CompletionStatus::Error(error) => Some(error),
            CompletionStatus::Ready if item_count == 0 => Some("No matches".to_owned()),
            CompletionStatus::Ready => None,
        }
    }

    /// Rows the content will occupy, so the box never reserves space for
    /// candidates that are not there.
    pub fn required_rows(status: CompletionStatus, item_count: usize) -> u16 {
        if Self::message(status, item_count).is_some() {
            return 1;
        }
        item_count.min(VISIBLE_ROWS) as u16
    }

    /// Popup area sitting directly above `prompt`, spanning the frame width and
    /// tall enough for `rows` of content plus its padding.
    ///
    /// Anchored to the prompt rather than the cursor so it never covers the
    /// status line, which is what describes the keys the popup has taken.
    /// Returns `None` when there is no room above.
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
        if let Some(message) = Self::message(completion.status(), completion.item_count()) {
            frame.render_widget(
                Paragraph::new(message)
                    .style(Style::default().bg(theme::EDITOR_BG))
                    .block(Block::default().padding(CONTENT_PADDING)),
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
                .block(Block::default().padding(CONTENT_PADDING)),
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

        let area = PopupList::area_above(prompt, frame, VISIBLE_ROWS as u16).unwrap();

        assert_eq!(
            area.height,
            VISIBLE_ROWS as u16 + CONTENT_PADDING.top + CONTENT_PADDING.bottom
        );
        assert_eq!(area.bottom(), prompt.y);
        assert!(area.bottom() <= prompt.y, "overlaps the prompt");
    }

    #[test]
    fn no_room_above_the_prompt_means_no_popup() {
        let frame = Rect::new(0, 0, 80, 24);
        let rows = VISIBLE_ROWS as u16;
        // Not enough rows above the prompt for the height asked for.
        assert!(PopupList::area_above(Rect::new(0, 3, 80, 8), frame, rows).is_none());
        assert!(PopupList::area_above(Rect::new(0, 0, 80, 8), frame, rows).is_none());
        // Prompt off the bottom of the frame.
        assert!(PopupList::area_above(Rect::new(0, 25, 80, 8), frame, rows).is_none());
    }

    /// The box reserves exactly what [`PopupList::render`] will draw, so a
    /// short list leaves no dead rows and a long one does not run off-screen.
    #[test]
    fn required_rows_tracks_what_will_be_drawn() {
        use CompletionStatus::{Error, Loading, Ready};

        assert_eq!(PopupList::required_rows(Ready, 3), 3);
        assert_eq!(PopupList::required_rows(Ready, 99), VISIBLE_ROWS as u16);
        // A status message is one row however many candidates sit behind it.
        assert_eq!(PopupList::required_rows(Ready, 0), 1);
        assert_eq!(PopupList::required_rows(Loading, 9), 1);
        assert_eq!(PopupList::required_rows(Error("nope".into()), 9), 1);
    }
}
