//! The chat surface as a single `tui` component.
//!
//! Owns the vertical stack of the chat: the transcript ([`ChatHistory`]),
//! the pending attachment list, the status line, and the prompt editor. A
//! single owner is what lets the attachments sit *above* the status line —
//! the root could not interleave them because it held only the editor while
//! `ChatHistory` owned the status.

use std::sync::Arc;

use providers::{ModelInfo, ProviderId, ProviderRegistry, bind_model};

use crate::core::settings::{self, Settings, SettingsStore};
use crate::root::AlanAction;
use crate::views::components::{ModelPick, ModelsPicker};
use crate::views::theme;
use crossterm::event::{Event, KeyCode, KeyEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;
use tui::component::{ActionStatus, Component, RenderContext};
use tui::context::Context;
use tui::entity::Entity;

use super::chat_history::ChatHistory;
use super::editor::{PromptEditor, is_plain_prompt};
use super::status::{STATUS_HEIGHT, StatusSnapshot, render_status};

/// Blank rows between the status line and the prompt cursor.
const STATUS_EDITOR_GAP: u16 = 1;

/// Blank rows below the prompt editor.
const EDITOR_BOTTOM_PAD: u16 = 1;

/// Emitted when a submission needs the login overlay opened. The root owns the
/// providers and credentials, so it subscribes and opens the overlay itself.
#[derive(Debug, Clone, Copy)]
pub struct LoginRequested;

/// The chat column: transcript, attachments, status, and prompt editor.
pub struct ChatView {
    /// The transcript component to install on `init`; taken when inserted.
    chat_source: Option<ChatHistory>,
    chat: Option<Entity<ChatHistory>>,
    editor: Option<Entity<PromptEditor>>,
    providers: Arc<ProviderRegistry>,
    model_subscription: Option<tui::Subscription>,
}

impl ChatView {
    pub fn new(chat: ChatHistory, providers: Arc<ProviderRegistry>) -> Self {
        Self {
            chat_source: Some(chat),
            chat: None,
            editor: None,
            providers,
            model_subscription: None,
        }
    }

    /// Dispatch `action` to the transcript component, if installed.
    fn dispatch_chat(
        &self,
        action: &AlanAction,
        cx: &mut Context<'_, Self, AlanAction>,
    ) -> ActionStatus {
        let Some(chat) = self.chat else {
            return ActionStatus::Continue;
        };
        cx.dispatch(chat, action)
    }

    /// Open the `/models` picker, populate it with the cached catalog, refresh
    /// it from the providers in the background, and subscribe to a selection.
    fn open_models_picker(
        &mut self,
        chat: Entity<ChatHistory>,
        cx: &mut Context<'_, Self, AlanAction>,
    ) {
        let providers = Arc::clone(&self.providers);
        let picker = cx.open_overlay(ModelsPicker::new("Select Model", model_labels(&providers)));

        let providers_for_fetch = Arc::clone(&providers);
        let providers_for_items = Arc::clone(&providers);
        let _ = cx.spawn(
            async move {
                fetch_all_models(&providers_for_fetch).await;
                Ok(())
            },
            move |result, _view, cx| {
                if result.is_ok() {
                    let items = model_labels(&providers_for_items);
                    let _ = cx.update(picker, |p| p.set_items(items));
                }
            },
        );

        let providers = Arc::clone(&self.providers);
        self.model_subscription = Some(cx.subscribe::<ModelPick, ModelsPicker, _>(
            picker,
            move |event, _view, _picker, cx| {
                let ModelPick::Chosen(index) = event else {
                    return;
                };
                let Some(model_info) = all_models(&providers).into_iter().nth(*index) else {
                    return;
                };
                let Some(agent) = cx.read(chat, |c| c.agent()) else {
                    return;
                };
                let providers = Arc::clone(&providers);
                let _ = cx.spawn(
                    async move {
                        let provider = providers
                            .providers()
                            .iter()
                            .find(|p| p.id() == model_info.provider)
                            .ok_or_else(|| {
                                tui::TaskError("selected provider is unavailable".into())
                            })?;
                        let settings = settings::get_settings()
                            .await
                            .map_err(|e| tui::TaskError(e.into()))?;
                        let provider_order = settings.provider_order(&model_info.id);
                        let mut options = agent.model_options().await;
                        options.provider_order = provider_order;
                        let model = bind_model(provider.as_ref(), &model_info.id, options)
                            .map_err(|e| tui::TaskError(e.into()))?;
                        let name = model_info.name.clone();
                        let max_context = model_info.context_length;
                        let reasoning_effort = model.reasoning_effort();
                        agent
                            .set_model(model)
                            .await
                            .map_err(|e| tui::TaskError(e.into()))?;
                        persist_model(&model_info.id, &model_info.provider)
                            .await
                            .map_err(|e| tui::TaskError(e.into()))?;
                        Ok((name, max_context, reasoning_effort))
                    },
                    move |result, _view, cx| {
                        let _ = cx.update(chat, |c| match result {
                            Ok((name, max_context, reasoning_effort)) => {
                                c.set_max_context(max_context);
                                c.set_reasoning_effort(reasoning_effort);
                                c.apply_model_switch(name);
                            }
                            Err(e) => c.apply_model_switch_failed(e.to_string()),
                        });
                    },
                );
            },
        ));
    }
}

impl Component<AlanAction> for ChatView {
    fn init(&mut self, cx: &mut Context<'_, Self, AlanAction>)
    where
        Self: Sized,
    {
        let chat = self
            .chat_source
            .take()
            .expect("chat component installed once");
        let mut editor = PromptEditor::new();
        let prompts: Vec<String> = chat
            .entries()
            .iter()
            .filter_map(|e| match e {
                // Defensive: `Entry::Prompt` is normally plain prompts only,
                // but filter anyway so the recall deque stays clean if the
                // contract changes.
                crate::core::Entry::Prompt(text) if is_plain_prompt(text) => Some(text.clone()),
                _ => None,
            })
            .collect();
        editor.seed_history(prompts);
        self.chat = Some(cx.insert(chat));
        self.editor = Some(cx.insert(editor));
        let editor_entity = self.editor.expect("editor entity");
        cx.focus_entity(editor_entity);

        let chat_entity = self.chat.expect("chat installed before spawn");
        let providers_for_fetch = Arc::clone(&self.providers);
        let providers_for_lookup = Arc::clone(&self.providers);
        let _ = cx.spawn(
            async move {
                fetch_all_models(&providers_for_fetch).await;
                Ok(())
            },
            move |result, _view, cx| {
                if result.is_err() {
                    return;
                }
                if let Some(model_id) = cx.read(chat_entity, |c| c.model_name()) {
                    // Find the model in any provider's catalog
                    let max_context = all_models(&providers_for_lookup)
                        .into_iter()
                        .find(|m| m.id == model_id)
                        .and_then(|m| m.context_length);
                    let _ = cx.update(chat_entity, |c| c.set_max_context(max_context));
                }
            },
        );
    }

    fn handle_action(
        &mut self,
        action: &AlanAction,
        cx: &mut Context<'_, Self, AlanAction>,
    ) -> ActionStatus
    where
        Self: Sized,
    {
        // While a blocking operation (e.g. `/summarize-new`) is in flight, all
        // input is swallowed so it cannot race the in-flight task.
        if let Some(chat) = self.chat
            && cx.read(chat, |c| c.is_loading()).unwrap_or(false)
        {
            return ActionStatus::Handled;
        }
        match action {
            // The editor bubbles submissions up to here. Run them on the
            // transcript, then forward a `/login` request to the root as a
            // typed event — the chat cannot open the overlay itself.
            AlanAction::Submit(_) => {
                let Some(chat) = self.chat else {
                    return ActionStatus::Continue;
                };
                let status = cx.dispatch(chat, action);
                if cx.update(chat, |c| c.take_login_request()).unwrap_or(false) {
                    cx.emit(LoginRequested);
                }
                if cx
                    .update(chat, |c| c.take_models_request())
                    .unwrap_or(false)
                {
                    self.open_models_picker(chat, cx);
                }
                status
            }
            // Mode toggle, quit (cancel-or-exit), and wheel scrolling are all
            // owned by the transcript component.
            AlanAction::ToggleMode
            | AlanAction::Quit
            | AlanAction::MouseScrollUp
            | AlanAction::MouseScrollDown => self.dispatch_chat(action, cx),
            AlanAction::Raw(event) => match event {
                // Mouse traffic is owned by the transcript; it hit-tests its
                // own rect and ignores misses.
                Event::Mouse(_) => self.dispatch_chat(action, cx),
                // PageUp/PageDown scroll the transcript, not the editor.
                Event::Key(key)
                    if matches!(key.code, KeyCode::PageUp | KeyCode::PageDown)
                        && key.kind == KeyEventKind::Press =>
                {
                    self.dispatch_chat(action, cx)
                }
                // Esc clears an active transcript selection (the editor gets
                // first refusal and pops attachments).
                Event::Key(key)
                    if key.code == KeyCode::Esc
                        && key.kind == KeyEventKind::Press
                        && self
                            .chat
                            .and_then(|chat| cx.read(chat, |c| c.has_active_selection()))
                            .unwrap_or(false) =>
                {
                    self.dispatch_chat(action, cx)
                }
                // Everything else is editor input, already offered to the
                // focused editor before it reached this component.
                _ => ActionStatus::Continue,
            },
            _ => ActionStatus::Continue,
        }
    }

    fn render(&self, frame: &mut Frame, area: Rect, cx: &RenderContext<'_, AlanAction>) {
        let Some(chat) = self.chat else {
            return;
        };
        let Some(editor) = self.editor else {
            return;
        };

        // Size the editor and attachment bands from the editor's own state so
        // they always reflect the current buffer and attached images.
        let editor_width = area.width.saturating_sub(theme::PROMPT_GUTTER);
        let editor_rows = cx.read(editor, |e| e.rows(editor_width)).unwrap_or(1);
        let attachment_height = cx.read(editor, |e| e.attachment_height()).unwrap_or(0);

        let [
            chat_area,
            attachment_area,
            status_area,
            _gap,
            editor_area,
            _bottom_pad,
        ] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(attachment_height),
            Constraint::Length(STATUS_HEIGHT),
            Constraint::Length(STATUS_EDITOR_GAP),
            Constraint::Length(editor_rows),
            Constraint::Length(EDITOR_BOTTOM_PAD),
        ])
        .areas(area);

        cx.render_entity(chat, frame, chat_area);
        render_attachments(frame, attachment_area, editor, cx);
        // Paint the whole footer (status through bottom pad) with the editor
        // background, so the gap and bottom padding don't fall back to the
        // terminal default.
        let footer = Rect {
            x: area.x,
            y: status_area.y,
            width: area.width,
            height: area.bottom().saturating_sub(status_area.y),
        };
        frame.render_widget(
            Paragraph::new("").style(Style::default().bg(theme::EDITOR_BG)),
            footer,
        );
        if let Some(snap) = cx.read(chat, |c| c.snapshot()).flatten() {
            render_status(frame, status_area, &StatusSnapshot::from_snapshot(&snap));
        }
        cx.render_entity(editor, frame, editor_area);
    }
}

/// Paint the pending attachment list into `area`.
fn render_attachments(
    frame: &mut Frame,
    area: Rect,
    editor: Entity<PromptEditor>,
    cx: &RenderContext<'_, AlanAction>,
) {
    if area.height == 0 {
        return;
    }
    let Some(names) = cx.read(editor, |e| {
        e.attachments()
            .iter()
            .map(|attachment| attachment.name.clone())
            .collect::<Vec<_>>()
    }) else {
        return;
    };
    if names.is_empty() {
        return;
    }

    let mut lines: Vec<Line<'static>> = vec![
        Line::from("\n"),
        Line::from(Span::styled(
            "  Attachments  (esc removes last)",
            Style::default().fg(theme::ATTACHMENT_FG).bold(),
        )),
    ];
    for name in names {
        lines.push(Line::from(Span::styled(
            format!("   - {name}"),
            Style::default().fg(theme::ATTACHMENT_FG),
        )));
    }
    let attachments =
        Paragraph::new(Text::from(lines)).style(Style::default().bg(theme::ATTACHMENT_BG));
    frame.render_widget(attachments, area);
}

/// Refresh every provider's model catalog, logging (but not failing on)
/// individual provider errors.
async fn fetch_all_models(providers: &ProviderRegistry) {
    for provider in providers.providers() {
        if let Err(e) = provider.fetch_models().await {
            tracing::warn!(
                "Failed to fetch models for provider {:?}: {}",
                provider.id(),
                e
            );
        }
    }
}

/// Flatten every provider's catalog into a single list of models.
fn all_models(providers: &ProviderRegistry) -> Vec<ModelInfo> {
    providers
        .providers()
        .iter()
        .flat_map(|p| p.models())
        .collect()
}

/// Render each known model as a `"<provider> — <name>"` picker label.
fn model_labels(providers: &ProviderRegistry) -> Vec<String> {
    all_models(providers)
        .into_iter()
        .map(|m| format!("{} — {}", m.provider, m.name))
        .collect()
}

async fn persist_model(model_id: &str, provider_id: &ProviderId) -> anyhow::Result<()> {
    let store = SettingsStore::<Settings>::new(settings::default_settings_path()?);
    let mut settings = store.load().await?.unwrap_or_default();
    settings.model = Some(model_id.to_string());
    settings.provider = Some(provider_id.to_string());

    store.save(&settings).await
}
