//! The chat column: transcript, attachments, status, and prompt editor.
//!
//! `ChatView` owns the agent stream, the model/provider state, and the
//! prompt editor. Sub-modules handle session lifecycle ([`session`]),
//! model/provider management ([`models`]), attachments ([`attachments`]),
//! and steering ([`steering`]).

mod attachments;
mod models;
mod permissions;
mod session;
mod steering;

use crate::core::chat::ChatController;
use crate::core::permissions::PermissionHandler;
use crate::core::permissions::PermissionRequest;
use crate::core::permissions::{Policy, ToolPolicy};
use crate::core::settings::{self, Settings, SettingsStore};
use crate::core::{Activity, Entry};
use crate::root::{AlanAction, PromptSubmission};
use crate::views::theme;
use agent::{AgentError, AgentEvent, AgentStream};
use alan_tui::TaskError;
use alan_tui::component::{ActionStatus, Component, RenderContext};
use alan_tui::context::Context;
use alan_tui::entity::Entity;
use alan_tui::{Subscription, SubscriptionEvent};
use crossterm::event::{Event, KeyCode, KeyEventKind};
use futures_util::Stream;
use providers::ProviderRegistry;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::widgets::Paragraph;
use std::sync::Arc;

use super::chat_history::ChatHistory;
use super::editor::{PromptEditor, is_plain_prompt};
use super::status::{STATUS_HEIGHT, Status, StatusInputs};

/// Rows between the status line and the prompt cursor.
const STATUS_EDITOR_GAP: u16 = 1;

/// Rows below the prompt editor.
const EDITOR_BOTTOM_PAD: u16 = 1;

/// Fixed-rate repaint interval while an agent stream is in flight.
const STREAM_REPAINT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(32);

/// Action chosen from the local-model picker.
pub(crate) enum LocalPick {
    Remove,
    Edit,
}

#[derive(Debug, Clone, Copy)]
pub struct LoginRequested;

/// The chat column: transcript, attachments, status, and prompt editor.
pub struct ChatView {
    controller: ChatController,
    chat: Option<Entity<ChatHistory>>,
    status: Option<Entity<Status>>,
    editor: Option<Entity<PromptEditor>>,
    /// Pending tool-authorization request, if any. While set, the
    /// permission band is shown and focused; `1`/`0` answer it.
    pending_permission: Option<PermissionRequest>,
    permission: Option<Entity<permissions::PermissionPrompt>>,

    providers: Arc<ProviderRegistry>,
    permission_handler: PermissionHandler,
    policy: ToolPolicy,
    /// Subscription to the permission-request stream.
    permission_subscription: Option<Subscription>,
    model_subscription: Option<alan_tui::Subscription>,
    /// Subscription to the in-flight agent stream. Dropping it cancels the run.
    prompt: Option<Subscription>,
    /// Fixed-rate repaint ticker, alive only while the agent is streaming.
    /// Keeps render cadence independent of the token rate.
    stream_repaint: Option<Subscription>,
    fork_in_flight: bool,
}

impl ChatView {
    pub fn new(
        controller: ChatController,
        providers: Arc<ProviderRegistry>,
        permission_handler: PermissionHandler,
        policy: ToolPolicy,
    ) -> Self {
        Self {
            controller,
            chat: None,
            status: None,
            editor: None,
            pending_permission: None,
            permission: None,
            providers,
            permission_handler,
            policy,
            permission_subscription: None,
            model_subscription: None,
            prompt: None,
            stream_repaint: None,
            fork_in_flight: false,
        }
    }

    fn set_tool_policy(&mut self, policy: Policy, cx: &mut Context<'_, Self, AlanAction>) {
        self.policy.set_policy(policy);

        cx.spawn(
            async move {
                persist_tool_policy(policy)
                    .await
                    .map(|()| policy)
                    .map_err(|e| TaskError(e.into()))
            },
            |result, view, cx| {
                match result {
                    Ok(policy) => {
                        view.controller
                            .push_info(format!("tool permission policy set to {policy}"));
                    }
                    Err(error) => {
                        view.controller
                            .push_info(format!("unable to persist policy: {error}"));
                    }
                }

                cx.notify();
            },
        );
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

    fn handle_permission(
        &mut self,
        action: &AlanAction,
        cx: &mut Context<'_, Self, AlanAction>,
    ) -> bool {
        let Some(request) = self.pending_permission.as_ref() else {
            return false;
        };

        let AlanAction::PermissionAnswered(decision) = action else {
            return false;
        };

        let request_id = request.id;
        self.pending_permission = None;

        self.permission_handler
            .respond(request_id, decision.clone());

        if *decision == crate::core::permissions::Answer::Stop && self.controller.is_busy() {
            self.stop_stream();
        }

        cx.notify();
        true
    }

    fn handle_quit(&mut self, cx: &mut Context<'_, Self, AlanAction>) {
        if self.controller.is_busy() {
            self.stop_stream();
            cx.notify();
        } else {
            cx.quit();
        }
    }

    fn stop_stream(&mut self) {
        self.prompt = None;
        self.stream_repaint = None;
        self.controller.finish_stream();
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

                Cmd::Fork => session::request_fork(self, &submission.text, cx),
                Cmd::Effort => session::apply_effort(self, &submission.text, cx),
                Cmd::SummarizeNew => session::start_summarize_new(self, cx, &submission.text),
                Cmd::ModelProviders => models::apply_model_provider(self, cx, &submission.text),
                Cmd::Rename => session::rename_session(self, cx, &submission.text),

                Cmd::ToolFree => self.set_tool_policy(Policy::Free, cx),
                Cmd::ToolSlip => self.set_tool_policy(Policy::Slip, cx),
                Cmd::ToolStrict => self.set_tool_policy(Policy::Strict, cx),

                Cmd::Help => controller.push_info(crate::core::SlashCommand::help()),
                Cmd::New => session::start_new_session(self, cx),
                Cmd::Models => models::open_models_picker(self, cx),
                Cmd::Local => models::handle_local_command(self, &submission.text, cx),
                Cmd::Quit => cx.quit(),
            }
            cx.notify();
            return;
        }

        let text = submission.text.trim().to_owned();

        if controller.is_busy() {
            controller.steer(text.clone());
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

    fn fetch_models(&mut self, cx: &mut Context<'_, Self, AlanAction>) {
        let providers = Arc::clone(&self.providers);

        let _ = cx.spawn(
            async move {
                models::fetch_all_models(&providers).await;
                Ok(())
            },
            |result, view, cx| {
                if result.is_err() {
                    return;
                }

                let model_id = view.controller.model_name();
                let max_context = models::all_models(&view.providers)
                    .into_iter()
                    .find(|model| model.id == model_id)
                    .and_then(|model| model.context_length);

                view.controller.set_max_context(max_context);
                cx.notify();
            },
        );
    }

    fn subscribe_permissions(&mut self, cx: &mut Context<'_, Self, AlanAction>) {
        self.permission_subscription = Some(cx.subscribe_stream(
            self.permission_handler.subscribe(),
            |event, view, cx| match event {
                SubscriptionEvent::Item(request) => {
                    view.pending_permission = Some(request.clone());

                    if let Some(prompt) = view.permission {
                        cx.update(prompt, |prompt| prompt.show(request.clone()));
                        cx.focus_entity(prompt);
                    }

                    cx.notify();
                }
                SubscriptionEvent::Closed => {
                    view.permission_subscription = None;
                }
            },
        ));
    }
}

impl Component<AlanAction> for ChatView {
    fn init(&mut self, cx: &mut Context<'_, Self, AlanAction>) {
        let mut editor = PromptEditor::new();
        editor.seed_history(recall_prompts(self.controller.entries()));

        self.chat = Some(cx.insert(ChatHistory::new()));
        self.status = Some(cx.insert(Status::new()));
        self.editor = Some(cx.insert(editor));

        cx.focus_entity(self.editor.expect("editor entity"));

        self.permission = Some(cx.insert(permissions::PermissionPrompt::new()));
        self.subscribe_permissions(cx);
        self.fetch_models(cx);
    }

    fn handle_action(
        &mut self,
        action: &AlanAction,
        cx: &mut Context<'_, Self, AlanAction>,
    ) -> ActionStatus {
        if self.handle_permission(action, cx) {
            return ActionStatus::Handled;
        }

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
                self.handle_quit(cx);
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

        let editor_width = area.width.saturating_sub(theme::PROMPT_GUTTER);
        let editor_rows = cx.read(editor, |e| e.rows(editor_width)).unwrap_or(1);
        let attachment_height = cx.read(editor, |e| e.attachment_height()).unwrap_or(0);
        let steer_text = self.controller.steering().map(str::to_owned);

        let steer_height = if steer_text.is_some() {
            steering::STEER_BAND_HEIGHT
        } else {
            0
        };

        let permission_height = if self.pending_permission.is_some() {
            permissions::PERMISSION_HEIGHT
        } else {
            0
        };

        let [
            chat_area,
            steer_area,
            permission_area,
            attachment_area,
            status_area,
            _gap,
            editor_area,
            _bottom_pad,
        ] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(steer_height),
            Constraint::Length(permission_height),
            Constraint::Length(attachment_height),
            Constraint::Length(STATUS_HEIGHT),
            Constraint::Length(STATUS_EDITOR_GAP),
            Constraint::Length(editor_rows),
            Constraint::Length(EDITOR_BOTTOM_PAD),
        ])
        .areas(area);

        cx.render_with_state(chat, frame, chat_area, &self.controller);

        steering::render_steering(frame, steer_area, steer_text.as_deref());

        if self.pending_permission.is_some()
            && let Some(prompt) = self.permission
        {
            cx.render_entity(prompt, frame, permission_area);
        }

        attachments::render_attachments(frame, attachment_area, editor, cx);

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
            policy: self.policy.policy(),
            usage: self.controller.usage(),
            model_name: self.controller.model_name(),
            max_context: self.controller.max_context(),
            reasoning_effort: self.controller.reasoning_effort(),
        };
        cx.render_with_state(status, frame, status_area, &inputs);
        cx.render_entity(editor, frame, editor_area);
    }
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

async fn persist_tool_policy(policy: crate::core::permissions::Policy) -> anyhow::Result<()> {
    let store = SettingsStore::<Settings>::new(settings::default_settings_path()?);
    let mut settings = store.load().await?.unwrap_or_default();
    settings.tool_policy = Some(policy);

    store.save(&settings).await
}
