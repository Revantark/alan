//! Streaming example demonstrating `cx.subscribe_stream`:
//!
//! An external asynchronous stream is polled by a worker, then its items and
//! normal close are delivered to the owning component on the runtime side.

use std::collections::VecDeque;
use std::time::Duration;

use futures_util::Stream;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use tui::context::Context;
use tui::entity::Entity;
use tui::keymap::KeyMapper;
use tui::subscription::SubscriptionEvent;
use tui::{ActionStatus, Component, InputContext, RenderContext, Runtime, Subscription};

/// Semantic user input for this application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Quit,
    Start,
    Cancel,
}

struct AppKeyMapper;

impl KeyMapper<Action> for AppKeyMapper {
    fn map(&self, event: &crossterm::event::Event, _context: &InputContext) -> Option<Action> {
        use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};

        let Event::Key(key) = event else {
            return None;
        };
        if key.kind != KeyEventKind::Press {
            return None;
        }

        match (key.code, key.modifiers) {
            (KeyCode::Char('q'), KeyModifiers::NONE)
            | (KeyCode::Char('c'), KeyModifiers::CONTROL) => Some(Action::Quit),
            (KeyCode::Enter, KeyModifiers::NONE) => Some(Action::Start),
            (KeyCode::Esc, KeyModifiers::NONE) => Some(Action::Cancel),
            _ => None,
        }
    }
}

/// The text the simulated upstream "LLM" streams back, word by word.
const RESPONSE: &str = "Streaming responses arrive incrementally, so the interface can render each \
chunk as soon as it lands instead of waiting for the whole payload. Cancelling is cheap: the \
runtime discards queued deliveries, the worker stops polling, and no Closed event is ever \
delivered after cancellation.";

/// A simulated upstream token stream.
///
/// Errors are part of the item type (`Result<String, String>`): the framework
/// never turns a stream item into a runtime error. One transient failure is
/// injected mid-stream and the stream keeps going afterwards.
fn token_stream() -> impl Stream<Item = Result<String, String>> + Send + 'static {
    let mut tokens: VecDeque<Result<String, String>> = RESPONSE
        .split_whitespace()
        .map(|word| Ok(format!("{word} ")))
        .collect();
    tokens.insert(4, Err("transient upstream hiccup".to_owned()));

    futures_util::stream::unfold(tokens, |mut tokens| async move {
        // Simulate upstream latency between chunks.
        tokio::time::sleep(Duration::from_millis(120)).await;
        tokens.pop_front().map(|token| (token, tokens))
    })
}

/// The component that owns the stream subscription.
///
/// The subscription handle must be retained for as long as the stream should
/// run; dropping it (here: when a new stream starts) cancels the stream.
struct StreamView {
    generation: u32,
    streamed: String,
    status: String,
    subscription: Option<Subscription>,
}

impl StreamView {
    fn new() -> Self {
        Self {
            generation: 0,
            streamed: String::new(),
            status: "press enter to start a stream".to_owned(),
            subscription: None,
        }
    }

    fn start(&mut self, cx: &mut Context<'_, Self, Action>) {
        // Dropping the previous handle cancels that stream: the runtime
        // discards its queued deliveries and no Closed event arrives.
        self.subscription.take();

        self.generation += 1;
        self.streamed.clear();
        let generation = self.generation;

        // The callback runs on the runtime side, after the item crossed the
        // async boundary — never inside the stream worker. It receives the
        // component and its context, so it may mutate state and notify.
        self.subscription = Some(cx.subscribe_stream(token_stream(), move |event, view, cx| {
            match event {
                SubscriptionEvent::Item(Ok(token)) => {
                    view.streamed.push_str(&token);
                    cx.notify();
                }
                SubscriptionEvent::Item(Err(error)) => {
                    view.status = format!("item error: {error} (stream continues)");
                    cx.notify();
                }
                SubscriptionEvent::Closed => {
                    // The stream ended normally. Clear the handle so a later
                    // Cancel action does not claim a dead subscription.
                    view.subscription = None;
                    view.status = format!("stream #{generation} closed");
                    cx.notify();
                }
            }
        }));

        self.status = format!("streaming #{generation}…");
        cx.notify();
    }

    fn cancel(&mut self, cx: &mut Context<'_, Self, Action>) {
        let Some(subscription) = self.subscription.take() else {
            return;
        };
        subscription.cancel();
        self.status = format!(
            "stream #{} cancelled; no Closed event follows a cancel",
            self.generation
        );
        cx.notify();
    }
}

impl Component<Action> for StreamView {
    fn handle_action(
        &mut self,
        action: &Action,
        cx: &mut Context<'_, Self, Action>,
    ) -> ActionStatus {
        match action {
            Action::Start => {
                self.start(cx);
                ActionStatus::Handled
            }
            Action::Cancel if self.subscription.is_some() => {
                self.cancel(cx);
                ActionStatus::Handled
            }
            _ => ActionStatus::Continue,
        }
    }

    fn render(&self, frame: &mut Frame, area: Rect, cx: &RenderContext<'_, Action>) {
        let border_style = if cx.is_focused() {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let block = Block::default()
            .border_style(border_style)
            .borders(Borders::ALL)
            .title(format!(" streamed output — {} ", self.status));
        frame.render_widget(
            Paragraph::new(self.streamed.clone())
                .wrap(Wrap { trim: false })
                .block(block),
            area,
        );
    }
}

struct Status {
    text: String,
}

impl Component<Action> for Status {
    fn render(&self, frame: &mut Frame, area: Rect, _cx: &RenderContext<'_, Action>) {
        frame.render_widget(Paragraph::new(self.text.clone()), area);
    }
}

/// Root component: owns child entities and interprets messages.
struct Root {
    view: Option<Entity<StreamView>>,
    status: Option<Entity<Status>>,
}

impl Root {
    fn new() -> Self {
        Self {
            view: None,
            status: None,
        }
    }
}

impl Component<Action> for Root {
    fn init(&mut self, cx: &mut Context<'_, Self, Action>) {
        let view = cx.insert(StreamView::new());
        let status = cx.insert(Status {
            text: "enter: start stream | esc: cancel | q: quit".to_owned(),
        });
        // The root decides focus: routed input starts at the view.
        cx.focus_entity(view);
        self.view = Some(view);
        self.status = Some(status);
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
            // The runtime already routed Start/Cancel to the focused view.
            _ => ActionStatus::Continue,
        }
    }

    fn render(&self, frame: &mut Frame, area: Rect, cx: &RenderContext<'_, Action>) {
        let [main_area, status_area] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(area);
        if let Some(view) = self.view {
            cx.render_entity(view, frame, main_area);
        }
        if let Some(status) = self.status {
            cx.render_entity(status, frame, status_area);
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
