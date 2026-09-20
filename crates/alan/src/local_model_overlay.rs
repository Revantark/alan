use crate::root::AlanAction;
use crate::views::theme;
use alan_tui::context::Context;
use alan_tui::{ActionStatus, Component, RenderContext};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use providers::{LocalApi, LocalModelEntry, LocalProvider};
use ratatui::Frame;
use ratatui::layout::Alignment;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};
use std::sync::Arc;

const FIELD_COUNT: usize = 4;
const FIELD_LABELS: [&str; FIELD_COUNT] = ["Model ID", "URL", "API", "API Key"];

#[derive(Debug, Clone)]
struct FormFields {
    model_id: String,
    url: String,
    api_key: String,
}

impl FormFields {
    fn field_mut(&mut self, index: usize) -> Option<&mut String> {
        match index {
            0 => Some(&mut self.model_id),
            1 => Some(&mut self.url),
            3 => Some(&mut self.api_key),
            _ => None,
        }
    }

    fn field(&self, index: usize) -> &str {
        match index {
            0 => &self.model_id,
            1 => &self.url,
            3 => &self.api_key,
            _ => "",
        }
    }

    fn label(index: usize) -> &'static str {
        FIELD_LABELS[index]
    }

    fn to_entry(&self) -> Option<LocalModelEntry> {
        let model_id = self.model_id.trim().to_owned();
        let url = self.url.trim().to_owned();
        if model_id.is_empty() || url.is_empty() {
            return None;
        }
        let api_key = self.api_key.trim().to_owned();
        Some(LocalModelEntry {
            model_id,
            url,
            api: LocalApi::ChatCompletions,
            api_key: if api_key.is_empty() {
                None
            } else {
                Some(api_key)
            },
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum OverlayState {
    Editing,
    Saving,
    Success,
    Error(String),
}

pub struct LocalModelOverlay {
    provider: Arc<LocalProvider>,
    fields: FormFields,
    focused: usize,
    state: OverlayState,
    edit_mode: bool,
}

impl LocalModelOverlay {
    pub fn new(provider: Arc<LocalProvider>, edit_entry: Option<LocalModelEntry>) -> Self {
        let (fields, edit_mode) = match edit_entry {
            Some(entry) => (
                FormFields {
                    model_id: entry.model_id,
                    url: entry.url,
                    api_key: entry.api_key.unwrap_or_default(),
                },
                true,
            ),
            None => (
                FormFields {
                    model_id: String::new(),
                    url: String::new(),
                    api_key: String::new(),
                },
                false,
            ),
        };
        Self {
            provider,
            fields,
            focused: 0,
            state: OverlayState::Editing,
            edit_mode,
        }
    }

    fn move_focus(&mut self, delta: isize) {
        self.focused = ((self.focused as isize + delta).rem_euclid(FIELD_COUNT as isize)) as usize;
    }

    fn submit(&mut self, cx: &mut Context<'_, Self, AlanAction>) {
        let Some(entry) = self.fields.to_entry() else {
            self.state = OverlayState::Error("Model ID and URL are required".into());
            cx.notify();
            return;
        };

        let provider = Arc::clone(&self.provider);
        let edit_mode = self.edit_mode;

        cx.spawn(
            async move {
                if edit_mode {
                    provider
                        .update_model(entry)
                        .await
                        .map_err(|e| alan_tui::TaskError(e.to_string().into()))?;
                } else {
                    provider
                        .add_model(entry)
                        .await
                        .map_err(|e| alan_tui::TaskError(e.to_string().into()))?;
                }
                Ok::<(), alan_tui::TaskError>(())
            },
            move |result, overlay, cx| {
                match result {
                    Ok(()) => {
                        overlay.state = OverlayState::Success;
                    }
                    Err(e) => {
                        overlay.state = OverlayState::Error(e.to_string());
                    }
                }
                cx.notify();
            },
        );

        self.state = OverlayState::Saving;
        cx.notify();
    }

    fn dismiss(&self, cx: &mut Context<'_, Self, AlanAction>) {
        cx.close_overlay();
    }
}

impl Component<AlanAction> for LocalModelOverlay {
    fn handle_action(
        &mut self,
        action: &AlanAction,
        cx: &mut Context<'_, Self, AlanAction>,
    ) -> ActionStatus {
        match action {
            AlanAction::Paste(text) => {
                if let OverlayState::Editing = &self.state
                    && let Some(field) = self.fields.field_mut(self.focused)
                {
                    field.push_str(text);
                    cx.notify();
                }
                ActionStatus::Handled
            }
            AlanAction::Raw(event) => {
                let Event::Key(key) = event else {
                    return ActionStatus::Handled;
                };
                if key.kind != KeyEventKind::Press {
                    return ActionStatus::Handled;
                }
                match &self.state {
                    OverlayState::Editing => match key.code {
                        KeyCode::Esc => {
                            self.dismiss(cx);
                        }
                        KeyCode::Tab => {
                            self.move_focus(1);
                            cx.notify();
                        }
                        KeyCode::BackTab => {
                            self.move_focus(-1);
                            cx.notify();
                        }
                        KeyCode::Down => {
                            self.move_focus(1);
                            cx.notify();
                        }
                        KeyCode::Up => {
                            self.move_focus(-1);
                            cx.notify();
                        }
                        KeyCode::Enter => {
                            self.submit(cx);
                        }
                        KeyCode::Backspace => {
                            if let Some(field) = self.fields.field_mut(self.focused) {
                                field.pop();
                                cx.notify();
                            }
                        }
                        KeyCode::Char(c)
                            if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
                        {
                            if let Some(field) = self.fields.field_mut(self.focused) {
                                field.push(c);
                                cx.notify();
                            }
                        }
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            self.dismiss(cx);
                        }
                        _ => {}
                    },
                    OverlayState::Saving => {}
                    OverlayState::Success | OverlayState::Error(_) => {
                        if matches!(key.code, KeyCode::Enter | KeyCode::Esc) {
                            self.dismiss(cx);
                        }
                    }
                }
                ActionStatus::Handled
            }
            _ => ActionStatus::Continue,
        }
    }

    fn render(&self, frame: &mut Frame, area: Rect, _cx: &RenderContext<'_, '_, AlanAction>) {
        render_local_model_overlay(
            frame,
            area,
            &self.fields,
            self.focused,
            &self.state,
            self.edit_mode,
        );
    }
}

fn render_local_model_overlay(
    frame: &mut Frame,
    area: Rect,
    fields: &FormFields,
    focused: usize,
    state: &OverlayState,
    edit_mode: bool,
) {
    let width = area.width.saturating_sub(4).min(88);
    let height = area.height.saturating_sub(2).min(18);
    if width < 12 || height < 6 {
        return;
    }
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    let accent = Style::default().fg(theme::PROMPT_FG);
    let muted = Style::default().fg(theme::TOOL_FG);

    let title_label = if edit_mode { "Edit" } else { "Add" };
    let (heading, footer) = match state {
        OverlayState::Editing => (
            format!("{} Local Model", title_label),
            if width >= 60 {
                " Tab move   Enter save   Esc cancel "
            } else {
                " Tab · Enter · Esc "
            },
        ),
        OverlayState::Saving => ("Saving…".to_owned(), ""),
        OverlayState::Success => ("Saved".to_owned(), " Enter / Esc close "),
        OverlayState::Error(_) => ("Error".to_owned(), " Enter / Esc close "),
    };

    let title = vec![Span::styled(
        format!(" {} ", heading),
        accent.add_modifier(Modifier::BOLD),
    )];
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(accent)
        .style(Style::default().bg(theme::EDITOR_BG).fg(theme::USER_FG))
        .title(Line::from(title).style(accent))
        .title_alignment(Alignment::Center)
        .title_bottom(Line::from(footer).style(muted));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    match state {
        OverlayState::Editing => {
            let max_label = (0..FIELD_COUNT)
                .map(|i| FormFields::label(i).len())
                .max()
                .unwrap_or(0);
            for i in 0..FIELD_COUNT {
                let y = inner.y + i as u16 * 2;
                if y + 2 > inner.bottom() {
                    break;
                }
                let is_focused = i == focused;
                let label_style = if is_focused {
                    accent.add_modifier(Modifier::BOLD)
                } else {
                    muted
                };
                let line_style = if is_focused {
                    Style::default().bg(theme::SELECTION_BG)
                } else {
                    Style::default()
                };

                // Value (field 2 is a fixed read-only label so the user
                // knows local models use the chat-completions API).
                let value = if i == 2 {
                    "Chat Completions"
                } else {
                    fields.field(i)
                };
                let value_style = if is_focused {
                    Style::default()
                        .fg(theme::SELECTION_FG)
                        .bg(theme::SELECTION_BG)
                } else {
                    Style::default().fg(theme::EDITOR_FG)
                };
                let display = if value.is_empty() && i != 2 {
                    Span::styled("required", muted)
                } else {
                    Span::styled(value, value_style)
                };

                let line = Line::from(vec![
                    Span::styled(if is_focused { " › " } else { "   " }, accent),
                    Span::styled(
                        format!("{:>width$}  ", FormFields::label(i), width = max_label),
                        label_style,
                    ),
                    Span::styled(": ", muted),
                    display,
                ]);
                frame.render_widget(
                    Paragraph::new(line).style(line_style),
                    Rect::new(inner.x, y, inner.width, 1),
                );
                frame.render_widget(
                    Paragraph::new(Span::raw("")).style(Style::default()),
                    Rect::new(inner.x, y + 1, inner.width, 1),
                );
            }
        }
        OverlayState::Saving => {
            frame.render_widget(Paragraph::new("  Saving…").style(accent), inner);
        }
        OverlayState::Success => {
            let msg = if edit_mode {
                "Model updated."
            } else {
                "Model added."
            };
            frame.render_widget(Paragraph::new(format!("  {msg}")).style(accent), inner);
        }
        OverlayState::Error(err) => {
            frame.render_widget(
                Paragraph::new(format!("  {err}")).style(Style::default().fg(Color::Red)),
                inner,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn render_grid(width: u16, height: u16) -> Vec<String> {
        let fields = FormFields {
            model_id: "gpt-4o-mini".to_owned(),
            url: "http://localhost:11434".to_owned(),
            api_key: "sk-abc".to_owned(),
        };
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                render_local_model_overlay(
                    frame,
                    frame.area(),
                    &fields,
                    0,
                    &OverlayState::Editing,
                    false,
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
            .chars()
            .collect::<Vec<char>>()
            .chunks(width as usize)
            .map(|row| row.iter().collect::<String>())
            .collect()
    }

    #[test]
    fn form_field_labels_align_to_a_fixed_column() {
        let rows = render_grid(80, 18);
        let mut colon_cols = Vec::new();
        for row in &rows {
            if let Some(col) = row.chars().position(|character| character == ':') {
                colon_cols.push(col);
            }
        }
        assert_eq!(
            colon_cols.len(),
            FIELD_COUNT,
            "expected one colon per field, got {colon_cols:?}"
        );
        let first = colon_cols[0];
        assert!(
            colon_cols.iter().all(|&c| c == first),
            "label colon columns are not aligned: {colon_cols:?}"
        );
    }
}
