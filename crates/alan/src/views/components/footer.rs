use crate::core::{Activity, Controller, LoginState};
use crate::views::UiState;
use crate::views::component::Component;
use crate::views::components::PopupList;
use crate::views::theme;
use providers::AuthPrompt;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::prelude::Widget;
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;

#[derive(Debug, Default)]
pub struct Footer {
    popup: PopupList,
}

/// How an [`Activity`] presents itself in the status line.
struct Status {
    style: Style,
    label: &'static str,
    hints: &'static str,
}

impl From<Activity> for Status {
    fn from(activity: Activity) -> Self {
        match activity {
            Activity::Thinking => Status {
                label: "  ● thinking",
                hints: "  Ctrl-C stop",
                style: Style::default().italic().fg(ratatui::style::Color::Yellow),
            },
            Activity::Suggesting => Status {
                label: "  ●",
                hints: "  Enter accept · ↑↓ move · Esc dismiss",
                style: Style::default().fg(theme::PROMPT_FG),
            },
            Activity::Idle => Status {
                label: "  ● idle",
                hints: "  Enter send · Ctrl-C quit",
                style: Style::default().fg(ratatui::style::Color::Green),
            },
        }
    }
}

/// Flags that layer onto any activity.
fn badges(controller: &Controller) -> Vec<Span<'static>> {
    let mut badges = Vec::new();
    if controller.plan_mode() {
        badges.push(Span::styled(
            " · Plan mode",
            Style::default().fg(ratatui::style::Color::White),
        ));
    }
    if let Some(cost) = controller.usage().cost {
        badges.push(Span::styled(
            format!(" · ${:.4}", (cost * 10_000.0).trunc() / 10_000.0),
            Style::default().fg(theme::MUTED_FG),
        ));
    }
    badges
}

fn status_line(controller: &Controller) -> Line<'static> {
    let status = Status::from(controller.activity());
    let mut spans = vec![
        Span::styled(status.label, status.style),
        Span::styled(status.hints, Style::default().fg(theme::MUTED_FG)),
    ];
    spans.extend(badges(controller));
    Line::from(spans)
}

impl Component for Footer {
    fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        controller: &Controller,
        state: &mut UiState,
    ) {
        let background = Paragraph::new("").style(Style::default().bg(theme::EDITOR_BG));
        frame.render_widget(background, area);

        let [
            attachment_area,
            _top_padding,
            status_area,
            _status_editor_gap,
            editor_area,
            _bottom_padding,
        ] = Layout::vertical([
            Constraint::Length(state.attachment_height()),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(area);

        // Render attachment section when there are pending images.
        if !state.attachments().is_empty() {
            let mut lines: Vec<Line<'static>> = vec![
                Line::from("\n"),
                Line::from(Span::styled(
                    "  Attachments",
                    Style::default().fg(theme::ATTACHMENT_FG).bold(),
                )),
            ];
            for attachment in state.attachments() {
                lines.push(Line::from(Span::styled(
                    format!("   - {}", attachment.name),
                    Style::default().fg(theme::ATTACHMENT_FG),
                )));
            }
            let attachments =
                Paragraph::new(Text::from(lines)).style(Style::default().bg(theme::ATTACHMENT_BG));
            frame.render_widget(attachments, attachment_area);
        }

        frame.render_widget(
            Paragraph::new(status_line(controller)).style(Style::default().bg(theme::EDITOR_BG)),
            status_area,
        );

        let secret_input = matches!(
            controller.login_state(),
            LoginState::Prompting {
                prompt: AuthPrompt::Secret { .. },
                ..
            }
        );
        if secret_input {
            let value = "•".repeat(state.input().chars().count());
            let input_line = Line::from(vec![
                Span::styled("  › ", Style::default().fg(theme::PROMPT_FG)),
                Span::styled(value.clone(), Style::default().fg(theme::EDITOR_FG)),
            ]);
            frame.render_widget(
                Paragraph::new(Text::from(input_line))
                    .style(Style::default().fg(theme::EDITOR_FG).bg(theme::EDITOR_BG)),
                editor_area,
            );
            let cursor_x = editor_area
                .x
                .saturating_add(4)
                .saturating_add(value.chars().count() as u16)
                .min(editor_area.right().saturating_sub(1));
            frame.set_cursor_position((cursor_x, editor_area.y));
            return;
        }

        let [prompt_area, input_area] =
            Layout::horizontal([Constraint::Length(theme::PROMPT_GUTTER), Constraint::Min(1)])
                .areas(editor_area);
        frame.render_widget(
            Paragraph::new("  › ")
                .style(Style::default().fg(theme::PROMPT_FG).bg(theme::EDITOR_BG)),
            prompt_area,
        );
        state.editor().render(input_area, frame.buffer_mut());
        if let Some(position) = state.cursor_screen_position() {
            frame.set_cursor_position(position);
        }
        let completion = controller.completion();
        let rows = PopupList::required_rows(completion.status(), completion.item_count());
        if let Some(popup_area) = PopupList::area_above(area, frame.area(), rows) {
            self.popup.render(frame, popup_area, controller, state);
        }
    }
}
