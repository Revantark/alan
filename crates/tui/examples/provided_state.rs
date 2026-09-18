//! `render_with_state`: a parent provides slices of its own state to child
//! components by immutable reference — no clones, no shared handles.
//!
//! `Dashboard` owns one big `Model`. Three children each need a different
//! part of it, and the parent hands each child exactly the slice it needs:
//!
//! - `Header`    gets `&self.model.title`
//! - `ItemList`  gets `&self.model.items`
//! - `StatusBar` gets `&self.model.status`
//!
//! Each child reads its slice back with `cx.state::<T>()` during render.
//! The children never see the whole `Model`, and the parent keeps sole
//! ownership and mutability.
//!
//! The example also demonstrates the two context rules:
//! - a child rendered with plain `render_entity` (the `Orphan`) sees no
//!   provided state (`cx.state::<T>() == None`);
//! - a child that itself calls `render_with_state` (`Nested`) shadows the
//!   outer state for its own descendants (`Leaf`).
//!
//! Keys: `a` append an item, `s` change the status, `Esc` quit.

use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Widget};
use tui::context::Context;
use tui::entity::Entity;
use tui::{ActionStatus, Component, PassthroughMapper, RenderContext, Runtime};

/// The one big state owned by the parent.
struct Model {
    title: String,
    items: Vec<String>,
    status: String,
}

struct Dashboard {
    model: Model,
    header: Option<Entity<Header>>,
    list: Option<Entity<ItemList>>,
    status: Option<Entity<StatusBar>>,
    orphan: Option<Entity<Orphan>>,
    nested: Option<Entity<Nested>>,
}

impl Dashboard {
    fn new() -> Self {
        Self {
            model: Model {
                title: "render_with_state demo".to_string(),
                items: vec!["item 1".to_string(), "item 2".to_string()],
                status: "press a to add an item, s to poke the status".to_string(),
            },
            header: None,
            list: None,
            status: None,
            orphan: None,
            nested: None,
        }
    }
}

impl Component<Event> for Dashboard {
    fn init(&mut self, cx: &mut Context<'_, Self, Event>) {
        self.header = Some(cx.insert(Header));
        self.list = Some(cx.insert(ItemList));
        self.status = Some(cx.insert(StatusBar));
        self.orphan = Some(cx.insert(Orphan));
        self.nested = Some(cx.insert(Nested { leaf: None }));
    }

    fn handle_action(&mut self, event: &Event, cx: &mut Context<'_, Self, Event>) -> ActionStatus {
        let Event::Key(key) = event else {
            return ActionStatus::Handled;
        };
        if key.kind != KeyEventKind::Press {
            return ActionStatus::Handled;
        }
        match key.code {
            KeyCode::Esc => cx.quit(),
            KeyCode::Char('a') => {
                self.model
                    .items
                    .push(format!("item {}", self.model.items.len() + 1));
                self.model.status = format!("{} items", self.model.items.len());
                cx.notify();
            }
            KeyCode::Char('s') => {
                self.model.status = format!("status changed ({} items)", self.model.items.len());
                cx.notify();
            }
            _ => {}
        }
        ActionStatus::Handled
    }

    fn render(&self, frame: &mut Frame, area: Rect, cx: &RenderContext<'_, '_, Event>) {
        let [header_area, body, status_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .areas(area);
        let [list_area, side] =
            Layout::horizontal([Constraint::Percentage(60), Constraint::Fill(1)]).areas(body);

        // Data down: each child gets exactly the slice it needs, by ref.
        if let (Some(header), Some(list), Some(status)) = (self.header, self.list, self.status) {
            cx.render_with_state(header, frame, header_area, &self.model.title);
            cx.render_with_state(list, frame, list_area, &self.model.items);
            cx.render_with_state(status, frame, status_area, &self.model.status);
        }

        // A child rendered without provided state sees none.
        if let Some(orphan) = self.orphan {
            cx.render_entity(orphan, frame, side);
        }
        // A child that provides its own state shadows the outer one for its
        // own subtree.
        if let Some(nested) = self.nested {
            cx.render_entity(nested, frame, side);
        }
    }
}

struct Header;

impl Component<Event> for Header {
    fn render(&self, frame: &mut Frame, area: Rect, cx: &RenderContext<'_, '_, Event>) {
        let title = cx.expect_state::<String>();
        Paragraph::new(title.as_str())
            .style(Style::default().add_modifier(Modifier::BOLD))
            .render(area, frame.buffer_mut());
    }
}

struct ItemList;

impl Component<Event> for ItemList {
    fn render(&self, frame: &mut Frame, area: Rect, cx: &RenderContext<'_, '_, Event>) {
        let Some(items) = cx.state::<Vec<String>>() else {
            Paragraph::new("(no items)").render(area, frame.buffer_mut());
            return;
        };
        let list: Vec<ListItem> = items
            .iter()
            .map(|item| ListItem::new(Line::from(item.clone())))
            .collect();
        List::new(list)
            .block(Block::new().borders(Borders::ALL).title("items"))
            .render(area, frame.buffer_mut());
    }
}

struct StatusBar;

impl Component<Event> for StatusBar {
    fn render(&self, frame: &mut Frame, area: Rect, cx: &RenderContext<'_, '_, Event>) {
        let status = cx
            .state::<String>()
            .map(String::as_str)
            .unwrap_or("(no status)");
        Paragraph::new(status)
            .style(Style::default().fg(Color::DarkGray))
            .render(area, frame.buffer_mut());
    }
}

/// Rendered via plain `render_entity`: it must observe `state() == None`.
struct Orphan;

impl Component<Event> for Orphan {
    fn render(&self, frame: &mut Frame, area: Rect, cx: &RenderContext<'_, '_, Event>) {
        let seen = cx.state::<String>().map(String::as_str);
        let text = match seen {
            Some(_) => "BUG: orphan saw provided state",
            None => "orphan: no state (correct)",
        };
        Paragraph::new(text)
            .style(Style::default().fg(Color::Yellow))
            .render(area, frame.buffer_mut());
    }
}

/// Rendered via plain `render_entity`, but provides its own state to its
/// child — shadowing anything the grandparent provided for that subtree.
struct Nested {
    leaf: Option<Entity<Leaf>>,
}

impl Component<Event> for Nested {
    fn init(&mut self, cx: &mut Context<'_, Self, Event>) {
        self.leaf = Some(cx.insert(Leaf));
    }

    fn render(&self, frame: &mut Frame, area: Rect, cx: &RenderContext<'_, '_, Event>) {
        let top = Rect { height: 1, ..area };
        let rest = Rect {
            y: area.y + 1,
            height: area.height.saturating_sub(1),
            ..area
        };
        Paragraph::new("nested provides its own state below:")
            .style(Style::default().fg(Color::Cyan))
            .render(top, frame.buffer_mut());
        if let Some(leaf) = self.leaf {
            // Shadows the dashboard's state for the leaf only.
            cx.render_with_state(leaf, frame, rest, &"shadowed".to_string());
        }
    }
}

struct Leaf;

impl Component<Event> for Leaf {
    fn render(&self, frame: &mut Frame, area: Rect, cx: &RenderContext<'_, '_, Event>) {
        let text = match cx.state::<String>().map(String::as_str) {
            Some(s) => format!("leaf sees: {s:?}"),
            None => "leaf: no state".to_string(),
        };
        Paragraph::new(text).render(area, frame.buffer_mut());
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            Runtime::builder(Dashboard::new())
                .key_mapper(PassthroughMapper)
                .tick_rate(Duration::from_millis(250))
                .build()
                .run()
                .await
        })?;
    Ok(())
}
