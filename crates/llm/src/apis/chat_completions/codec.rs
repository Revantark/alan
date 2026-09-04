use crate::{
    LlmError, LlmEvent, LlmRequest, Message, ReasoningEffort, Role, StopReason, ToolSpec, Usage,
};
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
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<WireReasoning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<&'a crate::PromptCacheControl>,
}

#[derive(Serialize)]
struct WireMessage {
    role: &'static str,
    content: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<WireToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_details: Option<Vec<serde_json::Value>>,
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
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    function: Option<WireDefinition>,
}

#[derive(Serialize)]
struct WireDefinition {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Serialize)]
struct WireReasoning {
    effort: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    exclude: Option<bool>,
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
    reasoning: Option<String>,
    reasoning_content: Option<String>,
    reasoning_details: Option<Vec<serde_json::Value>>,
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

#[derive(Deserialize, Default)]
#[serde(default)]
struct WireUsage {
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: Option<u64>,
    cost: Option<f64>,
    cost_details: Option<WireCostDetails>,
    prompt_tokens_details: Option<WirePromptTokensDetails>,
    completion_tokens_details: Option<WireCompletionTokensDetails>,
}

#[derive(Deserialize, Clone, Default)]
#[serde(default)]
struct WireCostDetails {
    upstream_inference_cost: Option<f64>,
}

#[derive(Deserialize, Clone, Default)]
#[serde(default)]
struct WirePromptTokensDetails {
    cached_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,
    audio_tokens: Option<u64>,
}

#[derive(Deserialize, Clone, Default)]
#[serde(default)]
struct WireCompletionTokensDetails {
    reasoning_tokens: Option<u64>,
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
    // How `Auto` reaches the provider is this protocol's business
    let reasoning = match request.reasoning_effort {
        ReasoningEffort::Auto => None,
        effort => Some(WireReasoning {
            effort: effort.as_str().to_string(),
            exclude: None,
        }),
    };
    serde_json::to_string(&Request {
        model: request.model_id,
        stream: true,
        messages,
        tools,
        temperature: request.options.temperature,
        max_tokens: request.options.max_tokens,
        reasoning,
        session_id: request.options.session_id.as_deref(),
        prompt_cache_key: request.options.prompt_cache_key.as_deref(),
        cache_control: request.options.cache_control.as_ref(),
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
        model: response.model,
        text,
        reasoning,
        reasoning_details,
        tool_calls,
        finish_reason: choice.and_then(|choice| choice.finish_reason.clone()),
        usage: response.usage.map(|usage| {
            let prompt_details = usage.prompt_tokens_details;
            let completion_details = usage.completion_tokens_details;
            Usage {
                input_tokens: usage.prompt_tokens,
                output_tokens: usage.completion_tokens,
                total_tokens: usage.total_tokens,
                cost: usage.cost,
                upstream_inference_cost: usage.cost_details.and_then(|d| d.upstream_inference_cost),
                cached_tokens: prompt_details.as_ref().and_then(|d| d.cached_tokens),
                cache_write_tokens: prompt_details.as_ref().and_then(|d| d.cache_write_tokens),
                reasoning_tokens: completion_details.as_ref().and_then(|d| d.reasoning_tokens),
                audio_tokens: prompt_details.as_ref().and_then(|d| d.audio_tokens),
            }
        }),
    })
}

pub(crate) fn stream_events(chunk: StreamChunk) -> Vec<LlmEvent> {
    let mut events = Vec::new();
    let details = chunk.reasoning_details;
    if let Some(reasoning) = chunk.reasoning {
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
        reasoning_details: message
            .reasoning_details
            .clone()
            .filter(|details| !details.is_empty()),
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
    use crate::{ReasoningEffort, RequestOptions, ToolDefinition, ToolSpec};

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
            reasoning_effort: ReasoningEffort::Auto,
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
    fn serializes_reasoning_effort_and_preserves_reasoning_details() {
        let messages = [Message::assistant_with_tool_calls_and_reasoning(
            None,
            vec![crate::ToolCall {
                id: "call-1".into(),
                name: "weather".into(),
                arguments: "{}".into(),
            }],
            Some("think".into()),
            vec![serde_json::json!({"type": "reasoning.text", "text": "think"})],
        )];
        let options = RequestOptions::default();
        let mut request = request("model-a", &messages, &[], &options);
        request.reasoning_effort = ReasoningEffort::High;
        let json: serde_json::Value =
            serde_json::from_str(&serialize_request(&request).unwrap()).unwrap();
        assert_eq!(json["reasoning"]["effort"], "high");
        assert_eq!(json["messages"][0]["reasoning_details"][0]["text"], "think");
    }

    /// `Auto` and `None` are opposites and must not produce the same request.
    /// Omitting the field lets a reasoning model apply its own default, which
    /// is typically enabled — so collapsing `none` into "omit" turns reasoning
    /// *on* for someone who asked for it off.
    #[test]
    fn auto_omits_reasoning_but_none_disables_it_explicitly() {
        let messages = [Message::user("hello")];
        let options = RequestOptions::default();

        let mut request = request("model-a", &messages, &[], &options);
        request.reasoning_effort = ReasoningEffort::Auto;
        let json: serde_json::Value =
            serde_json::from_str(&serialize_request(&request).unwrap()).unwrap();
        assert!(
            json.get("reasoning").is_none(),
            "Auto must omit the field entirely, got {json}"
        );

        request.reasoning_effort = ReasoningEffort::None;
        let json: serde_json::Value =
            serde_json::from_str(&serialize_request(&request).unwrap()).unwrap();
        assert_eq!(json["reasoning"]["effort"], "none");
    }

    #[test]
    fn omits_empty_tools_and_unset_options() {
        let messages = [Message::user("hello")];
        let options = RequestOptions {
            temperature: None,
            max_tokens: None,
            ..RequestOptions::default()
        };
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
    fn rejects_tool_call_without_index() {
        let error = deserialize_stream_chunk(
            r#"{"choices":[{"delta":{"tool_calls":[{"id":"call-1","function":{"name":"bash"}}]}}]}"#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("missing index"));
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
        let body = serialize_request(&request("model-a", &messages, &[], &options)).unwrap();
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
        let body = serialize_request(&request("model-a", &messages, &[], &options)).unwrap();
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
        let body = serialize_request(&request("model-a", &messages, &[], &options)).unwrap();
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();

        let content = &json["messages"][0]["content"];
        assert!(content.is_array());
        let parts = content.as_array().unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "hello");
    }
}
