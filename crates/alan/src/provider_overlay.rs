//! `/provider add` and `/provider edit`: the form for a custom provider.

use crate::core::custom_providers::{CustomProviderEntry, CustomProviders, ProviderName};
use crate::root::AlanAction;
use crate::views::theme;
use alan_tui::context::Context;
use alan_tui::{ActionStatus, Component, RenderContext, TaskError};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use providers::ApiId;
use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Wrap};
use std::sync::Arc;
use unicode_width::UnicodeWidthStr;

const FIELD_COUNT: usize = 4;
const FIELD_LABELS: [&str; FIELD_COUNT] = ["Name", "URL", "API", "API Key"];
const NAME: usize = 0;
const URL: usize = 1;
const API: usize = 2;
const API_KEY: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Add,
    Edit,
}

#[derive(Default)]
struct FormFields {
    name: String,
    url: String,
    api_key: String,
}

impl FormFields {
    fn submission(&self) -> anyhow::Result<Submission> {
        let name = ProviderName::parse(&self.name)?;
        let url = self.url.trim();
        anyhow::ensure!(!url.is_empty(), "URL is required");
        let key = self.api_key.trim();
        Ok(Submission {
            name,
            url: url.to_owned(),
            key: (!key.is_empty()).then(|| key.to_owned()),
        })
    }
}

/// What the form sends once its fields parse.
struct Submission {
    name: ProviderName,
    url: String,
    key: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum OverlayState {
    Editing,
    Checking,
    /// Saved; the server lists this many models.
    Saved(usize),
    Error(String),
}

/// What a key press needs from the host beyond the overlay's own state.
#[derive(Debug, PartialEq, Eq)]
enum KeyOutcome {
    Redraw,
    Submit,
    Close,
    Ignore,
}

pub struct ProviderOverlay {
    providers: Arc<CustomProviders>,
    mode: Mode,
    api: ApiId,
    fields: FormFields,
    focused: usize,
    state: OverlayState,
}

impl ProviderOverlay {
    pub fn add(providers: Arc<CustomProviders>) -> Self {
        Self {
            providers,
            mode: Mode::Add,
            api: ApiId::ChatCompletions,
            fields: FormFields::default(),
            focused: NAME,
            state: OverlayState::Editing,
        }
    }

    /// The name is fixed: `auth.json`, `settings.json` and profiles refer to
    /// the provider by it.
    pub fn edit(
        providers: Arc<CustomProviders>,
        entry: CustomProviderEntry,
        key: Option<String>,
    ) -> Self {
        Self {
            mode: Mode::Edit,
            api: entry.api,
            fields: FormFields {
                name: entry.name.to_string(),
                url: entry.url,
                api_key: key.unwrap_or_default(),
            },
            focused: URL,
            ..Self::add(providers)
        }
    }

    fn on_key(&mut self, key: KeyEvent) -> KeyOutcome {
        match &self.state {
            OverlayState::Editing => self.on_editing_key(key),
            OverlayState::Checking => KeyOutcome::Ignore,
            OverlayState::Saved(_) => match key.code {
                KeyCode::Enter | KeyCode::Esc => KeyOutcome::Close,
                _ => KeyOutcome::Ignore,
            },
            // Back to the form with everything typed, to fix the URL or key.
            OverlayState::Error(_) => match key.code {
                KeyCode::Enter | KeyCode::Esc => {
                    self.state = OverlayState::Editing;
                    KeyOutcome::Redraw
                }
                _ => KeyOutcome::Ignore,
            },
        }
    }

    fn on_editing_key(&mut self, key: KeyEvent) -> KeyOutcome {
        match key.code {
            KeyCode::Esc => KeyOutcome::Close,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                KeyOutcome::Close
            }
            KeyCode::Enter => KeyOutcome::Submit,
            KeyCode::Tab | KeyCode::Down => self.move_focus(1),
            KeyCode::BackTab | KeyCode::Up => self.move_focus(-1),
            KeyCode::Backspace => self.edit_focused(|field| {
                field.pop();
            }),
            KeyCode::Char(c)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                self.edit_focused(|field| field.push(c))
            }
            _ => KeyOutcome::Ignore,
        }
    }

    fn move_focus(&mut self, delta: isize) -> KeyOutcome {
        self.focused = (self.focused as isize + delta).rem_euclid(FIELD_COUNT as isize) as usize;
        KeyOutcome::Redraw
    }

    fn edit_focused(&mut self, edit: impl FnOnce(&mut String)) -> KeyOutcome {
        let field = match self.focused {
            NAME if self.mode == Mode::Add => &mut self.fields.name,
            URL => &mut self.fields.url,
            API_KEY => &mut self.fields.api_key,
            _ => return KeyOutcome::Ignore,
        };
        edit(field);
        KeyOutcome::Redraw
    }

    fn submit(&mut self, cx: &mut Context<'_, Self, AlanAction>) {
        let submission = match self.fields.submission() {
            Ok(submission) => submission,
            Err(error) => {
                self.state = OverlayState::Error(error.to_string());
                cx.notify();
                return;
            }
        };
        let providers = Arc::clone(&self.providers);
        let mode = self.mode;
        let api = self.api.clone();
        cx.spawn(
            async move {
                let Submission { name, url, key } = submission;
                match mode {
                    Mode::Add => providers.add(name, api, &url, key).await,
                    Mode::Edit => providers.update(name, api, &url, key).await,
                }
                .map_err(|error| TaskError(format!("{error:#}").into()))
            },
            |result, overlay, cx| {
                overlay.state = match result {
                    Ok(count) => OverlayState::Saved(count),
                    Err(error) => OverlayState::Error(error.to_string()),
                };
                cx.notify();
            },
        );
        self.state = OverlayState::Checking;
        cx.notify();
    }
}

impl Component<AlanAction> for ProviderOverlay {
    fn handle_action(
        &mut self,
        action: &AlanAction,
        cx: &mut Context<'_, Self, AlanAction>,
    ) -> ActionStatus {
        match action {
            AlanAction::Paste(text) => {
                if self.state == OverlayState::Editing
                    && self.edit_focused(|field| field.push_str(text)) == KeyOutcome::Redraw
                {
                    cx.notify();
                }
                ActionStatus::Handled
            }
            AlanAction::Raw(Event::Key(key)) => {
                if key.kind == KeyEventKind::Press {
                    match self.on_key(*key) {
                        KeyOutcome::Redraw => cx.notify(),
                        KeyOutcome::Submit => self.submit(cx),
                        KeyOutcome::Close => cx.close_overlay(),
                        KeyOutcome::Ignore => {}
                    }
                }
                ActionStatus::Handled
            }
            AlanAction::Raw(_) => ActionStatus::Handled,
            _ => ActionStatus::Continue,
        }
    }

    fn render(&self, frame: &mut Frame, area: Rect, _cx: &RenderContext<'_, '_, AlanAction>) {
        render_provider_overlay(
            frame,
            area,
            &self.fields,
            self.focused,
            &self.state,
            self.mode,
        );
    }
}

fn render_provider_overlay(
    frame: &mut Frame,
    area: Rect,
    fields: &FormFields,
    focused: usize,
    state: &OverlayState,
    mode: Mode,
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
    frame.render_widget(Clear, popup);
    let accent = Style::default().fg(theme::PROMPT_FG);
    let muted = Style::default().fg(theme::TOOL_FG);

    let (heading, footer) = match state {
        OverlayState::Editing => (
            match mode {
                Mode::Add => "Add Provider",
                Mode::Edit => "Edit Provider",
            },
            if width >= 60 {
                " Tab move   Enter check and save   Esc cancel "
            } else {
                " Tab · Enter · Esc "
            },
        ),
        OverlayState::Checking => ("Checking…", ""),
        OverlayState::Saved(_) => ("Saved", " Enter / Esc close "),
        OverlayState::Error(_) => ("Not saved", " Enter / Esc back to the form "),
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(accent)
        .style(Style::default().bg(theme::EDITOR_BG).fg(theme::USER_FG))
        .title(
            Line::from(Span::styled(
                format!(" {heading} "),
                accent.add_modifier(Modifier::BOLD),
            ))
            .style(accent),
        )
        .title_alignment(Alignment::Center)
        .title_bottom(Line::from(footer).style(muted));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let url = fields.url.trim().trim_end_matches('/');
    match state {
        OverlayState::Editing => render_fields(frame, inner, fields, focused, accent, muted),
        OverlayState::Checking => frame.render_widget(
            Paragraph::new(format!("  Checking {url}/models…")).style(accent),
            inner,
        ),
        OverlayState::Saved(count) => frame.render_widget(
            Paragraph::new(format!(
                "  Saved {}: {count} model{}. Pick one with /models.",
                fields.name.trim(),
                if *count == 1 { "" } else { "s" }
            ))
            .style(accent),
            inner,
        ),
        OverlayState::Error(error) => frame.render_widget(
            Paragraph::new(format!("  {error}"))
                .style(Style::default().fg(theme::TOOL_ERROR_FG))
                .wrap(Wrap { trim: false }),
            inner,
        ),
    }
}

fn render_fields(
    frame: &mut Frame,
    inner: Rect,
    fields: &FormFields,
    focused: usize,
    accent: Style,
    muted: Style,
) {
    let max_label = FIELD_LABELS
        .iter()
        .map(|label| label.width())
        .max()
        .unwrap_or(0);
    for (i, label) in FIELD_LABELS.iter().enumerate() {
        let y = inner.y + i as u16 * 2;
        if y + 2 > inner.bottom() {
            break;
        }
        let is_focused = i == focused;
        let value_style = if is_focused {
            Style::default()
                .fg(theme::SELECTION_FG)
                .bg(theme::SELECTION_BG)
        } else {
            Style::default().fg(theme::EDITOR_FG)
        };
        let text = if i == NAME { &fields.name } else { &fields.url };
        let value = match i {
            API => Span::styled("Chat Completions", value_style),
            API_KEY if fields.api_key.is_empty() => Span::styled("optional", muted),
            API_KEY => Span::styled("•".repeat(fields.api_key.chars().count()), value_style),
            _ if text.is_empty() => Span::styled("required", muted),
            _ => Span::styled(text.as_str(), value_style),
        };
        let mut spans = vec![
            Span::styled(if is_focused { " › " } else { "   " }, accent),
            Span::styled(
                format!("{}{label}  ", " ".repeat(max_label - label.width())),
                if is_focused {
                    accent.add_modifier(Modifier::BOLD)
                } else {
                    muted
                },
            ),
            Span::styled(": ", muted),
        ];
        let value_column: usize = spans.iter().map(Span::width).sum();
        spans.push(value);
        let line = Line::from(spans);
        frame.render_widget(
            Paragraph::new(line).style(if is_focused {
                Style::default().bg(theme::SELECTION_BG)
            } else {
                Style::default()
            }),
            Rect::new(inner.x, y, inner.width, 1),
        );
        // No colon in the hint: the alignment test counts one per field row.
        if i == URL && fields.url.is_empty() {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    format!(
                        "{}the part before /chat/completions, e.g. https…/v1",
                        " ".repeat(value_column)
                    ),
                    muted,
                )),
                Rect::new(inner.x, y + 1, inner.width, 1),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::custom_providers::CustomProviderStore;
    use providers::{InMemoryCredentialStore, ProviderRegistry};
    use ratatui::{Terminal, backend::TestBackend};

    fn providers() -> Arc<CustomProviders> {
        Arc::new(CustomProviders::new(
            CustomProviderStore::new(std::env::temp_dir().join(format!(
                "alan-provider-overlay-test-{}.json",
                std::process::id()
            ))),
            Arc::new(ProviderRegistry::new([])),
            Arc::new(InMemoryCredentialStore::new()),
        ))
    }

    fn press(overlay: &mut ProviderOverlay, code: KeyCode) -> KeyOutcome {
        overlay.on_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn type_text(overlay: &mut ProviderOverlay, text: &str) {
        for c in text.chars() {
            press(overlay, KeyCode::Char(c));
        }
    }

    fn screen(fields: &FormFields, state: &OverlayState) -> String {
        let mut terminal = Terminal::new(TestBackend::new(80, 18)).unwrap();
        terminal
            .draw(|frame| {
                render_provider_overlay(frame, frame.area(), fields, NAME, state, Mode::Add)
            })
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn typing_fills_the_focused_field_and_tab_moves_on() {
        let mut overlay = ProviderOverlay::add(providers());
        type_text(&mut overlay, "grok");
        press(&mut overlay, KeyCode::Tab);
        type_text(&mut overlay, "https://api.x.ai/v1");
        press(&mut overlay, KeyCode::Tab);
        assert_eq!(press(&mut overlay, KeyCode::Char('x')), KeyOutcome::Ignore);
        press(&mut overlay, KeyCode::Tab);
        type_text(&mut overlay, "xai-1");

        assert_eq!(overlay.fields.name, "grok");
        assert_eq!(overlay.fields.url, "https://api.x.ai/v1");
        assert_eq!(overlay.fields.api_key, "xai-1");
    }

    #[test]
    fn the_name_is_fixed_when_editing() {
        let entry = CustomProviderEntry {
            name: ProviderName::parse("grok").unwrap(),
            url: "https://api.x.ai/v1".into(),
            api: ApiId::ChatCompletions,
        };
        let mut overlay = ProviderOverlay::edit(providers(), entry, Some("xai-1".into()));
        assert_eq!(overlay.focused, URL);
        press(&mut overlay, KeyCode::BackTab);
        assert_eq!(press(&mut overlay, KeyCode::Char('x')), KeyOutcome::Ignore);
        assert_eq!(overlay.fields.name, "grok");
        assert_eq!(overlay.fields.api_key, "xai-1");
    }

    #[test]
    fn an_error_goes_back_to_the_form_with_fields_kept() {
        let mut overlay = ProviderOverlay::add(providers());
        type_text(&mut overlay, "grok");
        overlay.state =
            OverlayState::Error("the API key was rejected (HTTP 401 Unauthorized)".into());

        assert_eq!(press(&mut overlay, KeyCode::Enter), KeyOutcome::Redraw);
        assert_eq!(overlay.state, OverlayState::Editing);
        assert_eq!(overlay.fields.name, "grok");
    }

    #[test]
    fn saved_closes_and_esc_cancels() {
        let mut overlay = ProviderOverlay::add(providers());
        assert_eq!(press(&mut overlay, KeyCode::Esc), KeyOutcome::Close);
        overlay.state = OverlayState::Saved(3);
        assert_eq!(press(&mut overlay, KeyCode::Enter), KeyOutcome::Close);
        overlay.state = OverlayState::Checking;
        assert_eq!(press(&mut overlay, KeyCode::Esc), KeyOutcome::Ignore);
    }

    #[test]
    fn submission_trims_and_treats_a_blank_key_as_none() {
        let fields = FormFields {
            name: " grok ".into(),
            url: " https://api.x.ai/v1 ".into(),
            api_key: "   ".into(),
        };
        let submission = fields.submission().unwrap();
        assert_eq!(submission.name.to_string(), "grok");
        assert_eq!(submission.url, "https://api.x.ai/v1");
        assert_eq!(submission.key, None);
    }

    #[test]
    fn submission_requires_a_url_and_a_valid_name() {
        let no_url = FormFields {
            name: "grok".into(),
            ..FormFields::default()
        };
        assert!(no_url.submission().is_err());
        let bad_name = FormFields {
            name: "Grok".into(),
            url: "http://x".into(),
            ..FormFields::default()
        };
        assert!(bad_name.submission().is_err());
    }

    #[test]
    fn labels_align_and_the_key_is_masked() {
        let fields = FormFields {
            name: "grok".into(),
            url: "https://api.x.ai/v1".into(),
            api_key: "xai-secret".into(),
        };
        let screen = screen(&fields, &OverlayState::Editing);
        assert!(!screen.contains("xai-secret"));
        assert!(screen.contains("••••••••••"));
        let rows: Vec<String> = screen
            .chars()
            .collect::<Vec<_>>()
            .chunks(80)
            .map(|row| row.iter().collect())
            .collect();
        // Char positions, not bytes: the focus marker `›` is multibyte.
        let colons: Vec<usize> = rows
            .iter()
            .filter_map(|row| row.chars().position(|c| c == ':'))
            .collect();
        assert_eq!(colons.len(), FIELD_COUNT, "{rows:#?}");
        assert!(colons.iter().all(|&c| c == colons[0]), "{colons:?}");
    }

    #[test]
    fn an_error_shows_the_reason() {
        let screen = screen(
            &FormFields::default(),
            &OverlayState::Error("the API key was rejected (HTTP 401 Unauthorized)".into()),
        );
        assert!(screen.contains("HTTP 401"));
    }
}
