use super::Agent;
use super::event::{AgentEvent, emit_event};
use crate::AgentMessage;
use crate::context::AgentContext;
use crate::{AgentError, AgentStream};
use futures_util::StreamExt;
use llm::{
    CompletionInput, LlmEvent, LlmResponse, LlmResponseBuilder, Message, RequestOptions, ToolSpec,
};
use providers::{Model, ModelError};
use tokio::sync::{mpsc::Sender, watch};

/// Mutable state threaded through the entire prompt lifecycle.
pub(super) struct PromptCx<'a> {
    pub events: Option<&'a Sender<Result<AgentEvent, AgentError>>>,
    pub cancellation: &'a mut watch::Receiver<bool>,
    pub partial: &'a mut String,
    pub stream: bool,
}

impl<'a> PromptCx<'a> {
    /// Return `Err(Aborted)` if cancellation has been signalled.
    pub(super) fn check_cancelled(&self) -> Result<(), AgentError> {
        if *self.cancellation.borrow() {
            Err(AgentError::Aborted)
        } else {
            Ok(())
        }
    }
}

pub(super) fn spawn_prompt_task(
    agent: &std::sync::Arc<Agent>,
    content: String,
    images: Vec<llm::ImageUrl>,
    stream: bool,
) -> AgentStream {
    let (tx, receiver) = tokio::sync::mpsc::channel(super::AGENT_EVENT_CAPACITY);
    let (cancellation, mut cancellation_receiver) = watch::channel(false);
    let agent = agent.clone();

    tokio::spawn(async move {
        let user_msg = build_user_message(content, images, agent.plan_mode());
        let mut partial = String::new();
        let mut cx = PromptCx {
            events: Some(&tx),
            cancellation: &mut cancellation_receiver,
            partial: &mut partial,
            stream,
        };
        let result = run_prompt_with_message(&agent, user_msg, &mut cx).await;
        if let Err(error) = result {
            let _ = tx.send(Err(error)).await;
        }
    });

    AgentStream {
        receiver,
        cancellation,
    }
}

/// Run the full prompt lifecycle inside the spawned background task.
async fn run_prompt_with_message(
    agent: &Agent,
    user_msg: AgentMessage,
    cx: &mut PromptCx<'_>,
) -> Result<LlmResponse, AgentError> {
    validate_prompt_message(&user_msg)?;

    let model = agent.model.lock().await;
    let mut context = agent.context.lock().await;

    super::persistence::ensure_session(agent, &model).await?;
    super::persistence::append_context_message(agent, &mut context, user_msg).await?;

    let result = super::tool_loop::run_with(agent, &model, &mut context, cx).await;

    save_partial_on_error(agent, &mut context, &result, cx.partial).await?;
    result
}

/// If the prompt failed but produced partial output, persist it so the
/// conversation history remains coherent.
async fn save_partial_on_error(
    agent: &Agent,
    context: &mut AgentContext,
    result: &Result<LlmResponse, AgentError>,
    partial: &str,
) -> Result<(), AgentError> {
    if result.is_err() && !partial.is_empty() {
        super::persistence::append_context_message(
            agent,
            context,
            AgentMessage::Assistant(partial_response(partial)),
        )
        .await?;
    }
    Ok(())
}

/// Stream a single LLM round: send messages, receive response events,
/// and build the final [`LlmResponse`].
pub(super) async fn stream_round(
    session_id: String,
    model: &Model,
    context: &AgentContext,
    cx: &mut PromptCx<'_>,
    plan: bool,
) -> Result<LlmResponse, AgentError> {
    let tools: Vec<_> = context
        .tools
        .iter()
        .filter(|tool| !plan || tool.read_only || tool.definition.name == "bash")
        .map(|tool| ToolSpec::Function(tool.definition.clone()))
        .collect();

    let messages = build_messages(context);
    let options = RequestOptions {
        prompt_cache_key: Some(session_id.clone()),
        session_id: Some(session_id),
        ..RequestOptions::default()
    };

    let mut stream_resp = model
        .stream(CompletionInput {
            messages: &messages,
            tools: &tools,
            options: &options,
        })
        .await?;

    let mut builder = LlmResponseBuilder::new();
    cx.partial.clear();

    loop {
        let next = tokio::select! {
            result = stream_resp.next() => result,
            changed = cx.cancellation.changed() => {
                changed.map_err(|_| AgentError::Aborted)?;
                if *cx.cancellation.borrow() {
                    return Err(AgentError::Aborted);
                }
                continue;
            }
        };

        let Some(event) = next else { break };
        let event = event.map_err(ModelError::from)?;
        builder.apply(&event).map_err(ModelError::from)?;

        if cx.stream {
            match &event {
                LlmEvent::ReasoningDelta { reasoning, .. } => {
                    emit_event(
                        cx.events,
                        AgentEvent::ReasoningDelta(reasoning.clone()),
                        cx.cancellation,
                    )
                    .await?;
                }
                LlmEvent::TextDelta { text } => {
                    cx.partial.push_str(text);
                    emit_event(
                        cx.events,
                        AgentEvent::TextDelta(text.clone()),
                        cx.cancellation,
                    )
                    .await?;
                }
                _ => {}
            }
        } else if let LlmEvent::TextDelta { text } = &event {
            // Even in non-streaming mode we must track partial output for
            // error recovery, but we suppress the event itself.
            cx.partial.push_str(text);
        }
    }

    Ok(builder.finish().map_err(ModelError::from)?)
}

fn build_messages(context: &AgentContext) -> Vec<Message> {
    let system = crate::build_system_prompt(context.system_prompt.as_deref(), &context.skills);
    let mut messages = Vec::with_capacity(context.messages.len() + usize::from(system.is_some()));
    if let Some(system) = system {
        messages.push(Message::system(system));
    }
    messages.extend(context.messages.iter().map(AgentMessage::to_llm));
    messages
}

/// Build a user message, applying the plan-mode suffix and optional images.
pub(super) fn build_user_message(
    content: String,
    images: Vec<llm::ImageUrl>,
    plan_mode: bool,
) -> AgentMessage {
    let text = prompt_content(content, plan_mode);
    if images.is_empty() {
        AgentMessage::user(text)
    } else {
        AgentMessage::user_with_images(text, images)
    }
}

fn prompt_content(content: impl Into<String>, plan_mode: bool) -> String {
    let content = content.into();
    if plan_mode {
        format!("{content}\n\nPlan mode is on, do not edit any files.")
    } else {
        content
    }
}

/// Validate that a user-facing prompt is non-empty.
pub(super) fn validate_not_empty(text: &str) -> Result<(), AgentError> {
    if text.trim().is_empty() {
        return Err(AgentError::Model(ModelError::Llm(
            llm::LlmError::Configuration("empty prompt".into()),
        )));
    }
    Ok(())
}

/// Validate a user message (used by the streaming path after plan-mode suffix).
fn validate_prompt_message(message: &AgentMessage) -> Result<(), AgentError> {
    if let AgentMessage::User { text, .. } = message {
        validate_not_empty(text)?;
    }
    Ok(())
}

pub(super) fn partial_response(text: &str) -> LlmResponse {
    LlmResponse {
        content: vec![llm::ContentBlock::Text(text.to_owned())],
        stop_reason: llm::StopReason::Aborted,
        usage: None,
        model: None,
        reasoning: None,
        reasoning_details: Vec::new(),
    }
}
