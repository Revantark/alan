use crate::views::theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Padding, Paragraph};
use tui::{Component, RenderContext};

const VISIBLE_ROWS: usize = 5;
const CONTENT_PADDING: Padding = Padding::new(2, 2, 1, 1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PopupStatus {
    Loading,
    Ready,
    Error(String),
}

#[derive(Debug, Default)]
pub struct PopupList {
    open: bool,
    status: Option<PopupStatus>,
    items: Vec<String>,
    selected: usize,
}

impl PopupList {
    pub fn set(&mut self, open: bool, status: PopupStatus, items: Vec<String>, selected: usize) {
        self.open = open;
        self.status = Some(status);
        self.items = items;
        self.selected = selected;
    }

    pub fn matches_snapshot(
        &self,
        open: bool,
        status: &PopupStatus,
        items: &[String],
        selected: usize,
    ) -> bool {
        self.open == open
            && self.status.as_ref() == Some(status)
            && self.items == items
            && self.selected == selected
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

        if let Some(message) = self.message() {
            frame.render_widget(
                Paragraph::new(message)
                    .style(Style::default().bg(theme::EDITOR_BG))
                    .block(Block::default().padding(CONTENT_PADDING)),
                area,
            );
            return;
        }

        frame.render_widget(ratatui::widgets::Clear, area);
        let selected = self.selected.min(self.items.len() - 1);
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

    fn message(&self) -> Option<String> {
        match self.status.as_ref()? {
            PopupStatus::Loading => Some("Loading…".to_owned()),
            PopupStatus::Error(error) => Some(error.clone()),
            PopupStatus::Ready if self.items.is_empty() => Some("No matches".to_owned()),
            PopupStatus::Ready => None,
        }
    }
}

impl<A: 'static> Component<A> for PopupList {
    fn render(&self, frame: &mut Frame, area: Rect, _: &RenderContext<'_, A>) {
        self.render_into(frame, area);
    }
}

fn item_line(display: &str, selected: bool) -> Line<'static> {
    let (marker, color) = if selected {
        ("› ", theme::SELECTION_FG)
    } else {
        ("  ", theme::EDITOR_FG)
    };
    Line::from(vec![
        Span::styled(marker, Style::default().fg(theme::PROMPT_FG)),
        Span::styled(display.to_owned(), Style::default().fg(color)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn popup_sits_above_prompt() {
        let area =
            PopupList::area_above(Rect::new(0, 16, 80, 8), Rect::new(0, 0, 80, 24), 5).unwrap();
        assert_eq!(area.bottom(), 16);
        assert_eq!(area.height, 7);
    }

    #[test]
    fn no_room_above_prompt_means_no_popup() {
        let frame = Rect::new(0, 0, 80, 24);
        assert!(PopupList::area_above(Rect::new(0, 3, 80, 8), frame, 5).is_none());
        assert!(PopupList::area_above(Rect::new(0, 25, 80, 8), frame, 5).is_none());
    }

    #[test]
    fn snapshot_comparison_avoids_redundant_updates() {
        let mut popup = PopupList::default();
        popup.set(true, PopupStatus::Ready, vec!["a".into()], 0);
        assert!(popup.matches_snapshot(true, &PopupStatus::Ready, &["a".into()], 0));
        assert!(!popup.matches_snapshot(false, &PopupStatus::Ready, &[], 0));
    }
}
