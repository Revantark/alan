use crate::core::Activity;
use crate::views::theme;
use agent::Mode;
use llm::Usage;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use tui::{Component, RenderContext};

/// Plain-data view of the state the status line presents. The parent builds
/// this from `Controller` each tick and pushes it down with `cx.update`, so
/// `Status` never names a core controller type.
#[derive(Debug, Clone, PartialEq)]
pub struct StatusSnapshot {
    pub activity: Activity,
    pub mode: Mode,
    pub usage: Usage,
}

/// Status line between the chat and the editor: activity, key hints, and the
/// mode/cost badges.
#[derive(Debug, Default)]
pub struct Status {
    snap: Option<StatusSnapshot>,
}

impl Status {
    pub fn set(&mut self, snap: StatusSnapshot) {
        self.snap = Some(snap);
    }

    /// Whether the entity already holds this snapshot, so the 16ms poll tick
    /// can skip a redundant update.
    pub fn matches(&self, snap: &StatusSnapshot) -> bool {
        self.snap.as_ref() == Some(snap)
    }
}

/// How an [`Activity`] presents itself in the status line.
struct StatusStyle {
    style: Style,
    label: &'static str,
    hints: &'static str,
}

impl From<Activity> for StatusStyle {
    fn from(activity: Activity) -> Self {
        match activity {
            Activity::Thinking => StatusStyle {
                label: "  ● thinking",
                hints: "  Ctrl-C stop",
                style: Style::default().italic().fg(ratatui::style::Color::Yellow),
            },
            Activity::Suggesting => StatusStyle {
                label: "  ●",
                hints: " Enter accept · ↑↓ move · Esc dismiss",
                style: Style::default().fg(theme::PROMPT_FG),
            },
            Activity::Idle => StatusStyle {
                label: "  ● idle",
                hints: "  Enter send · Ctrl-C quit",
                style: Style::default().fg(ratatui::style::Color::Green),
            },
        }
    }
}

/// Flags that layer onto any activity.
fn badges(snap: &StatusSnapshot) -> Vec<Span<'static>> {
    let mut badges = Vec::new();
    let badge = match snap.mode {
        Mode::Plan => Some((" · Plan mode", ratatui::style::Color::White)),
        Mode::Review => Some((" · Review mode", ratatui::style::Color::White)),
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
    badges
}

fn status_line(snap: &StatusSnapshot) -> Line<'static> {
    let status = StatusStyle::from(snap.activity);
    let mut spans = vec![
        Span::styled(status.label, status.style),
        Span::styled(status.hints, Style::default().fg(theme::MUTED_FG)),
    ];
    spans.extend(badges(snap));
    Line::from(spans)
}

impl<A: 'static> Component<A> for Status {
    fn render(&self, frame: &mut Frame, area: Rect, _cx: &RenderContext<'_, A>) {
        let Some(snap) = &self.snap else {
            return;
        };
        frame.render_widget(
            Paragraph::new(status_line(snap)).style(Style::default().bg(theme::EDITOR_BG)),
            area,
        );
    }
}
