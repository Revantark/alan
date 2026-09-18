//! Selection showcase: mouse text-selection with framework area routing.
//!
//! ```text
//! mouse event -> hit_test (render-time area cache, topmost wins)
//!            -> handle_mouse on the hit component (bubbles on Continue)
//! ```
//!
//! Three independent components own selection state and highlight their own
//! rendering; the framework only routes events by recorded areas. Panes:
//!
//! * Text pane (left): drag-select with word-mode double-click, wheel scroll.
//! * List pane (left, below): click moves the highlighted row (move-only).
//! * Notes pane (right): click-to-position + drag-select over note lines.
//! * Status bar: which entity last consumed a mouse event, and the selection.
//! * Overlay (`o`): modal with its own selectable text; consumes all mouse.
//!
//! Keys: `o` toggle overlay, `c` clear selections, `q` quit.

use alan_tui::context::Context;
use alan_tui::entity::Entity;
use alan_tui::keymap::KeyMapper;
use alan_tui::selection::{
    Selection, apply_selection_to_lines, extract_selected_text, find_word_bounds_at, rect_contains,
};
use alan_tui::{ActionStatus, Component, InputContext, RenderContext, Runtime};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use std::time::Duration;

type Cx<'a, T> = Context<'a, T, Action>;

const SEL_BG: Color = Color::Blue;
const SEL_FG: Color = Color::White;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Quit,
    ToggleOverlay,
    ClearSelections,
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
            KeyCode::Char('o') => Some(Action::ToggleOverlay),
            KeyCode::Char('c') => Some(Action::ClearSelections),
            _ => None,
        }
    }
}

/// Converts a screen point to a (line, col) text position inside the *inner*
/// area of a bordered block (the area the lines actually render into),
/// honouring `scroll_offset`. Columns past the end of a line clamp to the
/// line length so drags that end in empty space still select to the end.
fn screen_to_text(inner: Rect, scroll_offset: usize, col: u16, row: u16) -> (usize, usize) {
    let line = scroll_offset
        + (row.saturating_sub(inner.y) as usize).min(inner.height.max(1) as usize - 1);
    let col = col.saturating_sub(inner.x) as usize;
    (line, col)
}

/// The inner render area of a bordered block with the given title.
fn block_inner(area: Rect, title: &str) -> Rect {
    Block::default()
        .borders(Borders::ALL)
        .title(title.to_string())
        .inner(area)
}

fn line_display_width(line: &Line<'static>) -> usize {
    line.spans.iter().map(|s| s.content.width()).sum()
}

use unicode_width::UnicodeWidthStr;

// ---------------------------------------------------------------------------
// Text pane: drag-select with double-click word mode and wheel scrolling.
// ---------------------------------------------------------------------------

struct TextPane {
    lines: Vec<Line<'static>>,
    scroll: usize,
    selection: Option<Selection>,
    last_click: Option<(std::time::Instant, u16, u16)>,
}

impl TextPane {
    fn new() -> Self {
        let text = [
            "Mouse selection showcase",
            "",
            "Drag with the left button to select text in this pane.",
            "Double-click selects a whole word, then drag to extend it",
            "word by word. The wheel scrolls the viewport.",
            "",
            "Selection lives entirely inside this component: the",
            "framework only routes the mouse event here because the",
            "pointer hit the area recorded during the last render.",
            "",
            "Press c to clear all selections, o to open the overlay,",
            "q to quit. The status bar reports the last consumer.",
        ];
        Self {
            lines: text.iter().map(|l| Line::from(l.to_string())).collect(),
            scroll: 0,
            selection: None,
            last_click: None,
        }
    }

    /// Shared title so hit-testing and rendering agree on the border geometry.
    const TITLE: &str = " text (drag-select, double-click = word) ";

    fn drag_to(&mut self, area: Rect, col: u16, row: u16) {
        let (line, col) = screen_to_text(block_inner(area, Self::TITLE), self.scroll, col, row);
        let col = col.min(self.lines.get(line).map(line_display_width).unwrap_or(0));
        match &mut self.selection {
            Some(sel) => sel.update_cursor(alan_tui::TextPosition::new(line, col), &self.lines),
            None => {
                self.selection = Some(Selection::new(alan_tui::TextPosition::new(line, col)));
            }
        }
    }

    fn click(&mut self, area: Rect, col: u16, row: u16) {
        let now = std::time::Instant::now();
        let double = self
            .last_click
            .take_if(|(t, c, r)| {
                now.duration_since(*t) < std::time::Duration::from_millis(400)
                    && *c == col
                    && *r == row
            })
            .is_some();
        if double {
            let (line, scol) =
                screen_to_text(block_inner(area, Self::TITLE), self.scroll, col, row);
            if let Some(line_ref) = self.lines.get(line) {
                let (start, end) = find_word_bounds_at(line_ref, scol);
                self.selection = Some(Selection::new_word(
                    alan_tui::TextPosition::new(line, scol),
                    start,
                    end,
                ));
            }
        } else {
            self.selection = None;
            let (line, col) = screen_to_text(block_inner(area, Self::TITLE), self.scroll, col, row);
            self.selection = Some(Selection::new(alan_tui::TextPosition::new(
                line,
                col.min(self.lines.get(line).map(line_display_width).unwrap_or(0)),
            )));
        }
        self.last_click = Some((now, col, row));
    }

    fn selected_text(&self) -> String {
        self.selection
            .as_ref()
            .map(|s| extract_selected_text(&self.lines, s))
            .unwrap_or_default()
    }
}

impl Component<Action> for TextPane {
    fn handle_mouse(
        &mut self,
        mouse: MouseEvent,
        area: Rect,
        cx: &mut Cx<'_, Self>,
    ) -> ActionStatus {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => self.click(area, mouse.column, mouse.row),
            MouseEventKind::Drag(MouseButton::Left) => self.drag_to(area, mouse.column, mouse.row),
            MouseEventKind::ScrollUp => self.scroll = self.scroll.saturating_sub(1),
            MouseEventKind::ScrollDown => {
                let max = self
                    .lines
                    .len()
                    .saturating_sub(block_inner(area, Self::TITLE).height as usize);
                self.scroll = (self.scroll + 1).min(max);
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(sel) = &mut self.selection {
                    sel.is_dragging = false;
                }
            }
            _ => return ActionStatus::Continue,
        }
        cx.notify();
        ActionStatus::Handled
    }

    fn render(&self, frame: &mut Frame, area: Rect, _: &RenderContext<'_, '_, Action>) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(TextPane::TITLE);
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let visible: Vec<Line<'static>> = self
            .lines
            .iter()
            .skip(self.scroll)
            .take(inner.height as usize)
            .cloned()
            .collect();
        let highlighted = apply_selection_to_lines(
            visible,
            self.scroll,
            self.selection.as_ref(),
            SEL_BG,
            SEL_FG,
        );
        frame.render_widget(Paragraph::new(highlighted), inner);
    }
}

// ---------------------------------------------------------------------------
// List pane: click moves the highlighted row (move-only, no auto-accept).
// ---------------------------------------------------------------------------

struct ListPane {
    items: Vec<String>,
    selected: usize,
}

impl ListPane {
    fn new() -> Self {
        Self {
            items: (1..=8)
                .map(|i| format!("item {i} — click to highlight"))
                .collect(),
            selected: 0,
        }
    }

    /// Shared title so hit-testing and rendering agree on the border geometry.
    const TITLE: &str = " list (click to move) ";

    fn row_at(&self, area: Rect, row: u16) -> Option<usize> {
        let inner = block_inner(area, Self::TITLE);
        let idx = row.saturating_sub(inner.y) as usize;
        (idx < self.items.len().min(inner.height as usize)).then_some(idx)
    }
}

impl Component<Action> for ListPane {
    fn handle_mouse(
        &mut self,
        mouse: MouseEvent,
        area: Rect,
        cx: &mut Cx<'_, Self>,
    ) -> ActionStatus {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let Some(idx) = self.row_at(area, mouse.row) else {
                    return ActionStatus::Handled;
                };
                self.selected = idx;
            }
            MouseEventKind::ScrollUp => {
                self.selected = self.selected.saturating_sub(1);
            }
            MouseEventKind::ScrollDown => {
                self.selected = (self.selected + 1).min(self.items.len() - 1);
            }
            _ => return ActionStatus::Continue,
        }
        cx.notify();
        ActionStatus::Handled
    }

    fn render(&self, frame: &mut Frame, area: Rect, _: &RenderContext<'_, '_, Action>) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(ListPane::TITLE);
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let lines: Vec<Line<'static>> = self
            .items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                if i == self.selected {
                    Line::styled(item.clone(), Style::default().bg(SEL_BG).fg(SEL_FG))
                } else {
                    Line::raw(item.clone())
                }
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

// ---------------------------------------------------------------------------
// Notes pane: click-to-position caret + drag-select over note lines.
// ---------------------------------------------------------------------------

struct NotesPane {
    lines: Vec<Line<'static>>,
    scroll: usize,
    caret: Option<(usize, usize)>,
    selection: Option<Selection>,
}

impl NotesPane {
    /// Shared title so hit-testing and rendering agree on the border geometry.
    const TITLE: &str = " notes (click caret, drag-select) ";

    fn new() -> Self {
        let text = [
            "Notes",
            "",
            "This pane shows click-to-position: clicking moves the",
            "caret (shown as a reversed block) and dragging selects.",
            "",
            "Two components can own selection independently — routing",
            "is decided purely by which recorded area the pointer hit.",
        ];
        Self {
            lines: text.iter().map(|l| Line::from(l.to_string())).collect(),
            scroll: 0,
            caret: None,
            selection: None,
        }
    }

    fn selected_text(&self) -> String {
        self.selection
            .as_ref()
            .map(|s| extract_selected_text(&self.lines, s))
            .unwrap_or_default()
    }
}

impl Component<Action> for NotesPane {
    fn handle_mouse(
        &mut self,
        mouse: MouseEvent,
        area: Rect,
        cx: &mut Cx<'_, Self>,
    ) -> ActionStatus {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let (line, col) = screen_to_text(
                    block_inner(area, Self::TITLE),
                    self.scroll,
                    mouse.column,
                    mouse.row,
                );
                let col = col.min(self.lines.get(line).map(line_display_width).unwrap_or(0));
                self.caret = Some((line, col));
                self.selection = Some(Selection::new(alan_tui::TextPosition::new(line, col)));
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let (line, col) = screen_to_text(
                    block_inner(area, Self::TITLE),
                    self.scroll,
                    mouse.column,
                    mouse.row,
                );
                let col = col.min(self.lines.get(line).map(line_display_width).unwrap_or(0));
                let pos = alan_tui::TextPosition::new(line, col);
                match &mut self.selection {
                    Some(sel) => sel.update_cursor(pos, &self.lines),
                    None => self.selection = Some(Selection::new(pos)),
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(sel) = &mut self.selection {
                    sel.is_dragging = false;
                }
            }
            MouseEventKind::ScrollUp => self.scroll = self.scroll.saturating_sub(1),
            MouseEventKind::ScrollDown => {
                let max = self
                    .lines
                    .len()
                    .saturating_sub(block_inner(area, Self::TITLE).height as usize);
                self.scroll = (self.scroll + 1).min(max);
            }
            _ => return ActionStatus::Continue,
        }
        cx.notify();
        ActionStatus::Handled
    }

    fn render(&self, frame: &mut Frame, area: Rect, _: &RenderContext<'_, '_, Action>) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(NotesPane::TITLE);
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let visible: Vec<Line<'static>> = self
            .lines
            .iter()
            .skip(self.scroll)
            .take(inner.height as usize)
            .cloned()
            .collect();

        let mut highlighted = apply_selection_to_lines(
            visible,
            self.scroll,
            self.selection.as_ref(),
            SEL_BG,
            SEL_FG,
        );

        // Render the caret as a reversed cell, if on screen.
        if let Some((line, col)) = self.caret
            && line >= self.scroll
        {
            let rel = line - self.scroll;
            if rel < highlighted.len() {
                let text = highlighted[rel]
                    .spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<String>();
                let mut styled: Vec<Span> = Vec::new();
                for (cur, ch) in text.chars().enumerate() {
                    let style = if cur == col {
                        Style::default().bg(SEL_FG).fg(SEL_BG)
                    } else {
                        Style::default()
                    };
                    styled.push(Span::styled(ch.to_string(), style));
                }
                highlighted[rel] = Line::from(styled);
            }
        }

        frame.render_widget(Paragraph::new(highlighted), inner);
    }
}

// ---------------------------------------------------------------------------
// Status bar: last mouse consumer + extracted selection.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct StatusBar {
    last_consumer: Option<String>,
    last_kind: Option<String>,
    selected: String,
}

impl Component<Action> for StatusBar {
    fn render(&self, frame: &mut Frame, area: Rect, _: &RenderContext<'_, '_, Action>) {
        let consumer = self.last_consumer.as_deref().unwrap_or("none");
        let kind = self.last_kind.as_deref().unwrap_or("-");
        let text = Line::from(vec![
            Span::styled(
                " last mouse: ",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("{consumer} ({kind}) ")),
            Span::styled(
                "| selection: ",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(if self.selected.is_empty() {
                "—".to_string()
            } else {
                self.selected.replace('\n', " ⏎ ")
            }),
        ]);
        frame.render_widget(Paragraph::new(text), area);
    }
}

// ---------------------------------------------------------------------------
// Overlay: consumes all mouse events over the whole frame.
// ---------------------------------------------------------------------------

struct ModalOverlay {
    selection: Option<Selection>,
    lines: Vec<Line<'static>>,
}

impl ModalOverlay {
    fn new() -> Self {
        let text = [
            "",
            "  This overlay registers the full frame area, so mouse",
            "  events anywhere route here while it is open — clicks",
            "  outside the box never reach the panes below.",
            "",
            "  Drag inside this box to select this text. Press Esc",
            "  or o to close.",
            "",
        ];
        Self {
            lines: text.iter().map(|l| Line::from(l.to_string())).collect(),
            selection: None,
        }
    }

    fn popup(&self, frame_area: Rect) -> Rect {
        let width = 58.min(frame_area.width.saturating_sub(2));
        let height = (self.lines.len() as u16 + 2).min(frame_area.height.saturating_sub(2));
        Rect {
            x: frame_area.x + (frame_area.width - width) / 2,
            y: frame_area.y + (frame_area.height - height) / 2,
            width,
            height,
        }
    }
}

impl Component<Action> for ModalOverlay {
    fn handle_action(&mut self, action: &Action, cx: &mut Cx<'_, Self>) -> ActionStatus {
        if matches!(
            action,
            Action::ToggleOverlay | Action::Quit | Action::ClearSelections
        ) {
            cx.close_overlay();
            return ActionStatus::Handled;
        }
        ActionStatus::Handled
    }

    fn handle_mouse(
        &mut self,
        mouse: MouseEvent,
        area: Rect,
        cx: &mut Cx<'_, Self>,
    ) -> ActionStatus {
        let popup = self.popup(area);
        let inner = Rect {
            x: popup.x + 1,
            y: popup.y + 1,
            width: popup.width.saturating_sub(2),
            height: popup.height.saturating_sub(2),
        };
        // Clicks outside the box still stop here (full-frame area) but are
        // otherwise ignored.
        if !rect_contains(inner, mouse.column, mouse.row) {
            return ActionStatus::Handled;
        }
        let (line, col) = screen_to_text(inner, 0, mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.selection = Some(Selection::new(alan_tui::TextPosition::new(
                    line,
                    col.min(self.lines.get(line).map(line_display_width).unwrap_or(0)),
                )));
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let pos = alan_tui::TextPosition::new(line, col);
                match &mut self.selection {
                    Some(sel) => sel.update_cursor(pos, &self.lines),
                    None => self.selection = Some(Selection::new(pos)),
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(sel) = &mut self.selection {
                    sel.is_dragging = false;
                }
            }
            _ => return ActionStatus::Handled,
        }
        cx.notify();
        ActionStatus::Handled
    }

    fn render(&self, frame: &mut Frame, area: Rect, _: &RenderContext<'_, '_, Action>) {
        let popup = self.popup(area);
        frame.render_widget(Clear, popup);
        let block = Block::default().borders(Borders::ALL).title(" overlay ");
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        let highlighted = apply_selection_to_lines(
            self.lines.clone(),
            0,
            self.selection.as_ref(),
            SEL_BG,
            SEL_FG,
        );
        frame.render_widget(Paragraph::new(highlighted), inner);
    }
}

// ---------------------------------------------------------------------------
// Root: layout, overlay toggling, status aggregation.
// ---------------------------------------------------------------------------

struct Root {
    text: Option<Entity<TextPane>>,
    list: Option<Entity<ListPane>>,
    notes: Option<Entity<NotesPane>>,
    status: Option<Entity<StatusBar>>,
    overlay: Option<Entity<ModalOverlay>>,
}

impl Root {
    fn new() -> Self {
        Self {
            text: None,
            list: None,
            notes: None,
            status: None,
            overlay: None,
        }
    }
}

impl Component<Action> for Root {
    fn init(&mut self, cx: &mut Context<'_, Self, Action>) {
        let text = cx.insert(TextPane::new());
        let list = cx.insert(ListPane::new());
        let notes = cx.insert(NotesPane::new());
        let status = cx.insert(StatusBar::default());
        self.text = Some(text);
        self.list = Some(list);
        self.notes = Some(notes);
        self.status = Some(status);

        // Observe each pane so mouse activity there refreshes the status bar.
        // The panes notify() on every handled mouse event.
        let text_id = text;
        let status_entity = status;
        let _sub = cx.observe(text, move |_root, _source, cx| {
            let _ = cx.update(status_entity, |bar| {
                bar.last_consumer = Some("text pane".to_string());
            });
            let selected = cx.read(text_id, |pane: &TextPane| pane.selected_text());
            if let Some(selected) = selected {
                let _ = cx.update(status_entity, move |bar| bar.selected = selected);
            }
            cx.notify();
        });
        let _sub = cx.observe(list, move |_root, _source, cx| {
            let _ = cx.update(status_entity, |bar| {
                bar.last_consumer = Some("list pane".to_string());
            });
            cx.notify();
        });
        let notes_id = notes;
        let _sub = cx.observe(notes, move |_root, _source, cx| {
            let _ = cx.update(status_entity, |bar| {
                bar.last_consumer = Some("notes pane".to_string());
            });
            let selected = cx.read(notes_id, |pane: &NotesPane| pane.selected_text());
            if let Some(selected) = selected {
                let _ = cx.update(status_entity, move |bar| bar.selected = selected);
            }
            cx.notify();
        });
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
            Action::ToggleOverlay => {
                if self.overlay.is_some() {
                    cx.close_overlay();
                    self.overlay = None;
                } else {
                    self.overlay = Some(cx.open_overlay(ModalOverlay::new()));
                }
                cx.notify();
                ActionStatus::Handled
            }
            Action::ClearSelections => {
                if let Some(text) = self.text {
                    let _ = cx.update(text, |pane| pane.selection = None);
                }
                if let Some(notes) = self.notes {
                    let _ = cx.update(notes, |pane| pane.selection = None);
                }
                if let Some(status) = self.status {
                    let _ = cx.update(status, |bar| bar.selected.clear());
                }
                cx.notify();
                ActionStatus::Handled
            }
        }
    }

    fn render(&self, frame: &mut Frame, area: Rect, cx: &RenderContext<'_, '_, Action>) {
        let [main, status_row] =
            Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(area);

        let [left_col, right_col] =
            Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
                .areas(main);

        let [text_area, list_area] =
            Layout::vertical([Constraint::Percentage(65), Constraint::Percentage(35)])
                .areas(left_col);

        if let (Some(text), Some(list), Some(notes), Some(status)) =
            (self.text, self.list, self.notes, self.status)
        {
            cx.render_entity(text, frame, text_area);
            cx.render_entity(list, frame, list_area);
            cx.render_entity(notes, frame, right_col);
            cx.render_entity(status, frame, status_row);
        }
    }
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
