use crate::codec::{CodecChunk, LlmApiCodec};
use crate::{LlmError, LlmEvent, LlmRequest, Message, Role, StopReason, ToolSpec, Usage};
use serde::{Deserialize, Serialize};

/// Codec for the canonical OpenAI-compatible `/chat/completions` protocol.
pub struct DefaultChatCompletionsCodec;

impl LlmApiCodec for DefaultChatCompletionsCodec {
    fn request(&self, request: &LlmRequest<'_>) -> Result<String, LlmError> {
        serde_json::to_string(&BaseRequest::new(request)).map_err(LlmError::Serialization)
    }

    fn response(&self, data: &str) -> Result<CodecChunk, LlmError> {
        let response: BaseResponse = serde_json::from_str(data).map_err(LlmError::Serialization)?;
        decode_stream_response(&response)
    }
}

/// Canonical `/chat/completions` request body. Providers with extras embed it
/// via `#[serde(flatten)]` and add their own fields.
#[derive(Serialize)]
pub struct BaseRequest<'a> {
    pub model: &'a str,
    pub stream: bool,
    pub messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<WireTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    pub reasoning: WireReasoning,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<&'a crate::PromptCacheControl>,
}

impl<'a> BaseRequest<'a> {
    pub fn new(request: &'a LlmRequest<'a>) -> Self {
        Self {
            model: request.model_id,
            stream: true,
            messages: request.messages.iter().map(wire_message).collect(),
            tools: request.tools.iter().map(wire_tool).collect(),
            temperature: request.options.temperature,
            max_tokens: request.options.max_tokens,
            reasoning: WireReasoning {
                effort: request.reasoning_effort.to_string(),
            },
            prompt_cache_key: request
                .extensions
                .get::<crate::SessionId>()
                .map(|session| session.0.as_str()),
            cache_control: request.options.cache_control.as_ref(),
        }
    }
}

/// Canonical `/chat/completions` stream chunk. Providers with extra fields
/// embed it via `#[serde(flatten)]` and patch the decoded result.
#[derive(Deserialize)]
pub struct BaseResponse {
    pub model: Option<String>,
    pub choices: Vec<StreamChoice>,
    pub usage: Option<WireUsage>,
}

/// Decode a canonical [`BaseResponse`] into the API-facing [`CodecChunk`].
/// Exposed so provider codecs can reuse the canonical mapping after
/// flattening [`BaseResponse`] into their own wire chunk.
pub fn decode_stream_response(response: &BaseResponse) -> Result<CodecChunk, LlmError> {
    let chunk = stream_chunk_from_response(response)?;
    let events = stream_events(&chunk);
    Ok(CodecChunk {
        model: chunk.model,
        finish_reason: chunk.finish_reason,
        usage: chunk.usage,
        events,
    })
}

#[derive(Serialize)]
pub struct WireMessage {
    pub role: &'static str,
    pub content: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<WireToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Serialize)]
pub struct WireToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub function: WireFunction,
}

#[derive(Serialize)]
pub struct WireFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Serialize)]
pub struct WireTool {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function: Option<WireDefinition>,
}

#[derive(Serialize)]
pub struct WireDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Serialize)]
pub struct WireReasoning {
    pub effort: String,
}

#[derive(Deserialize)]
pub struct StreamChoice {
    pub delta: Option<StreamDelta>,
    pub finish_reason: Option<String>,
}

#[derive(Deserialize)]
pub struct StreamDelta {
    pub content: Option<String>,
    pub tool_calls: Option<Vec<StreamToolCallWire>>,
    pub reasoning: Option<String>,
    pub reasoning_content: Option<String>,
    pub reasoning_details: Option<Vec<serde_json::Value>>,
}

#[derive(Deserialize)]
pub struct StreamToolCallWire {
    pub index: Option<usize>,
    pub id: Option<String>,
    pub function: Option<StreamFunctionWire>,
}

#[derive(Deserialize)]
pub struct StreamFunctionWire {
    pub name: Option<String>,
    pub arguments: Option<String>,
}

/// Canonical usage fields; the OpenAI-standard token accounting.
#[derive(Deserialize, Default)]
#[serde(default)]
pub struct WireUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: Option<u64>,
    pub prompt_tokens_details: Option<WirePromptTokensDetails>,
    pub completion_tokens_details: Option<WireCompletionTokensDetails>,
}

#[derive(Deserialize, Clone, Default)]
#[serde(default)]
pub struct WirePromptTokensDetails {
    pub cached_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub audio_tokens: Option<u64>,
}

#[derive(Deserialize, Clone, Default)]
#[serde(default)]
pub struct WireCompletionTokensDetails {
    pub reasoning_tokens: Option<u64>,
}

/// Map canonical [`WireUsage`] onto [`Usage`].
pub fn usage_from_wire(wire: &WireUsage) -> Usage {
    Usage {
        input_tokens: wire.prompt_tokens,
        output_tokens: wire.completion_tokens,
        total_tokens: wire.total_tokens,
        cost: None,
        upstream_inference_cost: None,
        cached_tokens: wire
            .prompt_tokens_details
            .as_ref()
            .and_then(|d| d.cached_tokens),
        cache_write_tokens: wire
            .prompt_tokens_details
            .as_ref()
            .and_then(|d| d.cache_write_tokens),
        reasoning_tokens: wire
            .completion_tokens_details
            .as_ref()
            .and_then(|d| d.reasoning_tokens),
        audio_tokens: wire
            .prompt_tokens_details
            .as_ref()
            .and_then(|d| d.audio_tokens),
    }
}

/// Map a canonical finish reason onto [`StopReason`].
pub fn stop_reason_for_finish_reason(reason: Option<&str>) -> Option<StopReason> {
    reason.map(|reason| match reason {
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "length" => StopReason::Length,
        "content_filter" => StopReason::ContentFilter,
        "stop" => StopReason::Stop,
        _ => StopReason::Error,
    })
}

/// Decode a canonical [`BaseResponse`] into the internal stream chunk.
pub(crate) fn stream_chunk_from_response(response: &BaseResponse) -> Result<StreamChunk, LlmError> {
    let choice = response.choices.first();
    let delta = choice.and_then(|choice| choice.delta.as_ref());
    let text = delta
        .and_then(|delta| delta.content.clone())
        .filter(|text| !text.is_empty());
    let tool_calls = delta
        .and_then(|delta| delta.tool_calls.as_ref())
        .into_iter()
        .flatten()
        .map(|call| {
            Ok(StreamToolCall {
                index: call.index.ok_or_else(|| {
                    LlmError::InvalidResponse("stream tool call is missing index".into())
                })?,
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
        })
        .collect::<Result<Vec<_>, LlmError>>()?;

    let reasoning = delta.and_then(|delta| {
        delta
            .reasoning
            .clone()
            .or_else(|| delta.reasoning_content.clone())
            .filter(|reasoning| !reasoning.is_empty())
    });
    let reasoning_details = delta
        .and_then(|delta| delta.reasoning_details.clone())
        .unwrap_or_default();

    Ok(StreamChunk {
        model: response.model.clone(),
        text,
        reasoning,
        reasoning_details,
        tool_calls,
        finish_reason: choice.and_then(|choice| choice.finish_reason.clone()),
        usage: response.usage.as_ref().map(usage_from_wire),
    })
}

#[derive(Debug)]
pub(crate) struct StreamChunk {
    pub(crate) model: Option<String>,
    pub(crate) text: Option<String>,
    pub(crate) reasoning: Option<String>,
    pub(crate) reasoning_details: Vec<serde_json::Value>,
    pub(crate) tool_calls: Vec<StreamToolCall>,
    pub(crate) finish_reason: Option<String>,
    pub(crate) usage: Option<Usage>,
}

#[derive(Debug, Clone)]
pub(crate) struct StreamToolCall {
    pub(crate) index: usize,
    pub(crate) id: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) arguments: String,
}

pub(crate) fn stream_events(chunk: &StreamChunk) -> Vec<LlmEvent> {
    let mut events = Vec::new();
    let details = chunk.reasoning_details.clone();
    if let Some(reasoning) = chunk.reasoning.clone() {
        let details = if details.is_empty() {
            vec![serde_json::json!({
                "type": "reasoning.text",
                "text": reasoning.clone(),
            })]
        } else {
            details
        };
        events.push(LlmEvent::ReasoningDelta { reasoning, details });
    } else if !details.is_empty() {
        let reasoning = reasoning_text(&details);
        if !reasoning.is_empty() {
            events.push(LlmEvent::ReasoningDelta { reasoning, details });
        }
    }
    if let Some(text) = chunk.text.clone() {
        events.push(LlmEvent::TextDelta { text });
    }
    events.extend(
        chunk
            .tool_calls
            .clone()
            .into_iter()
            .map(|call| LlmEvent::ToolCallDelta {
                index: call.index,
                id: call.id,
                name: call.name,
                arguments: call.arguments,
                signature: None,
            }),
    );
    events
}

fn reasoning_text(details: &[serde_json::Value]) -> String {
    details
        .iter()
        .filter_map(|detail| {
            detail
                .get("text")
                .and_then(serde_json::Value::as_str)
                .or_else(|| detail.get("summary").and_then(serde_json::Value::as_str))
        })
        .collect()
}

fn wire_message(message: &Message) -> WireMessage {
    let content = if let Some(parts) = &message.content_parts {
        Some(serde_json::to_value(parts).expect("content parts must serialize"))
    } else {
        message
            .content
            .as_ref()
            .map(|s| serde_json::Value::String(s.clone()))
    };
    WireMessage {
        role: role(message.role),
        content,
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

fn wire_tool(tool: &ToolSpec) -> WireTool {
    match tool {
        ToolSpec::Function(tool) => WireTool {
            kind: "function".into(),
            function: Some(WireDefinition {
                name: tool.name.clone(),
                description: tool.description.clone(),
                parameters: tool.parameters.clone(),
            }),
        },
        ToolSpec::Server(tool) => WireTool {
            kind: tool.kind.clone(),
            function: None,
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

    fn codec() -> super::DefaultChatCompletionsCodec {
        super::DefaultChatCompletionsCodec
    }
    use crate::{RequestOptions, ToolDefinition, ToolSpec};

    fn request<'a>(
        model_id: &'a str,
        messages: &'a [Message],
        tools: &'a [ToolSpec],
        options: &'a RequestOptions,
    ) -> LlmRequest<'a> {
        LlmRequest {
            model_id,
            messages,
            tools,
            options,
            credential: None,
            reasoning_effort: crate::ReasoningEffort::None,
            extensions: crate::Extensions::default(),
        }
    }

    #[test]
    fn serializes_stream_request_with_options_messages_and_tools() {
        let messages = [Message::system("system"), Message::user("hello")];
        let tools = [ToolSpec::Function(ToolDefinition {
            name: "weather".into(),
            description: "Get weather".into(),
            parameters: serde_json::json!({"type": "object"}),
        })];
        let options = RequestOptions {
            temperature: Some(0.2),
            max_tokens: Some(128),
            ..RequestOptions::default()
        };
        let body = codec()
            .request(&request("model-a", &messages, &tools, &options))
            .unwrap();
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();

        assert_eq!(json["model"], "model-a");
        assert_eq!(json["stream"], true);
        assert_eq!(json["temperature"], 0.2);
        assert_eq!(json["max_tokens"], 128);
        assert_eq!(json["messages"][1]["content"], "hello");
        assert_eq!(json["tools"][0]["function"]["name"], "weather");
    }

    #[test]
    fn serializes_reasoning_effort_and_omits_reasoning_history() {
        let messages = [Message::assistant_with_tool_calls_and_reasoning(
            None,
            vec![crate::ToolCall {
                id: "call-1".into(),
                name: "weather".into(),
                arguments: "{}".into(),
                signature: None,
            }],
            Some("think".into()),
            vec![serde_json::json!({"type": "reasoning.text", "text": "think"})],
        )];
        let options = RequestOptions::default();
        let mut request = request("model-a", &messages, &[], &options);
        request.reasoning_effort = crate::ReasoningEffort::High;
        let json: serde_json::Value =
            serde_json::from_str(&codec().request(&request).unwrap()).unwrap();
        assert_eq!(json["reasoning"]["effort"], "high");
        assert!(
            json["messages"][0]
                .as_object()
                .unwrap()
                .keys()
                .all(|key| key != "reasoning_details" && key != "reasoning_content"),
            "reasoning history must not be re-sent"
        );
    }

    #[test]
    fn omits_empty_tools_and_unset_options() {
        let messages = [Message::user("hello")];
        let options = RequestOptions {
            temperature: None,
            max_tokens: None,
            ..RequestOptions::default()
        };
        let body = codec()
            .request(&request("model-a", &messages, &[], &options))
            .unwrap();
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

        let chunk = codec().response(body).unwrap();
        assert_eq!(chunk.model.as_deref(), Some("served-model"));
        assert_eq!(
            chunk.events,
            vec![
                LlmEvent::TextDelta {
                    text: "Checking".into()
                },
                LlmEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("weather".into()),
                    arguments: "{\"city\":".into(),
                    signature: None,
                },
            ]
        );
        assert_eq!(chunk.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(chunk.usage.unwrap().output_tokens, 7);
    }

    #[test]
    fn rejects_tool_call_without_index() {
        let error = codec().response(
            r#"{"choices":[{"delta":{"tool_calls":[{"id":"call-1","function":{"name":"bash"}}]}}]}"#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("missing index"));
    }

    #[test]
    fn accepts_usage_only_chunk() {
        let chunk = codec()
            .response(r#"{"choices":[],"usage":{"prompt_tokens":2,"completion_tokens":3}}"#)
            .unwrap();
        assert!(chunk.events.is_empty());
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

    #[test]
    fn serializes_image_url_with_direct_url() {
        use crate::{ContentPart, ImageUrl};

        let messages = [Message::user_with_parts(vec![
            ContentPart::Text {
                text: "What's in this image?".into(),
            },
            ContentPart::Image {
                image_url: ImageUrl {
                    url: "https://example.com/photo.jpg".into(),
                },
            },
        ])];
        let options = RequestOptions::default();
        let body = codec()
            .request(&request("model-a", &messages, &[], &options))
            .unwrap();
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();

        let content = &json["messages"][0]["content"];
        assert!(content.is_array(), "content must be an array of parts");
        let parts = content.as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "What's in this image?");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(
            parts[1]["image_url"]["url"],
            "https://example.com/photo.jpg"
        );
    }

    #[test]
    fn serializes_image_url_with_base64_data_uri() {
        use crate::{ContentPart, ImageUrl};

        let data_uri = "data:image/jpeg;base64,/9j/4AAQSkZJRg==";
        let messages = [Message::user_with_parts(vec![
            ContentPart::Text {
                text: "Describe this local image".into(),
            },
            ContentPart::Image {
                image_url: ImageUrl {
                    url: data_uri.into(),
                },
            },
        ])];
        let options = RequestOptions::default();
        let body = codec()
            .request(&request("model-a", &messages, &[], &options))
            .unwrap();
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();

        let content = &json["messages"][0]["content"];
        let parts = content.as_array().unwrap();
        assert_eq!(parts[1]["image_url"]["url"], data_uri);
    }

    #[test]
    fn serializes_text_only_content_parts_as_array() {
        use crate::ContentPart;

        let messages = [Message::user_with_parts(vec![ContentPart::Text {
            text: "hello".into(),
        }])];
        let options = RequestOptions::default();
        let body = codec()
            .request(&request("model-a", &messages, &[], &options))
            .unwrap();
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();

        let content = &json["messages"][0]["content"];
        assert!(content.is_array());
        let parts = content.as_array().unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "hello");
    }
}
