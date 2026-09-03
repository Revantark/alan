//! A model browser example backed by the OpenRouter models API.
//!
//! Fetches the first ten models, displays their names and descriptions, and
//! opens a detail view for the selected model. Run it with:
//!
//! ```text
//! cargo run -p tui --example models
//! ```

use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use reqwest::Client;
use serde::Deserialize;
use tui::context::Context;
use tui::keymap::KeyMapper;
use tui::{ActionStatus, Component, InputContext, RenderContext, Runtime};

const MODELS_URL: &str = "https://openrouter.ai/api/v1/models?limit=10";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Quit,
    Up,
    Down,
    Open,
    Back,
}

#[derive(Debug)]
enum Message {
    ModelsLoaded(Result<Vec<Model>, String>),
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
            (KeyCode::Up, KeyModifiers::NONE) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                Some(Action::Up)
            }
            (KeyCode::Down, KeyModifiers::NONE) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                Some(Action::Down)
            }
            (KeyCode::Enter, KeyModifiers::NONE) => Some(Action::Open),
            (KeyCode::Esc, KeyModifiers::NONE) => Some(Action::Back),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct Model {
    id: String,
    #[serde(default)]
    canonical_slug: Option<String>,
    #[serde(default)]
    hugging_face_id: Option<String>,
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    context_length: Option<u64>,
    #[serde(default)]
    architecture: Option<Architecture>,
    #[serde(default)]
    pricing: Option<Pricing>,
    #[serde(default)]
    top_provider: Option<TopProvider>,
    #[serde(default)]
    supported_parameters: Vec<String>,
    #[serde(default)]
    reasoning: Option<Reasoning>,
}

#[derive(Debug, Clone, Deserialize)]
struct ModelsResponse {
    data: Vec<Model>,
}

#[derive(Debug, Clone, Deserialize)]
struct Architecture {
    #[serde(default)]
    modality: Option<String>,
    #[serde(default)]
    input_modalities: Vec<String>,
    #[serde(default)]
    output_modalities: Vec<String>,
    #[serde(default)]
    tokenizer: Option<String>,
    #[serde(default)]
    instruct_type: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct Pricing {
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    completion: Option<String>,
    #[serde(default)]
    input_cache_read: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct TopProvider {
    #[serde(default)]
    context_length: Option<u64>,
    #[serde(default)]
    max_completion_tokens: Option<u64>,
    #[serde(default)]
    is_moderated: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
struct Reasoning {
    #[serde(default)]
    mandatory: Option<bool>,
    #[serde(default)]
    default_enabled: Option<bool>,
    #[serde(default)]
    supported_efforts: Vec<String>,
    #[serde(default)]
    default_effort: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Loading,
    List,
    Detail,
    Error,
}

struct ModelsApp {
    screen: Screen,
    models: Vec<Model>,
    selected: usize,
    list_state: ListState,
    detail: Option<usize>,
    error: Option<String>,
}

impl ModelsApp {
    fn new() -> Self {
        Self {
            screen: Screen::Loading,
            models: Vec::new(),
            selected: 0,
            list_state: ListState::default(),
            detail: None,
            error: None,
        }
    }

    fn load_models(&mut self, cx: &mut Context<'_, Self, Action, Message>) {
        cx.spawn(async {
            let result = async {
                let response = Client::new()
                    .get(MODELS_URL)
                    .send()
                    .await
                    .map_err(|error| format!("request failed: {error}"))?
                    .error_for_status()
                    .map_err(|error| format!("OpenRouter returned an error: {error}"))?
                    .json::<ModelsResponse>()
                    .await
                    .map_err(|error| format!("invalid models response: {error}"))?;
                Ok(response.data)
            }
            .await;
            Ok(Message::ModelsLoaded(result))
        });
    }

    fn select(&mut self, index: usize) {
        if self.models.is_empty() {
            self.selected = 0;
            self.list_state.select(None);
        } else {
            self.selected = index.min(self.models.len() - 1);
            self.list_state.select(Some(self.selected));
        }
    }

    fn move_selection(&mut self, amount: isize) {
        if self.models.is_empty() {
            return;
        }
        let len = self.models.len() as isize;
        let next = (self.selected as isize + amount).rem_euclid(len) as usize;
        self.select(next);
    }

    fn active_detail(&self) -> Option<&Model> {
        self.detail.and_then(|index| self.models.get(index))
    }
}

impl Component<Action, Message> for ModelsApp {
    fn init(&mut self, cx: &mut Context<'_, Self, Action, Message>) {
        // The app is the root: with nothing focused, actions route straight
        // here, so no explicit focus is needed.
        self.load_models(cx);
    }

    fn handle_message(&mut self, message: Message, cx: &mut Context<'_, Self, Action, Message>) {
        let Message::ModelsLoaded(result) = message;
        match result {
            Ok(models) => {
                self.models = models;
                self.screen = Screen::List;
                self.error = None;
                self.select(0);
            }
            Err(error) => {
                self.screen = Screen::Error;
                self.error = Some(error);
            }
        }
        cx.notify();
    }

    fn handle_action(
        &mut self,
        action: &Action,
        cx: &mut Context<'_, Self, Action, Message>,
    ) -> ActionStatus {
        match action {
            Action::Quit => {
                cx.quit();
                ActionStatus::Handled
            }
            Action::Up if self.screen == Screen::List => {
                self.move_selection(-1);
                cx.notify();
                ActionStatus::Handled
            }
            Action::Down if self.screen == Screen::List => {
                self.move_selection(1);
                cx.notify();
                ActionStatus::Handled
            }
            Action::Open if self.screen == Screen::List && !self.models.is_empty() => {
                self.detail = Some(self.selected);
                self.screen = Screen::Detail;
                cx.notify();
                ActionStatus::Handled
            }
            Action::Back if self.screen == Screen::Detail => {
                self.detail = None;
                self.screen = Screen::List;
                cx.notify();
                ActionStatus::Handled
            }
            Action::Back | Action::Open | Action::Up | Action::Down => ActionStatus::Handled,
        }
    }

    fn render(&self, frame: &mut Frame, area: Rect, _cx: &RenderContext<'_, Action, Message>) {
        match self.screen {
            Screen::Loading => render_loading(frame, area),
            Screen::List => self.render_list(frame, area),
            Screen::Detail => self.render_detail(frame, area),
            Screen::Error => render_error(
                frame,
                area,
                self.error.as_deref().unwrap_or("unknown error"),
            ),
        }
    }
}

impl ModelsApp {
    fn render_list(&self, frame: &mut Frame, area: Rect) {
        let [header_area, list_area, footer_area] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(area);

        frame.render_widget(
            Paragraph::new(Text::from(vec![
                Line::from(Span::styled(
                    "OpenRouter Models",
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                Line::from("The first 10 models from the OpenRouter catalog"),
            ]))
            .block(Block::default().borders(Borders::ALL)),
            header_area,
        );

        let items = if self.models.is_empty() {
            vec![ListItem::new("No models were returned by OpenRouter.")]
        } else {
            self.models
                .iter()
                .map(|model| {
                    let description = model
                        .description
                        .as_deref()
                        .unwrap_or("No description available.");
                    ListItem::new(Text::from(vec![
                        Line::from(Span::styled(
                            model.name.clone(),
                            Style::default().add_modifier(Modifier::BOLD),
                        )),
                        Line::from(Span::styled(
                            truncate(description, list_area.width.saturating_sub(6) as usize),
                            Style::default().fg(Color::DarkGray),
                        )),
                    ]))
                })
                .collect::<Vec<_>>()
        };

        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(" Models "))
            .highlight_style(Style::default().bg(Color::Blue).fg(Color::White))
            .highlight_symbol("> ");
        let mut state = self.list_state;
        frame.render_stateful_widget(list, list_area, &mut state);
        frame.render_widget(
            Paragraph::new("↑/↓ or j/k: select | Enter: details | q: quit")
                .style(Style::default().fg(Color::DarkGray)),
            footer_area,
        );
    }

    fn render_detail(&self, frame: &mut Frame, area: Rect) {
        let Some(model) = self.active_detail() else {
            render_error(frame, area, "selected model is no longer available");
            return;
        };

        let [header_area, content_area, footer_area] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(area);
        frame.render_widget(
            Paragraph::new(Text::from(vec![
                Line::from(Span::styled(
                    model.name.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                Line::from(model.id.clone()),
            ]))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Model Details "),
            ),
            header_area,
        );

        let mut lines = vec![
            field(
                "Description",
                model.description.as_deref().unwrap_or("Not provided"),
            ),
            field("Canonical slug", optional(model.canonical_slug.as_deref())),
            field(
                "Hugging Face id",
                optional(model.hugging_face_id.as_deref()),
            ),
            field("Context length", optional_number(model.context_length)),
        ];
        if let Some(architecture) = &model.architecture {
            lines.extend([
                field("Modality", optional(architecture.modality.as_deref())),
                field("Input modalities", join(&architecture.input_modalities)),
                field("Output modalities", join(&architecture.output_modalities)),
                field("Tokenizer", optional(architecture.tokenizer.as_deref())),
                field(
                    "Instruct type",
                    optional(architecture.instruct_type.as_deref()),
                ),
            ]);
        }
        if let Some(pricing) = &model.pricing {
            lines.extend([
                field("Prompt price/token", optional(pricing.prompt.as_deref())),
                field(
                    "Completion price/token",
                    optional(pricing.completion.as_deref()),
                ),
                field(
                    "Cache read price/token",
                    optional(pricing.input_cache_read.as_deref()),
                ),
            ]);
        }
        if let Some(provider) = &model.top_provider {
            lines.extend([
                field(
                    "Provider context length",
                    optional_number(provider.context_length),
                ),
                field(
                    "Max completion tokens",
                    optional_number(provider.max_completion_tokens),
                ),
                field(
                    "Moderated",
                    provider
                        .is_moderated
                        .map_or_else(|| "Not provided".into(), |value| value.to_string()),
                ),
            ]);
        }
        if !model.supported_parameters.is_empty() {
            lines.push(field(
                "Supported parameters",
                join(&model.supported_parameters),
            ));
        }
        if let Some(reasoning) = &model.reasoning {
            lines.extend([
                field("Reasoning mandatory", optional_bool(reasoning.mandatory)),
                field(
                    "Reasoning enabled by default",
                    optional_bool(reasoning.default_enabled),
                ),
                field("Reasoning efforts", join(&reasoning.supported_efforts)),
                field(
                    "Default reasoning effort",
                    optional(reasoning.default_effort.as_deref()),
                ),
            ]);
        }

        frame.render_widget(
            Paragraph::new(Text::from(lines))
                .block(Block::default().borders(Borders::ALL))
                .wrap(Wrap { trim: false }),
            content_area,
        );
        frame.render_widget(
            Paragraph::new("Esc: back to models | q: quit")
                .style(Style::default().fg(Color::DarkGray)),
            footer_area,
        );
    }
}

fn field(label: &str, value: impl Into<String>) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{label}: "),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(value.into()),
    ])
}

fn optional(value: Option<&str>) -> String {
    value.unwrap_or("Not provided").to_owned()
}

fn optional_number(value: Option<u64>) -> String {
    value.map_or_else(|| "Not provided".to_owned(), |value| value.to_string())
}

fn optional_bool(value: Option<bool>) -> String {
    value.map_or_else(|| "Not provided".to_owned(), |value| value.to_string())
}

fn join(values: &[String]) -> String {
    if values.is_empty() {
        "Not provided".to_owned()
    } else {
        values.join(", ")
    }
}

fn truncate(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

fn render_loading(frame: &mut Frame, area: Rect) {
    frame.render_widget(
        Paragraph::new("Loading models from OpenRouter…")
            .block(Block::default().borders(Borders::ALL).title(" Models ")),
        area,
    );
}

fn render_error(frame: &mut Frame, area: Rect, error: &str) {
    let text = Text::from(vec![
        Line::from(Span::styled(
            "Could not load models",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(error.to_owned()),
        Line::from(""),
        Line::from("Press q to quit."),
    ]);
    frame.render_widget(
        Paragraph::new(text)
            .block(Block::default().borders(Borders::ALL).title(" Error "))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            Runtime::builder(ModelsApp::new())
                .key_mapper(AppKeyMapper)
                .tick_rate(Duration::from_millis(50))
                .build()
                .run()
                .await
        })?;
    Ok(())
}
