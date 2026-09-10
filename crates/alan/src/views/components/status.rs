//! Bottom status line of the chat area: activity state, key hints, and the
//! mode/cost/model badges. Rendered by the chat container, which owns the
//! vertical stack (transcript, attachments, status, editor); it depends only
//! on the controller snapshot types, the theme, and ratatui.

use crate::core::Activity;
use crate::views::theme;
use agent::Mode;
use llm::Usage;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Block;
use ratatui::widgets::Padding;
use ratatui::widgets::Paragraph;

/// Rows the status band occupies: one blank pad row above the status text.
pub(crate) const STATUS_HEIGHT: u16 = 2;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StatusSnapshot {
    pub activity: Activity,
    pub mode: Mode,
    pub usage: Usage,
    pub model_name: String,
}

impl StatusSnapshot {
    /// Project the transcript snapshot onto the fields the status line reads.
    pub(crate) fn from_snapshot(snap: &super::chat_history::ChatSnapshot) -> Self {
        Self {
            activity: snap.activity,
            mode: snap.mode,
            usage: snap.usage.clone(),
            model_name: snap.model_name.clone(),
        }
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
                style: Style::default().italic().fg(Color::Yellow),
            },
            Activity::Idle => StatusStyle {
                label: "  ● idle",
                hints: "  Enter send · Ctrl-C quit",
                style: Style::default().fg(Color::Green),
            },
        }
    }
}

/// Flags that layer onto any activity.
fn badges(snap: &StatusSnapshot) -> Vec<Span<'static>> {
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
    badges
}

fn status_line(snap: &StatusSnapshot) -> Line<'static> {
    let status = StatusStyle::from(snap.activity);
    let mut spans = vec![
        Span::styled(status.label, status.style),
        Span::styled(status.hints, Style::default().fg(theme::MUTED_FG)),
    ];
    spans.extend(badges(snap));
    spans.push(Span::styled(
        format!(" · {}", snap.model_name),
        Style::default().fg(theme::MUTED_FG),
    ));

    Line::from(spans)
}

/// Render the pinned status line into `area`.
pub(crate) fn render_status(frame: &mut Frame, area: Rect, snap: &StatusSnapshot) {
    frame.render_widget(
        Paragraph::new(status_line(snap))
            .style(Style::default().bg(theme::EDITOR_BG))
            .block(Block::new().padding(Padding::new(0, 0, 1, 0))),
        area,
    );
}
