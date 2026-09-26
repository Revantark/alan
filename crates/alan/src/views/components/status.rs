use crate::core::Activity;
use crate::core::permissions::Policy;
use crate::root::AlanAction;
use crate::views::theme;
use agent::Mode;
use alan_tui::component::{ActionStatus, Component, RenderContext};
use alan_tui::context::Context;
use alan_tui::{Subscription, SubscriptionEvent};
use llm::{ReasoningEffort, Usage};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Paragraph};

pub(crate) const STATUS_HEIGHT: u16 = 2;

const LOADING_DOT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(350);

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StatusInputs {
    pub activity: Activity,
    pub mode: Mode,
    pub policy: Policy,
    pub usage: Usage,
    pub model_name: String,
    pub max_context: Option<u64>,
    pub reasoning_effort: ReasoningEffort,
}

/// The pinned status band: activity, badges, and the loading-dot animation.
pub(crate) struct Status {
    /// Current dot count (0..3) driving the loading animation. Advanced by the
    /// repaint ticker while a blocking operation is in flight.
    loading_dots: usize,
    /// Fixed-rate ticker driving the loading-dot animation, alive only while a
    /// blocking operation is in flight. Dropping it cancels the animation.
    loading_repaint: Option<Subscription>,
}

impl Status {
    pub fn new() -> Self {
        Self {
            loading_dots: 0,
            loading_repaint: None,
        }
    }

    pub fn set_loading_with(&mut self, loading: bool, cx: &mut Context<'_, Self, AlanAction>) {
        if loading == self.loading_repaint.is_some() {
            return;
        }
        self.loading_dots = 0;
        if loading {
            self.loading_repaint =
                Some(cx.subscribe_stream(loading_ticks(), |event, status, cx| {
                    if matches!(event, SubscriptionEvent::Closed) {
                        status.loading_repaint = None;
                        return;
                    }
                    status.loading_dots = (status.loading_dots + 1) % 4;
                    cx.notify();
                }));
        } else {
            self.loading_repaint = None;
        }
        cx.notify();
    }
}

impl Component<AlanAction> for Status {
    fn handle_action(
        &mut self,
        action: &AlanAction,
        cx: &mut Context<'_, Self, AlanAction>,
    ) -> ActionStatus
    where
        Self: Sized,
    {
        match action {
            AlanAction::SetLoadingDots(loading) => {
                self.set_loading_with(*loading, cx);
                ActionStatus::Handled
            }
            _ => ActionStatus::Continue,
        }
    }

    fn render(&self, frame: &mut Frame, area: Rect, cx: &RenderContext<'_, '_, AlanAction>) {
        let inputs = cx.expect_state::<StatusInputs>();
        render_status(frame, area, inputs, self.loading_dots);
    }
}

struct StatusStyle {
    style: Style,
    label: String,
}

/// Indicator glyph for the current tool-permission policy mode.
fn dot(policy: Policy) -> &'static str {
    match policy {
        Policy::Free => "◌",
        Policy::Slip => "⟐",
        Policy::Strict => "#",
    }
}

impl StatusStyle {
    fn new(activity: &Activity, policy: Policy) -> Self {
        match activity {
            Activity::Thinking => StatusStyle {
                label: format!("  {} thinking", dot(policy)),
                style: Style::default().italic().fg(Color::Yellow),
            },
            Activity::Idle => StatusStyle {
                label: format!("  {} idle", dot(policy)),
                style: Style::default().fg(Color::Green),
            },
            // Loading is rendered separately (it carries a label and a live dot
            // count); this arm is never used via the `From` path.
            Activity::Loading(_) => StatusStyle {
                label: format!("  {}", dot(policy)),
                style: Style::default().fg(Color::Cyan),
            },
        }
    }
}

/// Flags that layer onto any activity.
fn badges(snap: &StatusInputs) -> Vec<Span<'static>> {
    let mut badges = Vec::new();
    let badge = match snap.mode {
        Mode::Plan => Some((" · Plan mode", Color::White)),
        Mode::Review => Some((" · Review mode", Color::White)),
        Mode::Normal => None,
    };
    if let Some((label, color)) = badge {
        badges.push(Span::styled(label, Style::default().fg(color)));
    }
    if let Some(cost) = snap.usage.cost {
        badges.push(Span::styled(
            format!(" · ${:.4}", (cost * 10_000.0).trunc() / 10_000.0),
            Style::default().fg(theme::MUTED_FG),
        ));
    }
    if let Some(context) = context_badge(snap) {
        badges.push(context);
    }
    badges
}

/// Token count formatted compactly: `12k`, `1.05M`, `850`.
fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        let m = n as f64 / 1_000_000.0;
        // Trim trailing zeros: `.to_string()` -> "1.05" not "1.050000"
        return format!("{}M", (m * 100.0).round() / 100.0);
    }
    if n >= 1000 {
        return format!("{}k", n / 1000);
    }
    n.to_string()
}

/// Context badge shows `context: 12k/1.05M`. Color is muted normally, yellow
/// at >= 70% of the window, red at >= 85%. Omitted when the window is unknown.
fn context_badge(snap: &StatusInputs) -> Option<Span<'static>> {
    let max = snap.max_context?;
    let context_tokens = snap.usage.input_tokens + snap.usage.output_tokens;
    let mut ratio = context_tokens as f64 / max as f64;
    if ratio > 1.0 {
        ratio = 1.0;
    }
    let color = if ratio >= 0.85 {
        Color::Red
    } else if ratio >= 0.70 {
        Color::Yellow
    } else {
        theme::MUTED_FG
    };
    let label = format!(
        " · context: {}/{}",
        format_tokens(context_tokens),
        format_tokens(max)
    );
    Some(Span::styled(label, Style::default().fg(color)))
}

fn status_line(snap: &StatusInputs, loading_dots: usize) -> Line<'static> {
    if let Activity::Loading(text) = &snap.activity {
        return Line::from(Span::styled(
            format!("  {} {text} {}", dot(snap.policy), ".".repeat(loading_dots)),
            Style::default().fg(Color::Cyan),
        ));
    }
    let status = StatusStyle::new(&snap.activity, snap.policy);
    let mut spans = vec![Span::styled(status.label, status.style)];
    spans.extend(badges(snap));
    spans.push(Span::styled(
        format!(" · {}", snap.model_name),
        Style::default().fg(theme::MUTED_FG),
    ));
    spans.push(Span::styled(
        format!(" · {}", snap.reasoning_effort),
        Style::default().fg(theme::MUTED_FG),
    ));

    Line::from(spans)
}

/// Render the pinned status line into `area`.
fn render_status(frame: &mut Frame, area: Rect, snap: &StatusInputs, loading_dots: usize) {
    frame.render_widget(
        Paragraph::new(status_line(snap, loading_dots))
            .style(Style::default().bg(theme::EDITOR_BG))
            .block(Block::new().padding(Padding::new(0, 0, 1, 0))),
        area,
    );
}

fn loading_ticks() -> impl futures_util::Stream<Item = ()> + Send + 'static {
    futures_util::stream::unfold((), |_| async {
        tokio::time::sleep(LOADING_DOT_INTERVAL).await;
        Some(((), ()))
    })
}
