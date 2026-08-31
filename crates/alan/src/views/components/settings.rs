use super::centered_rect;
use crate::core::Controller;
use crate::core::settings::{Layer, Marker};
use crate::views::UiState;
use crate::views::component::Component;
use crate::views::theme;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;

const LABEL_WIDTH: usize = 20;
const VALUE_WIDTH: usize = 28;

#[derive(Debug, Default)]
pub struct SettingsOverlayView;

impl Component for SettingsOverlayView {
    fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        controller: &Controller,
        state: &mut UiState,
    ) {
        let settings = controller.settings();
        let (Some(overlay), Some(path)) = (settings.overlay(), settings.target()) else {
            return;
        };

        let area = centered_rect(78, 60, area);
        frame.render_widget(ratatui::widgets::Clear, area);
        frame.render_widget(
            Paragraph::new("").style(Style::default().bg(theme::EDITOR_BG)),
            area,
        );

        let [header_area, rows_area, footer_area] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .spacing(1)
        .areas(area.inner(Margin {
            horizontal: 2,
            vertical: 1,
        }));

        let scope = if overlay.scope == Layer::Project {
            "project"
        } else {
            "global"
        };

        let exists = path.is_file();
        frame.render_widget(
            Paragraph::new(Text::from(vec![
                Line::from(vec![
                    Span::styled("/settings", Style::default().fg(theme::COMMAND_FG)),
                    Span::styled("   scope: ", Style::default().fg(theme::MUTED_FG)),
                    Span::styled(scope, Style::default().fg(theme::EDITOR_FG)),
                    Span::styled("  ·  Tab to switch", Style::default().fg(theme::MUTED_FG)),
                ]),
                Line::from(Span::styled(
                    if exists {
                        path.display().to_string()
                    } else {
                        format!("{} (not created yet)", path.display())
                    },
                    Style::default().fg(theme::MUTED_FG),
                )),
            ])),
            header_area,
        );

        let rows = settings.rows();
        let lines: Vec<Line> = rows
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let selected = index == overlay.selected;
                if selected && overlay.editing {
                    let typed = state.input().to_owned();
                    return Line::from(vec![
                        Span::styled(" › ", Style::default().fg(theme::PROMPT_FG)),
                        Span::styled(
                            pad(row.label, LABEL_WIDTH),
                            Style::default().fg(theme::SELECTION_FG),
                        ),
                        Span::styled(typed, Style::default().fg(theme::SELECTION_FG)),
                        Span::styled("▌", Style::default().fg(theme::PROMPT_FG)),
                        Span::styled(
                            "   Enter save · Esc cancel",
                            Style::default().fg(theme::MUTED_FG),
                        ),
                    ])
                    .style(Style::default().bg(theme::SELECTION_BG));
                }

                let value = if row.cycles {
                    format!("‹ {} ›", row.value)
                } else {
                    format!("  {}", row.value)
                };
                let (marker, marker_style) = marker_span(&row.marker);
                let line = Line::from(vec![
                    Span::styled(
                        if selected { " › " } else { "   " },
                        Style::default().fg(theme::PROMPT_FG),
                    ),
                    Span::styled(
                        pad(row.label, LABEL_WIDTH),
                        Style::default().fg(theme::EDITOR_FG),
                    ),
                    Span::styled(
                        pad(&value, VALUE_WIDTH),
                        Style::default().fg(theme::EDITOR_FG),
                    ),
                    Span::styled(marker, marker_style),
                ]);
                if selected {
                    line.style(Style::default().bg(theme::SELECTION_BG))
                } else {
                    line
                }
            })
            .collect();
        frame.render_widget(Paragraph::new(Text::from(lines)), rows_area);

        let hint = rows
            .get(overlay.selected)
            .map(|row| row.help)
            .unwrap_or_default();
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                hint,
                Style::default().fg(theme::MUTED_FG),
            ))),
            footer_area,
        );
    }
}

fn marker_span(marker: &Marker) -> (String, Style) {
    match marker {
        Marker::SetHere => (
            "● set here".into(),
            Style::default().fg(theme::TOOL_DONE_FG),
        ),
        Marker::Inherited(layer) => (
            format!("← {}", layer.label()),
            Style::default().fg(theme::MUTED_FG),
        ),
        Marker::Overridden(layer) => (
            format!("⚠ {} wins", layer.label()),
            Style::default().fg(theme::TOOL_ERROR_FG),
        ),
    }
}

fn pad(text: &str, width: usize) -> String {
    let shown: String = text.chars().take(width).collect();
    let used = shown.chars().count();
    format!("{shown}{}", " ".repeat(width.saturating_sub(used)))
}
