//! Live integration tests against Google's Gemini Interactions API.
//!
//! These tests require outbound network access and an API key in
//! `GEMINI_API_KEY` or `GOOGLE_API_KEY`. When neither is set they print a
//! skip notice and return successfully instead of failing.

use llm::{
    Credential, HttpClient, InteractionsApi, LlmApi, LlmRequest, Message, ReasoningEffort,
    RequestOptions, ToolDefinition, ToolSpec,
};
use std::sync::Arc;

const BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";
const MODEL: &str = "gemini-3.1-flash-lite";

fn api_key() -> Option<String> {
    std::env::var("GEMINI_API_KEY")
        .or_else(|_| std::env::var("GOOGLE_API_KEY"))
        .ok()
        .filter(|key| !key.is_empty())
}

#[tokio::test]
async fn live_text_completion() {
    let Some(key) = api_key() else {
        eprintln!("skipping live_text_completion: no GEMINI_API_KEY/GOOGLE_API_KEY set");
        return;
    };

    let api = InteractionsApi::new(BASE_URL, Arc::new(HttpClient::new()));
    let credential = Credential::Header("X-goog-api-key".to_string(), key);
    let messages = [
        Message::system("Answer in one short sentence."),
        Message::user("Say hello."),
    ];
    let options = RequestOptions::default();
    let request = LlmRequest {
        model_id: MODEL,
        messages: &messages,
        tools: &[],
        options: &options,
        credential: Some(&credential),
        reasoning_effort: ReasoningEffort::None,
        provider_order: None,
    };

    let response = api.complete(request).await.unwrap();
    assert!(
        !response.text().trim().is_empty(),
        "expected non-empty text response"
    );
    assert!(response.usage.is_some(), "expected usage information");
}

#[tokio::test]
async fn live_function_call() {
    let Some(key) = api_key() else {
        eprintln!("skipping live_function_call: no GEMINI_API_KEY/GOOGLE_API_KEY set");
        return;
    };

    let api = InteractionsApi::new(BASE_URL, Arc::new(HttpClient::new()));
    let credential = Credential::Header("X-goog-api-key".to_string(), key);
    let messages = [Message::user("What is the weather in Paris? Use the tool.")];
    let tools = [ToolSpec::Function(ToolDefinition {
        name: "get_weather".into(),
        description: "Get the current weather for a city".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "city": {"type": "string", "description": "City name"}
            },
            "required": ["city"]
        }),
    })];
    let options = RequestOptions::default();
    let request = LlmRequest {
        model_id: MODEL,
        messages: &messages,
        tools: &tools,
        options: &options,
        credential: Some(&credential),
        reasoning_effort: ReasoningEffort::None,
        provider_order: None,
    };

    let response = api.complete(request).await.unwrap();
    let calls: Vec<_> = response.tool_calls().collect();
    assert!(
        !calls.is_empty(),
        "expected a tool call for the weather query"
    );
    assert_eq!(calls[0].name, "get_weather");
}

#[tokio::test]
async fn live_function_call_round_trip() {
    let Some(key) = api_key() else {
        eprintln!("skipping live_function_call_round_trip: no GEMINI_API_KEY/GOOGLE_API_KEY set");
        return;
    };

    let api = InteractionsApi::new(BASE_URL, Arc::new(HttpClient::new()));
    let credential = Credential::Header("X-goog-api-key".to_string(), key);
    let tools = [ToolSpec::Function(ToolDefinition {
        name: "get_weather".into(),
        description: "Get the current weather for a city".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "city": {"type": "string", "description": "City name"}
            },
            "required": ["city"]
        }),
    })];
    let options = RequestOptions::default();

    // First turn: ask for the weather so the model issues a tool call.
    let first_messages = [Message::user("What is the weather in Paris? Use the tool.")];
    let first_request = LlmRequest {
        model_id: MODEL,
        messages: &first_messages,
        tools: &tools,
        options: &options,
        credential: Some(&credential),
        reasoning_effort: ReasoningEffort::None,
        provider_order: None,
    };
    let first_response = api.complete(first_request).await.unwrap();
    let calls: Vec<_> = first_response.tool_calls().cloned().collect();
    assert!(
        !calls.is_empty(),
        "expected a tool call for the weather query"
    );

    // Second turn: echo the assistant message (with its tool calls) back and
    // append a dummy tool result for each call.
    let mut messages: Vec<Message> = vec![first_messages[0].clone()];
    let mut tool_calls = Vec::new();
    for call in &calls {
        tool_calls.push(call.clone());
    }
    messages.push(Message::assistant_with_tool_calls_and_reasoning(
        (!first_response.text().trim().is_empty()).then_some(first_response.text()),
        tool_calls.clone(),
        first_response.reasoning.clone(),
        first_response.reasoning_details.clone(),
    ));
    for call in &tool_calls {
        messages.push(Message::tool_result(
            r#"{"temperature_c": 18, "condition": "cloudy"}"#,
            call.id.clone(),
        ));
    }
    messages.push(Message::user(
        "Using the tool result above, answer in one short sentence.",
    ));

    let second_request = LlmRequest {
        model_id: MODEL,
        messages: &messages,
        tools: &tools,
        options: &options,
        credential: Some(&credential),
        reasoning_effort: ReasoningEffort::None,
        provider_order: None,
    };
    let second_response = api.complete(second_request).await.unwrap();
    assert!(
        !second_response.text().trim().is_empty(),
        "expected a final text answer after the tool result round trip"
    );
}
