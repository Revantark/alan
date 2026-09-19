use crate::core::chat::ChatController;
use crate::core::settings::{self, Settings, SettingsStore};
use crate::core::{Activity, Entry};
use crate::root::{AlanAction, PromptSubmission};
use crate::views::components::{ForkEvent, ForkOverlay, ModelPick, ModelsPicker};
use crate::views::theme;
use agent::{AgentError, AgentEvent, AgentStream};
use alan_tui::component::{ActionStatus, Component, RenderContext};
use alan_tui::context::Context;
use alan_tui::entity::Entity;
use alan_tui::{Subscription, SubscriptionEvent};
use crossterm::event::{Event, KeyCode, KeyEventKind};
use futures_util::Stream;
use llm::Usage;
use providers::{ModelInfo, ProviderId, ProviderRegistry, bind_model};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;
use std::sync::Arc;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::chat_history::ChatHistory;
use super::editor::{PromptEditor, is_plain_prompt};
use super::status::{STATUS_HEIGHT, Status, StatusInputs};

/// Rows between the status line and the prompt cursor.
const STATUS_EDITOR_GAP: u16 = 1;

/// Rows below the prompt editor.
const EDITOR_BOTTOM_PAD: u16 = 1;

/// Rows of the steering band shown while a steering prompt is queued.
const STEER_BAND_HEIGHT: u16 = 3;

fn truncate_single_line(text: &str, max_width: usize) -> String {
    let text = text.lines().next().unwrap_or(text);
    if Line::from(text).width() <= max_width {
        return text.to_owned();
    }
    let budget = max_width.saturating_sub(1);
    let mut out = String::new();
    for ch in text.chars() {
        let w = ch.width().unwrap_or(0);
        if out.width() + w > budget {
            break;
        }
        out.push(ch);
    }
    format!("{out}…")
}

fn render_steering(frame: &mut Frame, area: Rect, text: Option<&str>) {
    let Some(text) = text else {
        return;
    };
    if area.height == 0 || area.width == 0 {
        return;
    }
    let content_width = area
        .width
        .saturating_sub((theme::CHAT_PADDING * 2) as u16)
        .max(1) as usize;
    let line = truncate_single_line(text, content_width);
    let style = Style::default().fg(theme::STEER_FG);
    let [top, middle, _bottom] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);
    frame.render_widget(Paragraph::new(""), top);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(
                "{}{} {}",
                " ".repeat(theme::CHAT_PADDING),
                theme::STEER_MARKER,
                line
            ),
            style,
        ))),
        middle,
    );
    frame.render_widget(Paragraph::new(""), _bottom);
}

const STREAM_REPAINT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(32);

#[derive(Debug, Clone, Copy)]
pub struct LoginRequested;

/// The chat column: transcript, attachments, status, and prompt editor.
pub struct ChatView {
    controller: ChatController,
    chat: Option<Entity<ChatHistory>>,
    status: Option<Entity<Status>>,
    editor: Option<Entity<PromptEditor>>,

    providers: Arc<ProviderRegistry>,
    model_subscription: Option<alan_tui::Subscription>,
    /// Subscription to the in-flight agent stream. Dropping it cancels the run.
    prompt: Option<Subscription>,
    /// Fixed-rate repaint ticker, alive only while the agent is streaming.
    /// Keeps render cadence independent of the token rate.
    stream_repaint: Option<Subscription>,
    fork_in_flight: bool,
}

impl ChatView {
    pub fn new(controller: ChatController, providers: Arc<ProviderRegistry>) -> Self {
        Self {
            controller,
            chat: None,
            status: None,
            editor: None,
            providers,
            model_subscription: None,
            prompt: None,
            stream_repaint: None,
            fork_in_flight: false,
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

    /// Current activity, projected from the controller for the status band.
    fn activity(&self) -> Activity {
        if self.controller.is_busy() {
            Activity::Thinking
        } else if let Some(label) = self.controller.loading() {
            Activity::Loading(label.to_owned())
        } else {
            Activity::Idle
        }
    }

    fn sync_status(&mut self, cx: &mut Context<'_, Self, AlanAction>) {
        let activity = self.activity();
        if let Some(status) = self.status {
            let loading = matches!(activity, Activity::Loading(_));
            cx.dispatch(status, &AlanAction::SetLoadingDots(loading));
        }
        cx.notify();
    }

    fn handle_submit(
        &mut self,
        submission: PromptSubmission,
        cx: &mut Context<'_, Self, AlanAction>,
    ) {
        let controller = &mut self.controller;

        if let Some(command) = crate::core::SlashCommand::parse(&submission.text) {
            use crate::core::SlashCommand as Cmd;
            match command {
                Cmd::Login => cx.emit(LoginRequested),
                Cmd::Plan => controller.set_mode(agent::Mode::Plan),
                Cmd::Review => controller.set_mode(agent::Mode::Review),
                Cmd::Normal => controller.set_mode(agent::Mode::Normal),

                Cmd::Fork => self.request_fork(&submission.text, cx),
                Cmd::Effort => self.apply_effort(&submission.text, cx),
                Cmd::SummarizeNew => self.start_summarize_new(cx, &submission.text),
                Cmd::ModelProviders => self.apply_model_provider(cx, &submission.text),
                Cmd::Rename => self.rename_session(cx, &submission.text),

                Cmd::Help => controller.push_info(crate::core::SlashCommand::help()),
                Cmd::New => self.start_new_session(cx),
                Cmd::Models => self.open_models_picker(cx),
                Cmd::Quit => cx.quit(),
            }
            cx.notify();
            return;
        }

        let text = submission.text.trim().to_owned();

        // Steering: a plain prompt submitted while a run streams is queued
        // for the agent's next LLM round instead of starting a new run.
        // Slash commands keep executing normally.
        if controller.is_busy() {
            controller.steer(text.clone());
            // Defer to a task: this callback runs with the entity store
            // locked, so dispatching to the editor inline would re-enter that
            // non-reentrant lock and freeze the UI.
            if self.editor.is_some() {
                cx.spawn(
                    async move { Ok::<String, alan_tui::TaskError>(text) },
                    |result, view, cx| {
                        if let Ok(text) = result
                            && let Some(editor) = view.editor
                        {
                            cx.dispatch(editor, &AlanAction::SetSteering(Some(text)));
                        }
                    },
                );
            }
            self.sync_status(cx);
            return;
        }

        let Some(stream) = controller.submit(text, submission.images) else {
            return;
        };
        // A new prompt pins the transcript to the newest content.
        if let Some(chat) = self.chat {
            cx.update(chat, |c| c.stick_to_bottom());
        }
        self.prompt = Some(cx.subscribe_stream(agent_events(stream), handle_agent_stream_event));
        self.ensure_stream_repaint(cx);
        cx.notify();
    }

    /// `/new`: reset to a fresh, empty session.
    fn start_new_session(&mut self, cx: &mut Context<'_, Self, AlanAction>) {
        if self.controller.is_busy() {
            return;
        }
        let agent = self.controller.agent();
        cx.spawn(
            async move {
                agent
                    .reset_session()
                    .await
                    .map_err(|error| alan_tui::TaskError(Box::new(error)))
            },
            move |result, view, _cx| match result {
                Ok(()) => {
                    view.controller.clear_transcript();
                    view.controller.push_info("Started a new session.");
                }
                Err(error) => view
                    .controller
                    .push_info(format!("failed to start new session: {error}")),
            },
        );
    }

    fn request_fork(&mut self, text: &str, cx: &mut Context<'_, Self, AlanAction>) {
        if self.controller.is_busy() {
            self.controller
                .push_info("cannot fork while a response is streaming".to_owned());
            return;
        }
        if let Some((_, args)) = crate::core::SlashCommand::parse_with_args(text)
            && !args.trim().is_empty()
        {
            self.controller.push_info("usage: /fork".to_owned());
            return;
        }
        self.open_fork_overlay(cx);
    }

    fn apply_model_provider(&mut self, cx: &mut Context<'_, Self, AlanAction>, text: &str) {
        if self.controller.is_busy() {
            return;
        }
        let agent = self.controller.agent();

        let provider_order =
            match crate::core::SlashCommand::parse_with_args(text).map(|(_, args)| args) {
                Some(args) if args.trim().eq_ignore_ascii_case("none") => Ok(Vec::new()),
                Some(args) => parse_provider_order(args),
                None => return,
            };
        let provider_order = match provider_order {
            Ok(order) => order,
            Err(error) => {
                self.controller.push_info(format!(
                    "usage: /providers <provider1,provider2,...>: {error}"
                ));

                return;
            }
        };

        cx.spawn(
            async move {
                let model_id = agent.info().await.id;
                agent
                    .set_provider_order(provider_order.clone())
                    .await
                    .map_err(|error| alan_tui::TaskError(Box::new(error)))?;

                persist_provider_order(&model_id, &provider_order)
                    .await
                    .map_err(|error| alan_tui::TaskError(error.into()))?;

                Ok::<_, alan_tui::TaskError>(if provider_order.is_empty() {
                    "provider order cleared (using default)".to_owned()
                } else {
                    format!("provider order set to {}", provider_order.join(", "))
                })
            },
            move |result, view, cx| match result {
                Ok(msg) => {
                    view.controller.push_info(msg);
                    cx.notify();
                }
                Err(error) => {
                    view.controller
                        .push_info(format!("failed to set provider order: {error}"));
                    cx.notify();
                }
            },
        );
    }

    fn start_summarize_new(&mut self, cx: &mut Context<'_, Self, AlanAction>, text: &str) {
        if self.controller.is_busy() {
            return;
        }
        let agent = self.controller.agent();
        let focus = crate::core::SlashCommand::parse_with_args(text)
            .map(|(_, args)| args.trim().to_owned())
            .filter(|args| !args.is_empty());

        self.controller.set_loading(Some("summarizing".to_owned()));
        self.sync_status(cx);

        cx.spawn(
            async move {
                let summary = agent
                    .summarize(focus.as_deref())
                    .await
                    .map_err(|error| alan_tui::TaskError(Box::new(error)))?;
                let seed = vec![agent::AgentMessage::user(format!(
                    "Session handoff — continue from this state:\n\n{summary}"
                ))];
                agent
                    .reset_session_with(seed)
                    .await
                    .map_err(|error| alan_tui::TaskError(Box::new(error)))?;
                Ok::<(), alan_tui::TaskError>(())
            },
            move |result, view, cx| {
                view.controller.set_loading(None);
                view.controller.clear_transcript();
                match result {
                    Ok(()) => view.controller.push_info("Summarized into a new session."),
                    Err(error) => view
                        .controller
                        .push_info(format!("failed to summarize session: {error}")),
                }
                view.sync_status(cx);
            },
        );
    }

    fn apply_effort(&mut self, text: &str, cx: &mut Context<'_, Self, AlanAction>) {
        if self.controller.is_busy() {
            return;
        }
        let agent = self.controller.agent();

        let effort = match crate::core::SlashCommand::parse_with_args(text).map(|(_, args)| args) {
            Some(args) => match crate::core::SlashCommand::parse_effort(args) {
                Some(effort) => effort,
                None => {
                    self.controller.push_info(
                        "usage: /effort <none|minimal|low|medium|high|xhigh|max>".to_owned(),
                    );
                    return;
                }
            },
            None => return,
        };

        let agent = agent.clone();
        cx.spawn(
            async move {
                agent
                    .set_reasoning_effort(effort)
                    .await
                    .map_err(|error| alan_tui::TaskError(Box::new(error)))?;

                persist_reasoning_effort(effort)
                    .await
                    .map_err(|error| alan_tui::TaskError(error.into()))?;

                Ok::<_, alan_tui::TaskError>(format!("reasoning effort set to {effort}"))
            },
            move |result, view, cx| match result {
                Ok(msg) => {
                    view.controller.set_reasoning_effort(effort);
                    view.controller.push_info(msg);
                    cx.notify();
                }
                Err(error) => {
                    view.controller
                        .push_info(format!("failed to set reasoning effort: {error}"));
                    cx.notify();
                }
            },
        );
    }

    fn rename_session(&mut self, cx: &mut Context<'_, Self, AlanAction>, text: &str) {
        let args = crate::core::SlashCommand::parse_with_args(text)
            .map(|(_, args)| args.trim().to_owned())
            .filter(|args| !args.is_empty());
        let name = match args {
            Some(name) => name,
            None => {
                self.controller
                    .push_info("usage: /rename <name>".to_owned());
                return;
            }
        };
        let agent = self.controller.agent();
        cx.spawn(
            async move {
                agent
                    .rename_session(&name)
                    .await
                    .map_err(|error| alan_tui::TaskError(Box::new(error)))?;

                Ok::<_, alan_tui::TaskError>(format!("Session renamed to {name}"))
            },
            move |result, view, cx| match result {
                Ok(msg) => {
                    view.controller.push_info(msg);
                    cx.notify();
                }
                Err(error) => {
                    view.controller
                        .push_info(format!("failed to rename session: {error}"));
                    cx.notify();
                }
            },
        );
    }

    /// Start the fixed-rate repaint ticker if a run is in flight and it is not
    /// already running. The ticker repaints the transcript at
    /// `STREAM_REPAINT_INTERVAL`; it is cancelled once the run finishes.
    fn ensure_stream_repaint(&mut self, cx: &mut Context<'_, Self, AlanAction>) {
        if self.stream_repaint.is_some() || !self.controller.is_busy() {
            return;
        }
        self.stream_repaint = Some(cx.subscribe_stream(
            stream_repaint_ticks(),
            |event, view, cx| {
                if matches!(event, SubscriptionEvent::Closed) || !view.controller.is_busy() {
                    view.stream_repaint = None;
                    return;
                }
                cx.notify();
            },
        ));
    }

    fn open_models_picker(&mut self, cx: &mut Context<'_, Self, AlanAction>) {
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
            move |event, view, _picker, cx| {
                let ModelPick::Chosen(index) = event else {
                    return;
                };
                let Some(model_info) = all_models(&providers).into_iter().nth(*index) else {
                    return;
                };
                let Some(agent) = Some(view.controller.agent()) else {
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
                                alan_tui::TaskError("selected provider is unavailable".into())
                            })?;

                        let settings = settings::get_settings()
                            .await
                            .map_err(|e| alan_tui::TaskError(e.into()))?;

                        let provider_order = settings.provider_order(&model_info.id);
                        let mut options = agent.model_options().await;
                        options.provider_order = provider_order;

                        let model = bind_model(provider.as_ref(), &model_info.id, options)
                            .map_err(|e| alan_tui::TaskError(e.into()))?;
                        let name = model_info.name.clone();
                        let max_context = model_info.context_length;
                        let reasoning_effort = model.reasoning_effort();

                        agent
                            .set_model(model)
                            .await
                            .map_err(|e| alan_tui::TaskError(e.into()))?;
                        persist_model(&model_info.id, &model_info.provider)
                            .await
                            .map_err(|e| alan_tui::TaskError(e.into()))?;
                        Ok((name, max_context, reasoning_effort))
                    },
                    move |result, view, cx| {
                        match result {
                            Ok((name, max_context, reasoning_effort)) => {
                                view.controller.set_max_context(max_context);
                                view.controller.set_reasoning_effort(reasoning_effort);
                                view.controller.apply_model_switch(name);
                            }
                            Err(e) => view.controller.apply_model_switch_failed(e.to_string()),
                        }
                        cx.notify();
                    },
                );
            },
        ));
    }

    fn open_fork_overlay(&mut self, cx: &mut Context<'_, Self, AlanAction>) {
        let agent = self.controller.agent();
        self.fork_in_flight = true;
        let picker = cx.open_overlay(ForkOverlay::new(Arc::clone(&agent)));
        cx.subscribe_once::<ForkEvent, ForkOverlay, _>(picker, move |_event, view, _picker, cx| {
            let ForkEvent::Chosen { end_index } = *_event else {
                // Closing the picker without choosing must release the
                // latch, otherwise every later `/fork` is rejected.
                view.fork_in_flight = false;
                return;
            };
            let agent = view.controller.agent();
            let _ = cx.spawn(
                async move {
                    agent
                        .fork_session(end_index)
                        .await
                        .map_err(|e| alan_tui::TaskError(Box::new(e)))?;
                    let messages = agent.messages().await;
                    Ok::<_, alan_tui::TaskError>(messages)
                },
                move |result, view, cx| {
                    view.fork_in_flight = false;
                    match result {
                        Ok(messages) => {
                            view.controller.apply_restored(
                                messages,
                                Usage::default(),
                                view.controller.model_name(),
                                view.controller.max_context(),
                            );
                            view.controller.push_info("forked session".to_owned());
                            if let Some(editor) = view.editor {
                                let prompts = recall_prompts(view.controller.entries());
                                cx.update(editor, |e| e.seed_history(prompts));
                            }
                        }
                        Err(error) => {
                            view.controller
                                .push_info(format!("failed to fork: {error}"));
                        }
                    }
                    cx.notify();
                },
            );
        });
    }
}

impl Component<AlanAction> for ChatView {
    fn init(&mut self, cx: &mut Context<'_, Self, AlanAction>)
    where
        Self: Sized,
    {
        let mut editor = PromptEditor::new();
        let prompts: Vec<String> = recall_prompts(self.controller.entries());
        editor.seed_history(prompts);
        self.chat = Some(cx.insert(ChatHistory::new()));
        self.status = Some(cx.insert(Status::new()));
        self.editor = Some(cx.insert(editor));
        let editor_entity = self.editor.expect("editor entity");
        cx.focus_entity(editor_entity);

        let providers_for_fetch = Arc::clone(&self.providers);
        let providers_for_lookup = Arc::clone(&self.providers);
        let _ = cx.spawn(
            async move {
                fetch_all_models(&providers_for_fetch).await;
                Ok(())
            },
            move |result, view, cx| {
                if result.is_err() {
                    return;
                }
                // Find the model in any provider's catalog
                if let Some(model_id) = view.controller.model_name().into() {
                    let max_context = all_models(&providers_for_lookup)
                        .into_iter()
                        .find(|m| m.id == model_id)
                        .and_then(|m| m.context_length);
                    view.controller.set_max_context(max_context);
                }
                cx.notify();
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
        if self.controller.loading().is_some() {
            return ActionStatus::Handled;
        }

        match action {
            AlanAction::Submit(submission) => {
                self.handle_submit(submission.clone(), cx);
                self.sync_status(cx);
                ActionStatus::Handled
            }
            AlanAction::ToggleMode => {
                self.controller.toggle_mode();
                cx.notify();
                ActionStatus::Handled
            }
            AlanAction::CancelSteer => {
                if self.controller.take_steering().is_some() {
                    if let Some(editor) = self.editor {
                        cx.dispatch(editor, &AlanAction::SetSteering(None));
                    }
                    cx.notify();
                }
                ActionStatus::Handled
            }
            AlanAction::Quit => {
                if self.controller.is_busy() {
                    self.prompt = None;
                    self.stream_repaint = None;
                    self.controller.finish_stream();
                    cx.notify();
                } else {
                    cx.quit();
                }
                ActionStatus::Handled
            }
            AlanAction::Raw(event) => match event {
                Event::Key(key)
                    if key.code == KeyCode::Esc
                        && key.kind == KeyEventKind::Press
                        && self.chat.is_some_and(|chat| {
                            cx.read(chat, |c| c.has_active_selection()).unwrap_or(false)
                        }) =>
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

    fn render(&self, frame: &mut Frame, area: Rect, cx: &RenderContext<'_, '_, AlanAction>) {
        let Some(chat) = self.chat else {
            return;
        };
        let Some(status) = self.status else {
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
        let steer_text = self.controller.steering().map(str::to_owned);
        let steer_height = if steer_text.is_some() {
            STEER_BAND_HEIGHT
        } else {
            0
        };

        let [
            chat_area,
            steer_area,
            attachment_area,
            status_area,
            _gap,
            editor_area,
            _bottom_pad,
        ] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(steer_height),
            Constraint::Length(attachment_height),
            Constraint::Length(STATUS_HEIGHT),
            Constraint::Length(STATUS_EDITOR_GAP),
            Constraint::Length(editor_rows),
            Constraint::Length(EDITOR_BOTTOM_PAD),
        ])
        .areas(area);

        cx.render_with_state(chat, frame, chat_area, &self.controller);

        render_steering(frame, steer_area, steer_text.as_deref());

        render_attachments(frame, attachment_area, editor, cx);

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

        let inputs = StatusInputs {
            activity: self.activity(),
            mode: self.controller.mode(),
            usage: self.controller.usage(),
            model_name: self.controller.model_name(),
            max_context: self.controller.max_context(),
            reasoning_effort: self.controller.reasoning_effort(),
        };
        cx.render_with_state(status, frame, status_area, &inputs);
        cx.render_entity(editor, frame, editor_area);
    }
}

fn render_attachments(
    frame: &mut Frame,
    area: Rect,
    editor: Entity<PromptEditor>,
    cx: &RenderContext<'_, '_, AlanAction>,
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

fn all_models(providers: &ProviderRegistry) -> Vec<ModelInfo> {
    providers
        .providers()
        .iter()
        .flat_map(|p| p.models())
        .collect()
}

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

fn agent_events(
    stream: AgentStream,
) -> impl Stream<Item = Result<AgentEvent, agent::AgentError>> + Send + 'static {
    futures_util::stream::unfold(stream, |mut stream| async move {
        stream.recv().await.map(|event| (event, stream))
    })
}

/// Shared event handler for agent-stream subscriptions. Both the initial
/// submit path and the steer auto-submit path wire their subscriptions
/// through this function so the `Closed` → auto-submit logic is not
/// duplicated.
fn handle_agent_stream_event(
    event: SubscriptionEvent<Result<AgentEvent, AgentError>>,
    view: &mut ChatView,
    cx: &mut Context<'_, ChatView, AlanAction>,
) {
    match event {
        SubscriptionEvent::Item(result) => {
            view.controller.apply_event(result);
        }
        SubscriptionEvent::Closed => {
            view.controller.disconnect_stream();
            if view.controller.steering().is_some() {
                // Defer to a task: this callback runs while the event loop
                // holds the entity-store mutex, so calling `subscribe_stream`
                // (which mutates `runtime_state.subscriptions`) inline would
                // re-enter that non-reentrant mutex and deadlock the UI.
                cx.spawn(
                    async { Ok::<(), alan_tui::TaskError>(()) },
                    |result, view, cx| {
                        if result.is_ok()
                            && let Some(text) = view.controller.take_steering()
                        {
                            if let Some(editor) = view.editor {
                                cx.dispatch(editor, &AlanAction::SetSteering(None));
                            }
                            if let Some(stream) = view.controller.submit(text, Vec::new()) {
                                if let Some(chat) = view.chat {
                                    cx.update(chat, |c| c.stick_to_bottom());
                                }
                                view.prompt = Some(cx.subscribe_stream(
                                    agent_events(stream),
                                    handle_agent_stream_event,
                                ));
                                view.ensure_stream_repaint(cx);
                            }
                            view.sync_status(cx);
                        }
                    },
                );
            }
        }
    }
    if !view.controller.is_busy() {
        view.stream_repaint = None;
        cx.notify();
    }
}

fn stream_repaint_ticks() -> impl Stream<Item = ()> + Send + 'static {
    futures_util::stream::unfold((), |_| async {
        tokio::time::sleep(STREAM_REPAINT_INTERVAL).await;
        Some(((), ()))
    })
}

fn parse_provider_order(value: &str) -> anyhow::Result<Vec<String>> {
    let order = value
        .split(',')
        .map(|entry| entry.trim().to_string())
        .filter(|entry| !entry.is_empty())
        .collect::<Vec<_>>();
    if order.is_empty() {
        return Err(anyhow::anyhow!("provider order must not be empty"));
    }
    Ok(order)
}

async fn persist_reasoning_effort(effort: llm::ReasoningEffort) -> anyhow::Result<()> {
    let store = SettingsStore::<Settings>::new(settings::default_settings_path()?);
    let mut settings = store.load().await?.unwrap_or_default();
    settings.reasoning = Some(effort);
    store.save(&settings).await
}

async fn persist_provider_order(model: &str, provider_order: &[String]) -> anyhow::Result<()> {
    let store = SettingsStore::<Settings>::new(settings::default_settings_path()?);
    let mut settings = store.load().await?.unwrap_or_default();
    if provider_order.is_empty() {
        settings.provider_orders.remove(model);
    } else {
        settings
            .provider_orders
            .insert(model.to_owned(), provider_order.to_vec());
    }
    store.save(&settings).await
}

fn recall_prompts(entries: &[Entry]) -> Vec<String> {
    entries
        .iter()
        .filter_map(|e| match e {
            // Defensive: `Entry::Prompt` is normally plain prompts only,
            // but filter anyway so the recall deque stays clean if the
            // contract changes.
            Entry::Prompt(text) if is_plain_prompt(text) => Some(text.clone()),
            _ => None,
        })
        .collect()
}
