use crate::root::AlanAction;
use crate::views::theme;
use crate::views::{SearchListEvent, SearchListOverlay};
use alan_tui::context::Context;
use alan_tui::{ActionStatus, Component, RenderContext, TaskError};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use providers::{LocalApi, LocalModelEntry, LocalProvider, list_local_models};
use ratatui::Frame;
use ratatui::layout::Alignment;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};
use std::sync::Arc;

const FIELD_COUNT: usize = 4;
// model last so url + key are known when we auto-detect it.
const FIELD_LABELS: [&str; FIELD_COUNT] = ["URL", "API", "API Key", "Model ID"];
const URL: usize = 0;
const API: usize = 1;
const API_KEY: usize = 2;
const MODEL: usize = 3;

#[derive(Debug, Clone)]
struct FormFields {
    model_id: String,
    url: String,
    api_key: String,
}

impl FormFields {
    fn field_mut(&mut self, index: usize) -> Option<&mut String> {
        match index {
            URL => Some(&mut self.url),
            API_KEY => Some(&mut self.api_key),
            MODEL => Some(&mut self.model_id),
            _ => None,
        }
    }

    fn field(&self, index: usize) -> &str {
        match index {
            URL => &self.url,
            API_KEY => &self.api_key,
            MODEL => &self.model_id,
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

/// `GET /models` result for `detect_source`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Detect {
    Idle,
    Loading,
    Found(Vec<String>),
    Failed(String),
}

pub struct LocalModelOverlay {
    provider: Arc<LocalProvider>,
    fields: FormFields,
    focused: usize,
    state: OverlayState,
    edit_mode: bool,
    detect: Detect,
    // (url, key) the current `detect` belongs to.
    detect_source: (String, String),
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
            detect: Detect::Idle,
            detect_source: (String::new(), String::new()),
        }
    }

    fn source(&self) -> (String, String) {
        (
            self.fields.url.trim().to_owned(),
            self.fields.api_key.trim().to_owned(),
        )
    }

    /// Fetch the model list when focus lands on Model ID. Reuses a result for
    /// the same url/key; a failed one retries.
    fn maybe_detect(&mut self, cx: &mut Context<'_, Self, AlanAction>) {
        if self.edit_mode || self.focused != MODEL {
            return;
        }
        let source = self.source();
        if source.0.is_empty()
            || (source == self.detect_source
                && matches!(self.detect, Detect::Loading | Detect::Found(_)))
        {
            return;
        }
        self.detect_source = source.clone();
        self.detect = Detect::Loading;
        let requested = source.clone();
        cx.spawn(
            async move {
                let (url, key) = source;
                list_local_models(&url, Some(&key))
                    .await
                    .map_err(|e| match e {
                        // drop the "failed to fetch models:" prefix, hint says it.
                        providers::ProviderError::Fetch(reason) => TaskError(reason.into()),
                        e => TaskError(e.to_string().into()),
                    })
            },
            move |result, overlay, cx| {
                // url/key changed mid-flight.
                if overlay.detect_source != requested {
                    return;
                }
                match result {
                    Ok(ids) => {
                        overlay.detect = Detect::Found(ids);
                        // don't yank focus if they already typed or moved on.
                        if overlay.focused == MODEL
                            && overlay.state == OverlayState::Editing
                            && overlay.fields.model_id.trim().is_empty()
                        {
                            overlay.open_picker(cx);
                        }
                    }
                    Err(e) => overlay.detect = Detect::Failed(e.to_string()),
                }
                cx.notify();
            },
        );
    }

    fn open_picker(&mut self, cx: &mut Context<'_, Self, AlanAction>) {
        let Detect::Found(ids) = &self.detect else {
            return;
        };
        let ids = ids.clone();
        let picker = cx.open_overlay(SearchListOverlay::new("Pick Model", ids.clone()));
        cx.subscribe_once::<SearchListEvent, SearchListOverlay, _>(
            picker,
            move |event, overlay, _picker, cx| {
                if let SearchListEvent::Chosen(index) = event
                    && let Some(id) = ids.get(*index)
                {
                    overlay.fields.model_id = id.clone();
                    cx.notify();
                }
            },
        );
    }

    fn on_enter(&mut self, cx: &mut Context<'_, Self, AlanAction>) {
        if !self.fields.model_id.trim().is_empty() || self.fields.url.trim().is_empty() {
            self.submit(cx);
            return;
        }
        // url set, model missing: detect/pick instead of erroring out the form.
        if self.focused != MODEL {
            self.focused = MODEL;
            self.maybe_detect(cx);
            cx.notify();
        } else if matches!(self.detect, Detect::Found(_)) {
            self.open_picker(cx);
        }
    }

    fn move_focus(&mut self, delta: isize, cx: &mut Context<'_, Self, AlanAction>) {
        self.focused = ((self.focused as isize + delta).rem_euclid(FIELD_COUNT as isize)) as usize;
        self.maybe_detect(cx);
        cx.notify();
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
                        KeyCode::Tab => self.move_focus(1, cx),
                        KeyCode::BackTab => self.move_focus(-1, cx),
                        KeyCode::Down => self.move_focus(1, cx),
                        KeyCode::Up => self.move_focus(-1, cx),
                        KeyCode::Enter => self.on_enter(cx),
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
            &self.detect,
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
    detect: &Detect,
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

                // Value (API is a fixed read-only label so the user
                // knows local models use the chat-completions API).
                let value = if i == API {
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
                let display = if value.is_empty() && i == API_KEY {
                    Span::styled("optional", muted)
                } else if value.is_empty() && i != API {
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
                let hint = if i == MODEL {
                    detect_hint(detect, fields.model_id.trim().is_empty())
                } else {
                    String::new()
                };
                frame.render_widget(
                    Paragraph::new(Span::styled(hint, muted)),
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

fn detect_hint(detect: &Detect, model_empty: bool) -> String {
    let indent = " ".repeat(5 + FIELD_LABELS.iter().map(|l| l.len()).max().unwrap_or(0) + 2);
    match detect {
        Detect::Idle => String::new(),
        Detect::Loading => format!("{indent}detecting models…"),
        Detect::Found(ids) if model_empty => {
            format!("{indent}{} found, Enter to pick", ids.len())
        }
        Detect::Found(_) => String::new(),
        Detect::Failed(reason) => {
            format!("{indent}auto-detect failed ({reason}), type the model ID")
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
                    &Detect::Idle,
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

    #[test]
    fn model_field_comes_last() {
        let rows = render_grid(80, 18);
        let labels: Vec<&str> = rows
            .iter()
            .filter_map(|row| row.split(':').next())
            .map(str::trim)
            .filter(|label| FIELD_LABELS.iter().any(|l| label.ends_with(l)))
            .collect();
        assert_eq!(labels.len(), FIELD_COUNT);
        assert!(labels[MODEL].ends_with("Model ID"), "{labels:?}");
    }

    #[test]
    fn detect_hint_per_state() {
        assert_eq!(detect_hint(&Detect::Idle, true), "");
        assert!(detect_hint(&Detect::Loading, true).ends_with("detecting models…"));
        let found = Detect::Found(vec!["a".into(), "b".into()]);
        assert!(detect_hint(&found, true).ends_with("2 found, Enter to pick"));
        // already typed: nothing to nag about.
        assert_eq!(detect_hint(&found, false), "");
        let failed = Detect::Failed("HTTP 401 Unauthorized".into());
        assert!(
            detect_hint(&failed, false)
                .ends_with("auto-detect failed (HTTP 401 Unauthorized), type the model ID")
        );
    }

    #[test]
    fn to_entry_rejects_empty_model_id() {
        let fields = FormFields {
            model_id: String::new(),
            url: "http://localhost:11434".to_owned(),
            api_key: String::new(),
        };
        assert!(fields.to_entry().is_none());
    }

    #[test]
    fn to_entry_rejects_empty_url() {
        let fields = FormFields {
            model_id: "llama3".to_owned(),
            url: String::new(),
            api_key: String::new(),
        };
        assert!(fields.to_entry().is_none());
    }

    #[test]
    fn to_entry_accepts_valid_fields() {
        let fields = FormFields {
            model_id: "llama3".to_owned(),
            url: "http://localhost:11434".to_owned(),
            api_key: "sk-abc".to_owned(),
        };
        let entry = fields.to_entry().unwrap();
        assert_eq!(entry.model_id, "llama3");
        assert_eq!(entry.url, "http://localhost:11434");
        assert_eq!(entry.api, LocalApi::ChatCompletions);
        assert_eq!(entry.api_key, Some("sk-abc".to_owned()));
    }

    #[test]
    fn to_entry_strips_whitespace() {
        let fields = FormFields {
            model_id: "  llama3  ".to_owned(),
            url: "  http://localhost:11434  ".to_owned(),
            api_key: String::new(),
        };
        let entry = fields.to_entry().unwrap();
        assert_eq!(entry.model_id, "llama3");
        assert_eq!(entry.url, "http://localhost:11434");
        assert_eq!(entry.api_key, None);
    }
}
