use super::event::{AgentEvent, emit_event};
use super::{Agent, Mode};
use crate::AgentMessage;
use crate::agent::persistence;
use crate::context::AgentContext;
use crate::{AgentError, AgentStream};
use futures_util::StreamExt;
use llm::{
    CompletionInput, LlmEvent, LlmResponse, LlmResponseBuilder, Message, PromptCacheControl,
    RequestOptions, ToolSpec, Usage,
};
use providers::{Model, ModelError};
use tokio::sync::{mpsc::Sender, watch};

/// Mutable state threaded through the entire prompt lifecycle.
pub(super) struct PromptCx<'a> {
    pub events: Option<&'a Sender<Result<AgentEvent, AgentError>>>,
    pub cancellation: &'a mut watch::Receiver<bool>,
    pub partial: &'a mut String,
    pub stream: bool,
    /// Latest usage snapshot observed in the current provider round.
    pub round_usage: Option<Usage>,
    /// Whether the current round's usage has already been added to context.
    pub round_usage_applied: bool,
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
        let mode = agent.mode();
        let review_intro = mode == Mode::Review && agent.take_review_intro();
        let user_msg = build_user_message(content, images, mode, review_intro);
        let mut partial = String::new();
        let mut cx = PromptCx {
            events: Some(&tx),
            cancellation: &mut cancellation_receiver,
            partial: &mut partial,
            stream,
            round_usage: None,
            round_usage_applied: false,
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

/// Append the "Current project dir" line to the first user message of the
/// conversation so the model knows where it is operating.
fn attach_working_directory(
    agent: &Agent,
    context: &AgentContext,
    mut user_msg: AgentMessage,
) -> AgentMessage {
    let Some(dir) = agent.working_directory.as_ref() else {
        return user_msg;
    };
    if !context.messages.is_empty() {
        return user_msg;
    }
    if let AgentMessage::User { text, .. } = &mut user_msg {
        text.push_str(&format!(
            "\nCurrent project dir which you are in : {}",
            dir.display()
        ));
    }
    user_msg
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

    let user_msg = attach_working_directory(agent, &context, user_msg);

    persistence::ensure_session(agent, &model).await?;
    persistence::append_context_message(agent, &mut context, user_msg).await?;

    let result = super::tool_loop::run_with(agent, &model, &mut context, cx).await;

    if result.is_err()
        && let Some(usage) = cx.round_usage.as_ref()
        && !cx.round_usage_applied
    {
        context.usage.accumulate(usage);
        persistence::persist_usage(agent, &context.usage).await?;
    }

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
    context: &mut AgentContext,
    cx: &mut PromptCx<'_>,
    mode: Mode,
) -> Result<(LlmResponse, Option<Usage>), AgentError> {
    let tools: Vec<_> = context
        .tools
        .iter()
        .filter(|tool| mode == Mode::Normal || tool.read_only || tool.definition.name == "bash")
        .map(|tool| ToolSpec::Function(tool.definition.clone()))
        .collect();

    let messages = build_messages(context);
    let options = RequestOptions {
        prompt_cache_key: Some(session_id.clone()),
        session_id: Some(session_id),
        cache_control: Some(PromptCacheControl::one_hour()),
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
    cx.round_usage = None;
    cx.round_usage_applied = false;
    // Usage is a provider snapshot. Keep the latest snapshot for this round;
    // providers may send it more than once (for example on a finish chunk and
    // in a separate usage-only chunk).
    let mut round_usage = None;

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

        if let LlmEvent::Usage { usage } = &event {
            round_usage = Some(usage.clone());
            cx.round_usage = round_usage.clone();

            if cx.stream {
                emit_event(
                    cx.events,
                    AgentEvent::Usage {
                        usage: aggregate_usage(&context.usage, usage),
                    },
                    cx.cancellation,
                )
                .await?;
            }
        }

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

    Ok((builder.finish().map_err(ModelError::from)?, round_usage))
}

fn aggregate_usage(current: &Usage, round: &Usage) -> Usage {
    let mut aggregate = current.clone();
    aggregate.accumulate(round);
    aggregate
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

/// Build a user message, applying the plan-mode suffix, the review-mode
/// guidelines, and optional images.
pub(super) fn build_user_message(
    content: String,
    images: Vec<llm::ImageUrl>,
    mode: Mode,
    review_intro: bool,
) -> AgentMessage {
    let text = prompt_content(content, mode, review_intro);
    if images.is_empty() {
        AgentMessage::user(text)
    } else {
        AgentMessage::user_with_images(text, images)
    }
}

fn prompt_content(content: impl Into<String>, mode: Mode, review_intro: bool) -> String {
    let content = content.into();
    let mut text = match mode {
        Mode::Plan => format!("{content}\n\n{}", plan_suffix()),
        _ => content,
    };
    if review_intro {
        text.push_str(&format!("\n\n{}", review_guidelines()));
    }
    text
}

fn plan_suffix() -> &'static str {
    "Plan mode is on, do not edit any files."
}

/// Guidelines attached to the first user message after review mode is
/// entered. Appended after the plan suffix when both are active.
fn review_guidelines() -> &'static str {
    "Review mode is on, do not edit any files.

Review the code in this project against:
1. Performance
2. SOLID, DRY, KISS
3. Language best practices
4. Edge cases
5. Security: input validation, secrets handling, injection, unsafe deserialization
6. Correctness: error handling, resource cleanup, concurrency and race conditions
7. Tests: coverage of the change, and do they fail when the code breaks
8. Readability: naming, function size, comments explain why not what
9. If something can be done in a better way, describe the best approach in two or three lines
10. Future scoping"
}

/// Validate that a prompt carries content: either non-empty text or at
/// least one attached image.
pub(super) fn validate_prompt(text: &str, images: &[llm::ImageUrl]) -> Result<(), AgentError> {
    if text.trim().is_empty() && images.is_empty() {
        return Err(AgentError::Model(ModelError::Llm(
            llm::LlmError::Configuration("empty prompt".into()),
        )));
    }
    Ok(())
}

/// Validate a user message (used by the streaming path after plan-mode suffix).
fn validate_prompt_message(message: &AgentMessage) -> Result<(), AgentError> {
    if let AgentMessage::User { text, images } = message {
        validate_prompt(text, images)?;
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
