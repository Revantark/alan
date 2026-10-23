use crate::apis::stream::WireError;
use crate::{
    ContentPart, LlmApiCodec, LlmError, LlmRequest, LlmStreamDecoder, Message, ReasoningEffort,
    Role, ToolSpec, Usage,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Requested on every call so reasoning items carry `encrypted_content` and
/// can be replayed in later turns.
pub(crate) const REASONING_INCLUDE: &str = "reasoning.encrypted_content";

/// Codec for the canonical `/responses` protocol.
pub struct DefaultResponsesCodec;

impl LlmApiCodec for DefaultResponsesCodec {
    fn request(&self, request: &LlmRequest<'_>) -> Result<String, LlmError> {
        serialize_request(request)
    }

    fn decoder(&self) -> Box<dyn LlmStreamDecoder> {
        Box::new(super::ResponsesDecoder::default())
    }
}

/// Canonical `POST {base}/responses` body. Providers whose gateway accepts
/// extra fields embed it via `#[serde(flatten)]` and add their own.
///
/// The backend requires stateless requests: `store` must be `false`,
/// `previous_response_id`/`conversation` must be absent, and the full history
/// is resent every turn.
#[derive(Serialize)]
pub struct BaseRequest<'a> {
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    input: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<serde_json::Value>,
    stream: bool,
    store: bool,
    include: Vec<&'static str>,
    /// Omitted entirely when the effort is `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ReasoningConfig>,
}

impl<'a> BaseRequest<'a> {
    /// Build the request body, rejecting tool outputs without a matching
    /// `call_id` in the history.
    pub fn new(request: &'a LlmRequest<'a>) -> Result<Self, LlmError> {
        let instructions = instructions(request.messages);
        let call_ids: HashSet<&str> = request
            .messages
            .iter()
            .filter_map(|message| message.tool_calls.as_ref())
            .flatten()
            .map(|call| call.id.as_str())
            .collect();
        let mut input = Vec::new();
        for message in request.messages {
            input.extend(message_items(message, &call_ids)?);
        }
        let tools = request
            .tools
            .iter()
            .map(wire_tool)
            .collect::<Result<Vec<_>, LlmError>>()?;
        Ok(Self {
            model: request.model_id,
            instructions,
            input,
            tools,
            stream: true,
            store: false,
            include: vec![REASONING_INCLUDE],
            reasoning: reasoning_config(request.reasoning_effort),
        })
    }
}

#[derive(Serialize)]
struct ReasoningConfig {
    effort: &'static str,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WireEvent {
    #[serde(rename = "type")]
    pub(crate) kind: String,
    #[serde(default)]
    pub(crate) message: Option<String>,
    #[serde(default)]
    pub(crate) delta: Option<String>,
    #[serde(default)]
    pub(crate) arguments: Option<String>,
    #[serde(default)]
    pub(crate) output_index: Option<usize>,
    #[serde(default)]
    pub(crate) item: Option<serde_json::Value>,
    #[serde(default)]
    pub(crate) response: Option<WireResponse>,
    #[serde(default)]
    pub(crate) error: Option<WireError>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct WireResponse {
    #[serde(default)]
    pub(crate) model: Option<String>,
    #[serde(default)]
    pub(crate) usage: Option<serde_json::Value>,
    #[serde(default)]
    pub(crate) output: Vec<serde_json::Value>,
    #[serde(default)]
    pub(crate) error: Option<WireError>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub(crate) struct WireUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    total_tokens: Option<u64>,
    input_tokens_details: Option<InputTokensDetails>,
    output_tokens_details: Option<OutputTokensDetails>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct InputTokensDetails {
    cached_tokens: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct OutputTokensDetails {
    reasoning_tokens: Option<u64>,
}

fn serialize_request(request: &LlmRequest<'_>) -> Result<String, LlmError> {
    serde_json::to_string(&BaseRequest::new(request)?).map_err(LlmError::Serialization)
}

pub(crate) fn deserialize_stream_event(body: &str) -> Result<WireEvent, LlmError> {
    serde_json::from_str(body).map_err(LlmError::Serialization)
}

/// Decode canonical Responses usage, ignoring provider-specific fields.
pub fn decode_usage(value: &serde_json::Value) -> Result<Usage, LlmError> {
    let usage: WireUsage =
        serde_json::from_value(value.clone()).map_err(LlmError::Serialization)?;
    Ok(usage_from_wire(&usage))
}

fn usage_from_wire(usage: &WireUsage) -> Usage {
    Usage {
        input_tokens: usage.input_tokens.unwrap_or(0),
        output_tokens: usage.output_tokens.unwrap_or(0),
        total_tokens: usage.total_tokens,
        cached_tokens: usage
            .input_tokens_details
            .as_ref()
            .and_then(|details| details.cached_tokens),
        reasoning_tokens: usage
            .output_tokens_details
            .as_ref()
            .and_then(|details| details.reasoning_tokens),
        ..Usage::default()
    }
}

/// Levels above `high` collapse onto it; which levels the backend accepts is
/// unconfirmed.
fn reasoning_config(effort: ReasoningEffort) -> Option<ReasoningConfig> {
    let effort = match effort {
        ReasoningEffort::None => return None,
        ReasoningEffort::Minimal => "minimal",
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High | ReasoningEffort::XHigh | ReasoningEffort::Max => "high",
    };
    Some(ReasoningConfig { effort })
}

fn instructions(messages: &[Message]) -> Option<String> {
    let joined = messages
        .iter()
        .filter(|message| message.role == Role::System)
        .filter_map(|message| message.content.as_deref())
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    (!joined.is_empty()).then_some(joined)
}

fn message_items(
    message: &Message,
    call_ids: &HashSet<&str>,
) -> Result<Vec<serde_json::Value>, LlmError> {
    match message.role {
        Role::System => Ok(Vec::new()),
        Role::User => Ok(vec![user_item(message)?]),
        Role::Assistant => Ok(assistant_items(message)),
        Role::Tool => tool_output_item(message, call_ids).map(|item| vec![item]),
    }
}

fn user_item(message: &Message) -> Result<serde_json::Value, LlmError> {
    let mut content = Vec::new();
    if let Some(parts) = &message.content_parts {
        for part in parts {
            match part {
                ContentPart::Text { text } => content.push(input_text(text)),
                ContentPart::Image { image_url } => {
                    content.push(input_image(&image_url.url));
                }
            }
        }
    }
    if content.is_empty() {
        let text = message.content.as_deref().unwrap_or_default();
        content.push(input_text(text));
    }
    Ok(serde_json::json!({
        "type": "message",
        "role": "user",
        "content": content,
    }))
}

fn input_text(text: &str) -> serde_json::Value {
    serde_json::json!({ "type": "input_text", "text": text })
}

/// Base64 data URIs and direct URLs both serialize as `input_image` items.
fn input_image(url: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "input_image",
        "image_url": url,
    })
}

/// Assistant turns replay as: reasoning items (verbatim, original position),
/// then function calls, then the text message. Reasoning entries that are not
/// Responses reasoning items (e.g. summaries captured from other providers)
/// are skipped; the recorded `encrypted_content` is preserved untouched.
fn assistant_items(message: &Message) -> Vec<serde_json::Value> {
    let mut items = Vec::new();
    if let Some(details) = &message.reasoning_details {
        for detail in details {
            if detail.get("type").and_then(serde_json::Value::as_str) == Some("reasoning") {
                items.push(detail.clone());
            }
        }
    }

    let calls = message.tool_calls.as_deref().unwrap_or(&[]);
    let text = message_text(message);
    if !items.is_empty() && calls.is_empty() && text.is_none() {
        // A reasoning item directly before the next user message is rejected
        // by the backend (HTTP 400); without other content in this turn there
        // is nothing to keep it attached to, so drop it.
        items.clear();
    }

    for call in calls {
        items.push(serde_json::json!({
            "type": "function_call",
            "call_id": call.id,
            "name": call.name,
            "arguments": call.arguments,
        }));
    }
    if let Some(text) = text {
        items.push(serde_json::json!({
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": text }],
        }));
    }
    items
}

fn tool_output_item(
    message: &Message,
    call_ids: &HashSet<&str>,
) -> Result<serde_json::Value, LlmError> {
    let call_id = message
        .tool_call_id
        .as_deref()
        .ok_or_else(|| LlmError::InvalidResponse("tool result is missing tool_call_id".into()))?;
    if !call_ids.contains(call_id) {
        return Err(LlmError::InvalidResponse(format!(
            "tool result references unknown tool call {call_id}"
        )));
    }
    Ok(serde_json::json!({
        "type": "function_call_output",
        "call_id": call_id,
        "output": message.content.clone().unwrap_or_default(),
    }))
}

fn message_text(message: &Message) -> Option<String> {
    if let Some(parts) = &message.content_parts {
        let joined = parts
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text { text } => Some(text.as_str()),
                ContentPart::Image { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        return (!joined.is_empty()).then_some(joined);
    }
    message
        .content
        .as_deref()
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn wire_tool(tool: &ToolSpec) -> Result<serde_json::Value, LlmError> {
    match tool {
        ToolSpec::Function(tool) => Ok(serde_json::json!({
            "type": "function",
            "name": tool.name,
            "description": tool.description,
            "parameters": tool.parameters,
            "strict": false,
        })),
        ToolSpec::Server(tool) => Ok(serde_json::json!({
            "type": tool.kind,
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Message, RequestOptions, ToolCall, ToolDefinition};

    fn body_for(messages: &[Message]) -> serde_json::Value {
        let options = RequestOptions::default();
        let request = LlmRequest {
            model_id: "gpt-test",
            messages,
            tools: &[],
            options: &options,
            credential: None,
            reasoning_effort: ReasoningEffort::None,
            extensions: crate::Extensions::default(),
        };
        let json = serialize_request(&request).unwrap();
        serde_json::from_str(&json).unwrap()
    }

    fn request_with(
        messages: &[Message],
        tools: &[ToolSpec],
        effort: ReasoningEffort,
    ) -> Result<String, LlmError> {
        let options = RequestOptions::default();
        let request = LlmRequest {
            model_id: "gpt-test",
            messages,
            tools,
            options: &options,
            credential: None,
            reasoning_effort: effort,
            extensions: crate::Extensions::default(),
        };
        serialize_request(&request)
    }

    fn reasoning_item(id: &str, encrypted: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "reasoning",
            "id": id,
            "summary": [],
            "encrypted_content": encrypted,
        })
    }

    #[test]
    fn body_requires_stateless_shape() {
        let messages = [Message::user("hello")];
        let body = body_for(&messages);

        assert_eq!(body["model"], "gpt-test");
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(
            body["include"],
            serde_json::json!(["reasoning.encrypted_content"])
        );
        // Stateless requests must not reference prior server-side turns.
        assert!(body.get("previous_response_id").is_none());
        assert!(body.get("conversation").is_none());
        // `tools` is omitted entirely when there are none.
        assert!(body.get("tools").is_none());
        // Effort `None` omits the reasoning block entirely.
        assert!(body.get("reasoning").is_none());
    }

    #[test]
    fn system_prompt_maps_to_instructions() {
        let messages = [Message::system("be terse"), Message::user("hello")];
        let body = body_for(&messages);

        assert_eq!(body["instructions"], "be terse");
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(input[0]["content"][0]["text"], "hello");
    }

    #[test]
    fn assistant_history_replays_with_reasoning_first() {
        let messages = [
            Message::user("hi"),
            Message::assistant_with_reasoning(
                Some("done".into()),
                None,
                vec![reasoning_item("rs_1", "ENC")],
            ),
            Message::user("more"),
        ];
        let body = body_for(&messages);
        let input = body["input"].as_array().unwrap();

        assert_eq!(input.len(), 4);
        assert_eq!(input[1]["type"], "reasoning");
        assert_eq!(input[1]["id"], "rs_1");
        assert_eq!(input[1]["encrypted_content"], "ENC");
        assert_eq!(input[1]["summary"], serde_json::json!([]));
        assert_eq!(input[2]["type"], "message");
        assert_eq!(input[2]["role"], "assistant");
        assert_eq!(input[2]["content"][0]["type"], "output_text");
        assert_eq!(input[2]["content"][0]["text"], "done");
        assert_eq!(input[3]["type"], "message");
        assert_eq!(input[3]["role"], "user");
    }

    #[test]
    fn reasoning_is_dropped_when_it_would_precede_a_user_message() {
        let messages = [
            Message::user("hi"),
            Message::assistant_with_reasoning(None, None, vec![reasoning_item("rs_1", "ENC")]),
            Message::user("more"),
        ];
        let body = body_for(&messages);
        let input = body["input"].as_array().unwrap();

        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[1]["type"], "message");
    }

    #[test]
    fn tool_calls_map_to_function_call_items() {
        let messages = [
            Message::user("run it"),
            Message::assistant_with_tool_calls_and_reasoning(
                None,
                vec![ToolCall {
                    id: "call_1".into(),
                    name: "bash".into(),
                    arguments: "{\"command\":\"ls\"}".into(),
                    signature: None,
                }],
                None,
                vec![reasoning_item("rs_1", "ENC")],
            ),
        ];
        let body = body_for(&messages);
        let input = body["input"].as_array().unwrap();

        assert_eq!(input.len(), 3);
        assert_eq!(input[1]["type"], "reasoning");
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[2]["call_id"], "call_1");
        assert_eq!(input[2]["name"], "bash");
        // Arguments stay a JSON-encoded string, never parsed into an object.
        assert_eq!(input[2]["arguments"], "{\"command\":\"ls\"}");
    }

    #[test]
    fn tool_results_map_to_function_call_output_items() {
        let messages = [
            Message::user("run it"),
            Message::assistant_with_tool_calls_and_reasoning(
                None,
                vec![ToolCall {
                    id: "call_1".into(),
                    name: "bash".into(),
                    arguments: "{}".into(),
                    signature: None,
                }],
                None,
                Vec::new(),
            ),
            Message::tool_result("output", "call_1"),
        ];
        let body = body_for(&messages);
        let input = body["input"].as_array().unwrap();

        assert_eq!(input.len(), 3);
        assert_eq!(input[2]["type"], "function_call_output");
        assert_eq!(input[2]["call_id"], "call_1");
        assert_eq!(input[2]["output"], "output");
    }

    #[test]
    fn unpaired_tool_result_is_an_error() {
        let messages = [Message::tool_result("orphan", "missing_call")];
        let error = request_with(&messages, &[], ReasoningEffort::None).unwrap_err();

        assert!(error.to_string().contains("missing_call"));
    }

    #[test]
    fn function_tools_serialize_with_strict_false() {
        let tools = [ToolSpec::Function(ToolDefinition {
            name: "bash".into(),
            description: "run a command".into(),
            parameters: serde_json::json!({"type": "object"}),
        })];
        let messages = [Message::user("run it")];
        let json = request_with(&messages, &tools, ReasoningEffort::None).unwrap();
        let body: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "bash");
        assert_eq!(body["tools"][0]["strict"], false);
    }

    #[test]
    fn server_tools_serialize_as_typed_items() {
        let tools = [ToolSpec::Server(crate::ServerTool {
            kind: "openrouter:web_search".into(),
        })];
        let messages = [Message::user("run it")];
        let json = request_with(&messages, &tools, ReasoningEffort::None).unwrap();
        let body: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(body["tools"][0]["type"], "openrouter:web_search");
    }

    #[test]
    fn reasoning_effort_maps_to_reasoning_object() {
        let messages = [Message::user("hi")];
        let json = request_with(&messages, &[], ReasoningEffort::Medium).unwrap();
        let body: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(body["reasoning"]["effort"], "medium");
    }

    #[test]
    fn default_codec_keeps_gateway_fields_out_of_the_body() {
        // ChatGPT rejects OpenRouter-specific fields.
        let mut extensions = crate::Extensions::default();
        extensions.insert(crate::SessionId("s-1".into()));
        extensions.insert(crate::OpenRouterOptions {
            only: vec!["azure".into()],
        });
        let options = RequestOptions {
            temperature: Some(0.7),
            max_tokens: Some(256),
            cache_control: Some(crate::PromptCacheControl::one_hour()),
        };
        let request = LlmRequest {
            model_id: "gpt-test",
            messages: &[Message::user("hi")],
            tools: &[],
            options: &options,
            credential: None,
            reasoning_effort: ReasoningEffort::None,
            extensions,
        };
        let body: serde_json::Value =
            serde_json::from_str(&DefaultResponsesCodec.request(&request).unwrap()).unwrap();

        for field in [
            "provider",
            "session_id",
            "prompt_cache_key",
            "cache_control",
            "temperature",
            "max_output_tokens",
        ] {
            assert!(
                body.get(field).is_none(),
                "{field} must stay out of the canonical body"
            );
        }
    }

    #[test]
    fn user_images_serialize_as_input_image_items() {
        let data_uri = "data:image/png;base64,iVBORw0KGgo=";
        let messages = [Message::user_with_parts(vec![
            ContentPart::Text {
                text: "What's in this image?".into(),
            },
            ContentPart::Image {
                image_url: crate::ImageUrl {
                    url: "https://example.invalid/a.png".into(),
                },
            },
            ContentPart::Image {
                image_url: crate::ImageUrl {
                    url: data_uri.into(),
                },
            },
        ])];
        let body = body_for(&messages);
        let input = body["input"].as_array().unwrap();

        assert_eq!(input.len(), 1);
        let content = input[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 3);
        assert_eq!(content[0]["type"], "input_text");
        assert_eq!(content[0]["text"], "What's in this image?");
        assert_eq!(content[1]["type"], "input_image");
        assert_eq!(content[1]["image_url"], "https://example.invalid/a.png");
        assert_eq!(content[2]["type"], "input_image");
        assert_eq!(content[2]["image_url"], data_uri);
    }
}
