use crate::AgentError;
use crate::AgentMessage;
use crate::context::AgentContext;
use llm::LlmResponse;
use providers::Model;
use tools::ToolOutput;

use super::Agent;
use super::event::{AgentEvent, emit_event};
use super::prompt::PromptCx;

/// Core agent loop: stream LLM responses and execute tool calls until the
/// model produces a final answer or `max_tool_rounds` is reached.
pub(super) async fn run_with(
    agent: &Agent,
    model: &Model,
    context: &mut AgentContext,
    cx: &mut PromptCx<'_>,
) -> Result<LlmResponse, AgentError> {
    let plan = agent.plan_mode();

    for _ in 0..agent.max_tool_rounds {
        cx.check_cancelled()?;

        let session_id = agent.session_id.lock().await.clone();
        let response = super::prompt::stream_round(session_id, model, context, cx, plan).await?;

        // Accumulate token usage across rounds.
        if let Some(usage) = response.usage.as_ref() {
            context.usage.accumulate(usage);
            super::persistence::persist_usage(agent, &context.usage).await?;
        }

        let calls: Vec<_> = response.tool_calls().cloned().collect();

        if calls.is_empty() {
            return finish_with_response(agent, context, response, cx).await;
        }

        // Persist the assistant message that declares the tool calls.
        super::persistence::append_context_message(
            agent,
            context,
            AgentMessage::Assistant(response),
        )
        .await?;

        handle_tool_calls(agent, calls, context, cx, plan).await?;
    }

    Err(AgentError::MaxToolRounds)
}

/// Emit the `Finished` event and return the final response.
async fn finish_with_response(
    agent: &Agent,
    context: &mut AgentContext,
    response: LlmResponse,
    cx: &mut PromptCx<'_>,
) -> Result<LlmResponse, AgentError> {
    super::persistence::append_context_message(
        agent,
        context,
        AgentMessage::Assistant(response.clone()),
    )
    .await?;

    emit_event(
        cx.events,
        AgentEvent::Finished {
            usage: context.usage.clone(),
            response: Box::new(response.clone()),
        },
        cx.cancellation,
    )
    .await?;

    Ok(response)
}

/// Execute a batch of tool calls, appending each result to the context
/// and emitting start/finish/fail events.
async fn handle_tool_calls(
    agent: &Agent,
    calls: Vec<llm::ToolCall>,
    context: &mut AgentContext,
    cx: &mut PromptCx<'_>,
    plan: bool,
) -> Result<(), AgentError> {
    for call in calls {
        cx.check_cancelled()?;

        let tool_index = context
            .tool_indexes
            .get(&call.name)
            .copied()
            .ok_or_else(|| AgentError::ToolNotFound(call.name.clone()))?;

        // In plan mode only read-only tools (and bash) may be invoked.
        if plan && !context.tools[tool_index].read_only && call.name != "bash" {
            return Err(AgentError::ToolNotFound(call.name.clone()));
        }

        let call_id = call.id.clone();

        emit_event(
            cx.events,
            AgentEvent::ToolCallStarted {
                id: call_id.clone(),
                name: call.name.clone(),
                arguments: call.arguments.clone(),
            },
            cx.cancellation,
        )
        .await?;

        match context.tools[tool_index].executor.execute(&call).await {
            Ok(output) => {
                let (output_text, content_parts) = match output {
                    ToolOutput::Text(text) => (text, vec![]),
                    ToolOutput::Image { mime_type, data } => {
                        let description =
                            format!("[{mime_type} image, {} bytes base64]", data.len());
                        let data_uri = format!("data:{mime_type};base64,{data}");
                        (
                            description,
                            vec![llm::ContentPart::Image {
                                image_url: llm::ImageUrl { url: data_uri },
                            }],
                        )
                    }
                };
                super::persistence::append_context_message(
                    agent,
                    context,
                    AgentMessage::tool_result_with_parts(
                        &call_id,
                        &output_text,
                        content_parts,
                    ),
                )
                .await?;

                emit_event(
                    cx.events,
                    AgentEvent::ToolCallFinished {
                        id: call_id,
                        output: tail_lines(&output_text, 5),
                    },
                    cx.cancellation,
                )
                .await?;
            }
            Err(error) => {
                let error = error.to_string();
                super::persistence::append_context_message(
                    agent,
                    context,
                    AgentMessage::ToolResult {
                        tool_call_id: call_id.clone(),
                        content: error.clone(),
                        content_parts: vec![],
                    },
                )
                .await?;

                emit_event(
                    cx.events,
                    AgentEvent::ToolCallFailed { id: call_id, error },
                    cx.cancellation,
                )
                .await?;
            }
        }

        cx.check_cancelled()?;
    }

    Ok(())
}

fn tail_lines(output: &str, count: usize) -> String {
    let mut lines: Vec<_> = output.lines().rev().take(count).collect();
    lines.reverse();
    lines.join("\n")
}
