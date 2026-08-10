use crate::{LlmError, LlmEvent, LlmRequest, Message, Role, StopReason, Usage};
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
struct Request<'a> {
    model: &'a str,
    stream: bool,
    messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
}

#[derive(Serialize)]
struct WireMessage {
    role: &'static str,
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<WireToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Serialize)]
struct WireToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireFunction,
}

#[derive(Serialize)]
struct WireFunction {
    name: String,
    arguments: String,
}

#[derive(Serialize)]
struct WireTool {
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireDefinition,
}

#[derive(Serialize)]
struct WireDefinition {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Deserialize)]
struct StreamResponse {
    model: Option<String>,
    choices: Vec<StreamChoice>,
    usage: Option<WireUsage>,
}

#[derive(Deserialize)]
struct StreamChoice {
    delta: Option<StreamDelta>,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct StreamDelta {
    content: Option<String>,
    tool_calls: Option<Vec<StreamToolCallWire>>,
}

#[derive(Deserialize)]
struct StreamToolCallWire {
    index: Option<usize>,
    id: Option<String>,
    function: Option<StreamFunctionWire>,
}

#[derive(Deserialize)]
struct StreamFunctionWire {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct WireUsage {
    prompt_tokens: u64,
    completion_tokens: u64,
}

#[derive(Debug)]
pub(crate) struct StreamChunk {
    pub(crate) model: Option<String>,
    pub(crate) text: Option<String>,
    pub(crate) tool_calls: Vec<StreamToolCall>,
    pub(crate) finish_reason: Option<String>,
    pub(crate) usage: Option<Usage>,
}

#[derive(Debug)]
pub(crate) struct StreamToolCall {
    pub(crate) index: usize,
    pub(crate) id: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) arguments: String,
}

pub(crate) fn serialize_request(request: &LlmRequest<'_>) -> Result<String, LlmError> {
    let messages = request.messages.iter().map(wire_message).collect();
    let tools = request.tools.iter().map(wire_tool).collect();
    serde_json::to_string(&Request {
        model: request.model_id,
        stream: true,
        messages,
        tools,
        temperature: request.options.temperature,
        max_tokens: request.options.max_tokens,
    })
    .map_err(LlmError::Serialization)
}

pub(crate) fn deserialize_stream_chunk(body: &str) -> Result<StreamChunk, LlmError> {
    let response: StreamResponse = serde_json::from_str(body).map_err(LlmError::Serialization)?;
    let choice = response.choices.first();
    let delta = choice.and_then(|choice| choice.delta.as_ref());
    let text = delta
        .and_then(|delta| delta.content.clone())
        .filter(|text| !text.is_empty());
    let tool_calls = delta
        .and_then(|delta| delta.tool_calls.as_ref())
        .into_iter()
        .flatten()
        .map(|call| StreamToolCall {
            index: call.index.unwrap_or_default(),
            id: call.id.clone(),
            name: call
                .function
                .as_ref()
                .and_then(|function| function.name.clone()),
            arguments: call
                .function
                .as_ref()
                .and_then(|function| function.arguments.clone())
                .unwrap_or_default(),
        })
        .collect();

    Ok(StreamChunk {
        model: response.model,
        text,
        tool_calls,
        finish_reason: choice.and_then(|choice| choice.finish_reason.clone()),
        usage: response.usage.map(|usage| Usage {
            input_tokens: usage.prompt_tokens,
            output_tokens: usage.completion_tokens,
        }),
    })
}

pub(crate) fn stream_events(chunk: StreamChunk) -> Vec<LlmEvent> {
    let mut events = Vec::new();
    if let Some(text) = chunk.text {
        events.push(LlmEvent::TextDelta { text });
    }
    events.extend(
        chunk
            .tool_calls
            .into_iter()
            .map(|call| LlmEvent::ToolCallDelta {
                index: call.index,
                id: call.id,
                name: call.name,
                arguments: call.arguments,
            }),
    );
    events
}

pub(crate) fn stop_reason_for_finish_reason(reason: Option<&str>) -> Option<StopReason> {
    reason.map(|reason| match reason {
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "length" => StopReason::Length,
        "content_filter" => StopReason::ContentFilter,
        "stop" => StopReason::Stop,
        _ => StopReason::Error,
    })
}

fn wire_message(message: &Message) -> WireMessage {
    WireMessage {
        role: role(message.role),
        content: message.content.clone(),
        tool_calls: message.tool_calls.as_ref().map(|calls| {
            calls
                .iter()
                .map(|call| WireToolCall {
                    id: call.id.clone(),
                    kind: "function",
                    function: WireFunction {
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                    },
                })
                .collect()
        }),
        tool_call_id: message.tool_call_id.clone(),
    }
}

fn wire_tool(tool: &crate::ToolDefinition) -> WireTool {
    WireTool {
        kind: "function",
        function: WireDefinition {
            name: tool.name.clone(),
            description: tool.description.clone(),
            parameters: tool.parameters.clone(),
        },
    }
}

fn role(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RequestOptions, ToolDefinition};

    fn request<'a>(
        model_id: &'a str,
        messages: &'a [Message],
        tools: &'a [ToolDefinition],
        options: &'a RequestOptions,
    ) -> LlmRequest<'a> {
        LlmRequest {
            model_id,
            messages,
            tools,
            options,
            credential: None,
        }
    }

    #[test]
    fn serializes_stream_request_with_options_messages_and_tools() {
        let messages = [Message::system("system"), Message::user("hello")];
        let tools = [ToolDefinition {
            name: "weather".into(),
            description: "Get weather".into(),
            parameters: serde_json::json!({"type": "object"}),
        }];
        let options = RequestOptions {
            temperature: Some(0.2),
            max_tokens: Some(128),
        };
        let body = serialize_request(&request("model-a", &messages, &tools, &options)).unwrap();
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();

        assert_eq!(json["model"], "model-a");
        assert_eq!(json["stream"], true);
        assert_eq!(json["temperature"], 0.2);
        assert_eq!(json["max_tokens"], 128);
        assert_eq!(json["messages"][1]["content"], "hello");
        assert_eq!(json["tools"][0]["function"]["name"], "weather");
    }

    #[test]
    fn omits_empty_tools_and_unset_options() {
        let messages = [Message::user("hello")];
        let options = RequestOptions::default();
        let body = serialize_request(&request("model-a", &messages, &[], &options)).unwrap();
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();

        assert!(json.get("tools").is_none());
        assert!(json.get("temperature").is_none());
        assert!(json.get("max_tokens").is_none());
    }

    #[test]
    fn decodes_text_tool_calls_finish_reason_and_usage() {
        let body = r#"{
            "model": "served-model",
            "choices": [{
                "finish_reason": "tool_calls",
                "delta": {
                    "content": "Checking",
                    "tool_calls": [{
                        "index": 0,
                        "id": "call-1",
                        "function": {"name": "weather", "arguments": "{\"city\":"}
                    }]
                }
            }],
            "usage": {"prompt_tokens": 11, "completion_tokens": 7}
        }"#;

        let chunk = deserialize_stream_chunk(body).unwrap();
        assert_eq!(chunk.model.as_deref(), Some("served-model"));
        assert_eq!(chunk.text.as_deref(), Some("Checking"));
        assert_eq!(chunk.tool_calls[0].name.as_deref(), Some("weather"));
        assert_eq!(chunk.tool_calls[0].arguments, "{\"city\":");
        assert_eq!(chunk.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(chunk.usage.unwrap().output_tokens, 7);
    }

    #[test]
    fn accepts_usage_only_chunk() {
        let chunk = deserialize_stream_chunk(
            r#"{"choices":[],"usage":{"prompt_tokens":2,"completion_tokens":3}}"#,
        )
        .unwrap();
        assert!(chunk.text.is_none());
        assert_eq!(chunk.usage.unwrap().input_tokens, 2);
    }

    #[test]
    fn maps_finish_reasons() {
        for (reason, expected) in [
            ("stop", StopReason::Stop),
            ("length", StopReason::Length),
            ("content_filter", StopReason::ContentFilter),
            ("tool_calls", StopReason::ToolUse),
        ] {
            assert_eq!(stop_reason_for_finish_reason(Some(reason)), Some(expected));
        }
    }
}
