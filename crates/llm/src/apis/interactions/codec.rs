use crate::{
    ContentPart, LlmError, LlmRequest, Message, ReasoningEffort, Role, StopReason, ToolCall,
    ToolSpec, Usage,
};
use serde::{Deserialize, Serialize};

/// Request body for `POST /interactions`.
#[derive(Serialize)]
struct Request<'a> {
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<String>,
    input: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<serde_json::Value>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    generation_config: Option<GenerationConfig>,
}

#[derive(Serialize)]
struct GenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking_level: Option<&'static str>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WireEvent {
    pub(crate) event_type: String,
    pub(crate) index: Option<usize>,
    pub(crate) step: Option<WireStep>,
    pub(crate) delta: Option<WireDelta>,
    pub(crate) metadata: Option<WireMetadata>,
    pub(crate) interaction: Option<WireInteraction>,
    pub(crate) status: Option<String>,
    pub(crate) usage: Option<WireUsage>,
    pub(crate) error: Option<WireError>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WireStep {
    #[serde(rename = "type")]
    pub(crate) kind: String,
    #[serde(default)]
    pub(crate) content: Vec<WireContent>,
    pub(crate) name: Option<String>,
    pub(crate) id: Option<String>,
    pub(crate) signature: Option<String>,
    pub(crate) arguments: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WireContent {
    pub(crate) text: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WireDelta {
    #[serde(rename = "type")]
    pub(crate) kind: String,
    pub(crate) text: Option<String>,
    pub(crate) arguments: Option<String>,
    pub(crate) content: Option<WireContent>,
    pub(crate) signature: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WireMetadata {
    pub(crate) total_usage: Option<WireUsage>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WireInteraction {
    pub(crate) model: Option<String>,
    pub(crate) status: Option<String>,
    #[serde(default)]
    pub(crate) steps: Vec<WireStep>,
    pub(crate) usage: Option<WireUsage>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WireError {
    pub(crate) code: Option<serde_json::Value>,
    pub(crate) message: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub(crate) struct WireUsage {
    pub(crate) total_input_tokens: Option<u64>,
    pub(crate) total_output_tokens: Option<u64>,
    pub(crate) total_tokens: Option<u64>,
    pub(crate) total_cached_tokens: Option<u64>,
    pub(crate) total_thought_tokens: Option<u64>,
}

pub(crate) fn serialize_request(request: &LlmRequest<'_>) -> Result<String, LlmError> {
    let system_instruction = system_instruction(request.messages);
    let tool_names: std::collections::HashMap<&str, &str> = request
        .messages
        .iter()
        .filter_map(|message| message.tool_calls.as_ref())
        .flatten()
        .map(|call| (call.id.as_str(), call.name.as_str()))
        .collect();
    let input = request
        .messages
        .iter()
        .filter(|message| message.role != Role::System)
        .flat_map(|message| message_steps(message, &tool_names))
        .collect();
    let tools = request.tools.iter().map(wire_tool).collect();

    let body = Request {
        model: request.model_id,
        system_instruction,
        input,
        tools,
        stream: true,
        generation_config: generation_config(request),
    };
    let json = serde_json::to_string(&body).map_err(LlmError::Serialization)?;
    Ok(json)
}

pub(crate) fn deserialize_stream_event(body: &str) -> Result<WireEvent, LlmError> {
    serde_json::from_str(body).map_err(LlmError::Serialization)
}

pub(crate) fn usage_from_wire(usage: &WireUsage) -> Usage {
    Usage {
        input_tokens: usage.total_input_tokens.unwrap_or(0),
        output_tokens: usage.total_output_tokens.unwrap_or(0),
        total_tokens: usage.total_tokens,
        cached_tokens: usage.total_cached_tokens,
        reasoning_tokens: usage.total_thought_tokens,
        ..Usage::default()
    }
}

pub(crate) fn stop_reason_for_status(status: Option<&str>) -> StopReason {
    match status {
        Some("requires_action") => StopReason::ToolUse,
        Some("incomplete" | "budget_exceeded") => StopReason::Length,
        Some("failed") => StopReason::Error,
        Some("cancelled") => StopReason::Aborted,
        _ => StopReason::Stop,
    }
}

fn system_instruction(messages: &[Message]) -> Option<String> {
    let joined = messages
        .iter()
        .filter(|message| message.role == Role::System)
        .filter_map(|message| message.content.as_deref())
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    (!joined.is_empty()).then_some(joined)
}

fn generation_config(request: &LlmRequest<'_>) -> Option<GenerationConfig> {
    let max_output_tokens = request.options.max_tokens;
    let thinking_level = thinking_level(request.reasoning_effort);
    if max_output_tokens.is_none() && thinking_level.is_none() {
        return None;
    }
    Some(GenerationConfig {
        max_output_tokens,
        thinking_level,
    })
}

fn thinking_level(effort: ReasoningEffort) -> Option<&'static str> {
    match effort {
        ReasoningEffort::None => None,
        ReasoningEffort::Minimal | ReasoningEffort::Low => Some("low"),
        ReasoningEffort::Medium => Some("medium"),
        ReasoningEffort::High | ReasoningEffort::XHigh | ReasoningEffort::Max => Some("high"),
    }
}

fn message_steps(
    message: &Message,
    tool_names: &std::collections::HashMap<&str, &str>,
) -> Vec<serde_json::Value> {
    match message.role {
        Role::System => Vec::new(),
        Role::User => vec![serde_json::json!({
            "type": "user_input",
            "content": message_contents(message),
        })],
        Role::Assistant => assistant_steps(message),
        Role::Tool => {
            let step = serde_json::json!({
                "type": "function_result",
                "call_id": message.tool_call_id,
                "result": [text_content(&message.content.clone().unwrap_or_default())],
            });
            match message
                .tool_call_id
                .as_deref()
                .and_then(|id| tool_names.get(id))
            {
                Some(name) => {
                    let mut step = step;
                    step["name"] = serde_json::Value::String((*name).to_string());
                    vec![step]
                }
                None => vec![step],
            }
        }
    }
}

fn assistant_steps(message: &Message) -> Vec<serde_json::Value> {
    let mut steps = Vec::new();
    if let Some(calls) = &message.tool_calls {
        steps.extend(calls.iter().map(function_call_step));
    }
    if steps.is_empty()
        && let Some(contents) =
            (!message_contents(message).is_empty()).then_some(message_contents(message))
    {
        steps.push(serde_json::json!({
            "type": "model_output",
            "content": contents,
        }));
    }
    steps
}

fn function_call_step(call: &ToolCall) -> serde_json::Value {
    let mut step = serde_json::json!({
        "type": "function_call",
        "name": call.name,
        "arguments": parse_arguments(&call.arguments),
        "id": call.id,
    });
    if let Some(signature) = &call.signature {
        step["signature"] = serde_json::Value::String(signature.clone());
    }
    step
}

fn parse_arguments(arguments: &str) -> serde_json::Value {
    serde_json::from_str(arguments).unwrap_or_else(|_| serde_json::Value::String(arguments.into()))
}

fn message_contents(message: &Message) -> Vec<serde_json::Value> {
    if let Some(parts) = &message.content_parts {
        return parts.iter().map(content_part).collect();
    }
    match message.content.as_deref() {
        Some(text) if !text.is_empty() => vec![text_content(text)],
        _ => Vec::new(),
    }
}

fn text_content(text: &str) -> serde_json::Value {
    serde_json::json!({ "type": "text", "text": text })
}

fn content_part(part: &ContentPart) -> serde_json::Value {
    match part {
        ContentPart::Text { text } => text_content(text),
        ContentPart::Image { image_url } => image_content(&image_url.url),
    }
}

fn image_content(url: &str) -> serde_json::Value {
    if let Some(rest) = url.strip_prefix("data:")
        && let Some((meta, data)) = rest.split_once(',')
    {
        let mime_type = meta.split(';').next().unwrap_or("application/octet-stream");
        if meta.contains("base64") {
            return serde_json::json!({
                "type": "image",
                "mime_type": mime_type,
                "data": data,
            });
        }
    }
    serde_json::json!({ "type": "image", "uri": url })
}

fn wire_tool(tool: &ToolSpec) -> serde_json::Value {
    match tool {
        ToolSpec::Function(tool) => serde_json::json!({
            "type": "function",
            "name": tool.name,
            "description": tool.description,
            "parameters": tool.parameters,
        }),
        ToolSpec::Server(tool) => serde_json::json!({ "type": tool.kind }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Message, RequestOptions, ToolDefinition, ToolSpec};

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
            reasoning_effort: ReasoningEffort::None,
            provider_order: None,
        }
    }

    #[test]
    fn serializes_system_input_and_stream_flag() {
        let messages = [Message::system("be nice"), Message::user("hello")];
        let options = RequestOptions::default();
        let body = serialize_request(&request("gemini-x", &messages, &[], &options)).unwrap();
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();

        assert_eq!(json["model"], "gemini-x");
        assert_eq!(json["stream"], true);
        assert_eq!(json["system_instruction"], "be nice");
        assert_eq!(json["input"][0]["type"], "user_input");
        assert_eq!(json["input"][0]["content"][0]["type"], "text");
        assert_eq!(json["input"][0]["content"][0]["text"], "hello");
        assert!(json.get("generation_config").is_none());
    }

    #[test]
    fn serializes_function_tool_and_tool_choice_omitted() {
        let messages = [Message::user("hello")];
        let tools = [ToolSpec::Function(ToolDefinition {
            name: "weather".into(),
            description: "Get weather".into(),
            parameters: serde_json::json!({"type": "object"}),
        })];
        let body = serialize_request(&request(
            "gemini-x",
            &messages,
            &tools,
            &RequestOptions::default(),
        ))
        .unwrap();
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["tools"][0]["type"], "function");
        assert_eq!(json["tools"][0]["name"], "weather");
        assert_eq!(json["tools"][0]["parameters"]["type"], "object");
    }

    #[test]
    fn serializes_server_tool_as_type_only() {
        let messages = [Message::user("hello")];
        let tools = [ToolSpec::Server(crate::ServerTool {
            kind: "google_search".into(),
        })];
        let body = serialize_request(&request(
            "gemini-x",
            &messages,
            &tools,
            &RequestOptions::default(),
        ))
        .unwrap();
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["tools"][0]["type"], "google_search");
        assert!(json["tools"][0].get("function").is_none());
    }

    #[test]
    fn serializes_assistant_tool_call_as_function_call_step() {
        let messages = [Message::assistant_with_tool_calls_and_reasoning(
            None,
            vec![ToolCall {
                id: "call-1".into(),
                name: "weather".into(),
                arguments: "{\"city\":\"Paris\"}".into(),
                signature: None,
            }],
            None,
            Vec::new(),
        )];
        let body = serialize_request(&request(
            "gemini-x",
            &messages,
            &[],
            &RequestOptions::default(),
        ))
        .unwrap();
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        let step = &json["input"][0];
        assert_eq!(step["type"], "function_call");
        assert_eq!(step["name"], "weather");
        assert_eq!(step["id"], "call-1");
        assert_eq!(step["arguments"]["city"], "Paris");
    }

    #[test]
    fn serializes_tool_result_as_function_result_step() {
        let messages = [Message::tool_result("{\"temp\":20}", "call-1")];
        let body = serialize_request(&request(
            "gemini-x",
            &messages,
            &[],
            &RequestOptions::default(),
        ))
        .unwrap();
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["input"][0]["type"], "function_result");
        assert_eq!(json["input"][0]["call_id"], "call-1");
        assert_eq!(json["input"][0]["result"][0]["type"], "text");
        assert_eq!(json["input"][0]["result"][0]["text"], "{\"temp\":20}");
    }

    #[test]
    fn maps_max_tokens_and_thinking_level() {
        let messages = [Message::user("hello")];
        let options = RequestOptions {
            max_tokens: Some(64),
            ..RequestOptions::default()
        };
        let mut request = request("gemini-x", &messages, &[], &options);
        request.reasoning_effort = ReasoningEffort::Medium;
        let json: serde_json::Value =
            serde_json::from_str(&serialize_request(&request).unwrap()).unwrap();
        assert_eq!(json["generation_config"]["max_output_tokens"], 64);
        assert_eq!(json["generation_config"]["thinking_level"], "medium");
    }

    #[test]
    fn serializes_base64_image_content() {
        let messages = [Message::user_with_parts(vec![ContentPart::Image {
            image_url: crate::ImageUrl {
                url: "data:image/png;base64,AAAA".into(),
            },
        }])];
        let body = serialize_request(&request(
            "gemini-x",
            &messages,
            &[],
            &RequestOptions::default(),
        ))
        .unwrap();
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        let content = &json["input"][0]["content"][0];
        assert_eq!(content["type"], "image");
        assert_eq!(content["mime_type"], "image/png");
        assert_eq!(content["data"], "AAAA");
    }

    #[test]
    fn serializes_direct_image_uri() {
        let messages = [Message::user_with_parts(vec![ContentPart::Image {
            image_url: crate::ImageUrl {
                url: "https://example.com/a.png".into(),
            },
        }])];
        let body = serialize_request(&request(
            "gemini-x",
            &messages,
            &[],
            &RequestOptions::default(),
        ))
        .unwrap();
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        let content = &json["input"][0]["content"][0];
        assert_eq!(content["type"], "image");
        assert_eq!(content["uri"], "https://example.com/a.png");
    }

    #[test]
    fn decodes_step_events_tolerantly() {
        let event = deserialize_stream_event(
            r#"{"event_type":"step.delta","index":0,"delta":{"type":"text","text":"hi"}}"#,
        )
        .unwrap();
        assert_eq!(event.event_type, "step.delta");
        assert_eq!(event.delta.unwrap().text.as_deref(), Some("hi"));

        // Unknown delta kinds must not fail parsing.
        deserialize_stream_event(
            r#"{"event_type":"step.delta","index":0,"delta":{"type":"future_thing"}}"#,
        )
        .unwrap();
        // Unknown event types must not fail parsing.
        deserialize_stream_event(r#"{"event_type":"surprise","index":9}"#).unwrap();
    }

    #[test]
    fn maps_status_and_usage() {
        assert_eq!(
            stop_reason_for_status(Some("requires_action")),
            StopReason::ToolUse
        );
        assert_eq!(
            stop_reason_for_status(Some("incomplete")),
            StopReason::Length
        );
        assert_eq!(stop_reason_for_status(Some("failed")), StopReason::Error);
        assert_eq!(stop_reason_for_status(Some("completed")), StopReason::Stop);

        let usage = usage_from_wire(&WireUsage {
            total_input_tokens: Some(11),
            total_output_tokens: Some(7),
            total_cached_tokens: Some(3),
            total_thought_tokens: Some(5),
            ..WireUsage::default()
        });
        assert_eq!(usage.input_tokens, 11);
        assert_eq!(usage.output_tokens, 7);
        assert_eq!(usage.cached_tokens, Some(3));
        assert_eq!(usage.reasoning_tokens, Some(5));
    }
}
