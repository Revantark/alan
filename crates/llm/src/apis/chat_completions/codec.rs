use crate::{
    AssistantMessage, ContentBlock, LlmError, LlmRequest, Message, Role, StopReason, ToolCall,
};
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
struct Request<'a> {
    model: &'a str,
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
struct Response {
    model: Option<String>,
    choices: Vec<Choice>,
    usage: Option<WireUsage>,
}

#[derive(Deserialize)]
struct Choice {
    message: ResponseMessage,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ResponseMessage {
    content: Option<String>,
    tool_calls: Option<Vec<ResponseToolCall>>,
}

#[derive(Deserialize)]
struct WireUsage {
    prompt_tokens: u64,
    completion_tokens: u64,
}

#[derive(Deserialize)]
struct ResponseToolCall {
    id: String,
    function: ResponseFunction,
}

#[derive(Deserialize)]
struct ResponseFunction {
    name: String,
    arguments: String,
}

pub(crate) fn serialize_request(request: &LlmRequest<'_>) -> Result<String, LlmError> {
    let messages = request.messages.iter().map(wire_message).collect();
    let tools = request.tools.iter().map(wire_tool).collect();
    serde_json::to_string(&Request {
        model: request.model_id,
        messages,
        tools,
        temperature: request.options.temperature,
        max_tokens: request.options.max_tokens,
    })
    .map_err(LlmError::Serialization)
}

pub(crate) fn deserialize_response(body: &str) -> Result<AssistantMessage, LlmError> {
    let response: Response = serde_json::from_str(body).map_err(LlmError::Serialization)?;
    let choice = response
        .choices
        .first()
        .ok_or_else(|| LlmError::InvalidResponse("response contains no choices".into()))?;
    let message = &choice.message;
    let calls = message
        .tool_calls
        .as_ref()
        .into_iter()
        .flatten()
        .map(|call| ToolCall {
            id: call.id.clone(),
            name: call.function.name.clone(),
            arguments: call.function.arguments.clone(),
        })
        .collect::<Vec<_>>();

    let mut content = Vec::new();
    if let Some(text) = message.content.clone().filter(|text| !text.is_empty()) {
        content.push(ContentBlock::Text(text));
    }
    content.extend(calls.into_iter().map(ContentBlock::ToolCall));
    let has_content = !content.is_empty();

    Ok(AssistantMessage {
        content,
        stop_reason: stop_reason(choice.finish_reason.as_deref(), has_content),
        usage: response.usage.map(|usage| crate::Usage {
            input_tokens: usage.prompt_tokens,
            output_tokens: usage.completion_tokens,
        }),
        model: response.model,
    })
}

fn stop_reason(finish_reason: Option<&str>, has_content: bool) -> StopReason {
    match finish_reason {
        Some("tool_calls") | Some("function_call") => StopReason::ToolUse,
        Some("length") => StopReason::Length,
        Some("content_filter") => StopReason::ContentFilter,
        Some("stop") => StopReason::Stop,
        _ if has_content => StopReason::Stop,
        _ => StopReason::Error,
    }
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
    fn serializes_model_id_options_messages_and_tools() {
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
    fn deserializes_model_usage_finish_reason_and_tool_calls() {
        let body = r#"{
            "model": "served-model",
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "content": "Checking",
                    "tool_calls": [{
                        "id": "call-1",
                        "type": "function",
                        "function": {"name": "weather", "arguments": "{\"city\":\"NYC\"}"}
                    }]
                }
            }],
            "usage": {"prompt_tokens": 11, "completion_tokens": 7}
        }"#;

        let response = deserialize_response(body).unwrap();
        assert_eq!(response.model.as_deref(), Some("served-model"));
        assert_eq!(response.stop_reason, StopReason::ToolUse);
        assert_eq!(response.usage.as_ref().unwrap().input_tokens, 11);
        assert_eq!(response.tool_calls().next().unwrap().name, "weather");
    }

    #[test]
    fn maps_finish_reasons() {
        for (finish_reason, expected) in [
            ("stop", StopReason::Stop),
            ("length", StopReason::Length),
            ("content_filter", StopReason::ContentFilter),
        ] {
            let body = format!(
                r#"{{"choices":[{{"finish_reason":"{finish_reason}","message":{{"content":"done"}}}}]}}"#
            );
            assert_eq!(deserialize_response(&body).unwrap().stop_reason, expected);
        }
    }

    #[test]
    fn rejects_response_without_choices() {
        let error = deserialize_response(r#"{"choices":[]}"#).unwrap_err();
        assert!(matches!(error, LlmError::InvalidResponse(_)));
    }
}
