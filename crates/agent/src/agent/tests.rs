use super::*;
use crate::AgentTool;
use crate::session::{SessionManager, SessionRecord};
use async_trait::async_trait;
use llm::{ContentBlock, LlmApi, LlmError, LlmEvent, LlmResponse, StopReason};
use providers::{ApiId, ModelCapabilities, ModelInfo, OpenRouterProvider, Provider, ProviderId};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct FakeApi;

#[async_trait]
impl LlmApi for FakeApi {
    async fn stream(&self, request: llm::LlmRequest<'_>) -> Result<llm::LlmStream, LlmError> {
        let user = request
            .messages
            .iter()
            .rev()
            .find_map(|message| message.content.clone())
            .unwrap_or_default();
        let response = LlmResponse {
            content: vec![ContentBlock::Text(format!("echo: {user}"))],
            stop_reason: StopReason::Stop,
            usage: None,
            model: Some(request.model_id.to_owned()),
            reasoning: None,
            reasoning_details: Vec::new(),
        };
        let text = response.text();
        let model = response.model.clone();
        Ok(Box::pin(futures_util::stream::iter([
            Ok(llm::LlmEvent::TextDelta { text }),
            Ok(llm::LlmEvent::Done {
                stop_reason: StopReason::Stop,
                usage: None,
                model,
            }),
        ])))
    }
}

struct FailAfterFirstApi {
    calls: AtomicUsize,
}

struct PendingAfterFirstApi {
    calls: AtomicUsize,
}

#[async_trait]
impl LlmApi for FailAfterFirstApi {
    async fn stream(&self, request: llm::LlmRequest<'_>) -> Result<llm::LlmStream, LlmError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            return Ok(Box::pin(futures_util::stream::iter([
                Ok(LlmEvent::TextDelta {
                    text: "first response".into(),
                }),
                Ok(LlmEvent::Done {
                    stop_reason: StopReason::Stop,
                    usage: None,
                    model: Some(request.model_id.to_owned()),
                }),
            ])));
        }

        Ok(Box::pin(futures_util::stream::iter([
            Ok(LlmEvent::TextDelta {
                text: "partial response".into(),
            }),
            Err(LlmError::InvalidResponse(
                "stream ended without [DONE]".into(),
            )),
        ])))
    }
}

#[async_trait]
impl LlmApi for PendingAfterFirstApi {
    async fn stream(&self, request: llm::LlmRequest<'_>) -> Result<llm::LlmStream, LlmError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            return Ok(Box::pin(futures_util::stream::iter([
                Ok(LlmEvent::TextDelta {
                    text: "first response".into(),
                }),
                Ok(LlmEvent::Done {
                    stop_reason: StopReason::Stop,
                    usage: None,
                    model: Some(request.model_id.to_owned()),
                }),
            ])));
        }

        Ok(Box::pin(futures_util::stream::unfold(
            0,
            |state| async move {
                match state {
                    0 => Some((
                        Ok(LlmEvent::TextDelta {
                            text: "partial response".into(),
                        }),
                        1,
                    )),
                    _ => futures_util::future::pending().await,
                }
            },
        )))
    }
}

fn model_with_api(api: Arc<dyn LlmApi>) -> Model {
    let info = ModelInfo {
        provider: ProviderId::new("openrouter"),
        id: "test".into(),
        name: "Test".into(),
        api: ApiId::ChatCompletions,
        capabilities: ModelCapabilities::default(),
        pricing: None,
    };
    OpenRouterProvider::builder("key")
        .with_models([info])
        .with_api(api)
        .build()
        .unwrap()
        .bind("test")
        .unwrap()
}

fn model() -> Model {
    model_with_api(Arc::new(FakeApi))
}

// ---------------------------------------------------------------------------
// Helper: build an Arc<Agent> with the given model and optional setup
// ---------------------------------------------------------------------------

fn agent(model: Model) -> Arc<Agent> {
    Arc::new(Agent::builder(model).build().unwrap())
}

fn agent_with_manager(model: Model, manager: Arc<SessionManager>) -> Arc<Agent> {
    Arc::new(
        Agent::builder(model)
            .session_manager(manager)
            .build()
            .unwrap(),
    )
}

// ---------------------------------------------------------------------------
// PromptBuilder API tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn prompt_owns_history_and_system_prompt() {
    let a = Arc::new(
        Agent::builder(model())
            .system_prompt("Be helpful")
            .build()
            .unwrap(),
    );
    let response = a
        .ask(a.prompt().content("hello"))
        .unwrap()
        .into_response()
        .await
        .unwrap();
    assert_eq!(response.text(), "echo: hello");
    assert_eq!(a.messages().await.len(), 2);
}

#[tokio::test]
async fn stream_false_suppresses_text_deltas() {
    let a = Arc::new(
        Agent::builder(model())
            .system_prompt("Be helpful")
            .build()
            .unwrap(),
    );
    let mut rx = a.ask(a.prompt().content("hello").stream(false)).unwrap();

    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event.unwrap());
    }

    // stream=false suppresses TextDelta; only Finished should arrive.
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], AgentEvent::Finished { .. }));
    assert_eq!(a.messages().await.len(), 2);
}

#[tokio::test]
async fn stream_true_emits_text_deltas_and_finished() {
    let a = Arc::new(
        Agent::builder(model())
            .system_prompt("Be helpful")
            .build()
            .unwrap(),
    );
    let mut rx = a.ask(a.prompt().content("hello").stream(true)).unwrap();

    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event.unwrap());
    }

    assert!(matches!(
        &events[0],
        AgentEvent::TextDelta(text) if text == "echo: hello"
    ));
    assert!(matches!(events.last(), Some(AgentEvent::Finished { .. })));
    assert_eq!(a.messages().await.len(), 2);
}

#[tokio::test]
async fn empty_prompt_content_returns_error() {
    let a = agent(model());
    let result = a.ask(a.prompt().content("   "));
    assert!(result.is_err(), "must reject blank prompt");
}

#[tokio::test]
async fn missing_content_returns_error() {
    let a = agent(model());
    let result = a.ask(a.prompt());
    assert!(result.is_err(), "must reject builder with no content");
}

#[tokio::test]
async fn into_response_extracts_final_response() {
    let a = Arc::new(
        Agent::builder(model())
            .system_prompt("Be helpful")
            .build()
            .unwrap(),
    );
    let stream = a.ask(a.prompt().content("hello").stream(true)).unwrap();
    let response = stream.into_response().await.unwrap();
    assert_eq!(response.text(), "echo: hello");
    assert_eq!(a.messages().await.len(), 2);
}

#[tokio::test]
async fn images_are_passed_through_builder() {
    let api = Arc::new(ImageInspectApi);
    let a = Arc::new(Agent::builder(model_with_api(api)).build().unwrap());

    let images = vec![llm::ImageUrl {
        url: "data:image/png;base64,abc123".into(),
    }];
    let response = a
        .ask(a.prompt().content("describe this").images(images))
        .unwrap()
        .into_response()
        .await
        .unwrap();
    assert_eq!(response.text(), "text=true,images=1");
}

// ---------------------------------------------------------------------------
// Reasoning streaming
// ---------------------------------------------------------------------------

struct ReasoningApi;

#[async_trait]
impl LlmApi for ReasoningApi {
    async fn stream(&self, _request: llm::LlmRequest<'_>) -> Result<llm::LlmStream, LlmError> {
        Ok(Box::pin(futures_util::stream::iter([
            Ok(llm::LlmEvent::ReasoningDelta {
                reasoning: "thinking...".into(),
                details: vec![serde_json::json!({
                    "type": "reasoning.text",
                    "text": "thinking..."
                })],
            }),
            Ok(llm::LlmEvent::TextDelta {
                text: "done thinking".into(),
            }),
            Ok(llm::LlmEvent::Done {
                stop_reason: StopReason::Stop,
                usage: None,
                model: Some("reasoning-model".into()),
            }),
        ])))
    }
}

#[tokio::test]
async fn stream_true_emits_reasoning_and_text_deltas() {
    let a = Arc::new(
        Agent::builder(model_with_api(Arc::new(ReasoningApi)))
            .build()
            .unwrap(),
    );
    let mut rx = a.ask(a.prompt().content("hello").stream(true)).unwrap();

    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event.unwrap());
    }

    assert!(matches!(
        &events[0],
        AgentEvent::ReasoningDelta(text) if text == "thinking..."
    ));
    assert!(matches!(
        &events[1],
        AgentEvent::TextDelta(text) if text == "done thinking"
    ));
    assert!(matches!(events.last(), Some(AgentEvent::Finished { .. })));

    let messages = a.messages().await;
    assert_eq!(messages.len(), 2);
    if let AgentMessage::Assistant(resp) = &messages[1] {
        assert_eq!(resp.reasoning.as_deref(), Some("thinking..."));
        assert_eq!(resp.text(), "done thinking");
    } else {
        panic!("expected assistant message");
    }
}

#[tokio::test]
async fn stream_false_suppresses_reasoning_deltas() {
    let a = Arc::new(
        Agent::builder(model_with_api(Arc::new(ReasoningApi)))
            .build()
            .unwrap(),
    );
    let mut rx = a.ask(a.prompt().content("hello").stream(false)).unwrap();

    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event.unwrap());
    }

    // ReasoningDelta should be suppressed.
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], AgentEvent::Finished { .. }));
}

// ---------------------------------------------------------------------------
// Error recovery
// ---------------------------------------------------------------------------

#[tokio::test]
async fn streaming_error_preserves_previous_history() {
    let api = Arc::new(FailAfterFirstApi {
        calls: AtomicUsize::new(0),
    });
    let a = Arc::new(Agent::builder(model_with_api(api)).build().unwrap());

    a.ask(a.prompt().content("first"))
        .unwrap()
        .into_response()
        .await
        .unwrap();

    let mut stream = a.ask(a.prompt().content("second").stream(true)).unwrap();
    let mut error = None;
    while let Some(event) = stream.recv().await {
        if let Err(error_event) = event {
            error = Some(error_event);
            break;
        }
    }

    assert_eq!(
        error.unwrap().to_string(),
        "invalid response: stream ended without [DONE]"
    );
    let messages = a.messages().await;
    assert_eq!(messages.len(), 4);
    assert!(matches!(&messages[0], AgentMessage::User { text, .. } if text == "first"));
    assert!(
        matches!(&messages[1], AgentMessage::Assistant(response) if response.text() == "first response")
    );
    assert!(matches!(&messages[2], AgentMessage::User { text, .. } if text == "second"));
    assert!(
        matches!(&messages[3], AgentMessage::Assistant(response) if response.text() == "partial response")
    );
}

#[tokio::test]
async fn abort_preserves_partial_assistant_output() {
    let api = Arc::new(PendingAfterFirstApi {
        calls: AtomicUsize::new(0),
    });
    let a = Arc::new(Agent::builder(model_with_api(api)).build().unwrap());

    a.ask(a.prompt().content("first"))
        .unwrap()
        .into_response()
        .await
        .unwrap();

    let mut stream = a.ask(a.prompt().content("second").stream(true)).unwrap();
    assert!(matches!(
        stream.recv().await,
        Some(Ok(AgentEvent::TextDelta(text))) if text == "partial response"
    ));
    stream.abort();
    assert!(matches!(
        stream.recv().await,
        Some(Err(AgentError::Aborted))
    ));

    let messages = a.messages().await;
    assert_eq!(messages.len(), 4);
    assert!(matches!(&messages[2], AgentMessage::User { text, .. } if text == "second"));
    assert!(
        matches!(&messages[3], AgentMessage::Assistant(response) if response.text() == "partial response")
    );
}

// ---------------------------------------------------------------------------
// Tool calling
// ---------------------------------------------------------------------------

struct ToolCallingApi {
    calls: AtomicUsize,
}

#[async_trait]
impl LlmApi for ToolCallingApi {
    async fn stream(&self, request: llm::LlmRequest<'_>) -> Result<llm::LlmStream, LlmError> {
        let call_count = self.calls.fetch_add(1, Ordering::SeqCst);
        if call_count == 0 {
            let response = LlmResponse {
                content: vec![],
                stop_reason: StopReason::ToolUse,
                usage: Some(llm::Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    ..llm::Usage::default()
                }),
                model: Some(request.model_id.to_owned()),
                reasoning: None,
                reasoning_details: Vec::new(),
            };
            Ok(Box::pin(futures_util::stream::iter([
                Ok(LlmEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call-1".into()),
                    name: Some("bash".into()),
                    arguments: serde_json::json!({"command": "echo hi"}).to_string(),
                }),
                Ok(LlmEvent::Done {
                    stop_reason: StopReason::ToolUse,
                    usage: response.usage.clone(),
                    model: response.model.clone(),
                }),
            ])))
        } else {
            let response = LlmResponse {
                content: vec![ContentBlock::Text("done".into())],
                stop_reason: StopReason::Stop,
                usage: Some(llm::Usage {
                    input_tokens: 20,
                    output_tokens: 3,
                    ..llm::Usage::default()
                }),
                model: Some(request.model_id.to_owned()),
                reasoning: None,
                reasoning_details: Vec::new(),
            };
            let text = response.text();
            let model = response.model.clone();
            Ok(Box::pin(futures_util::stream::iter([
                Ok(LlmEvent::TextDelta { text }),
                Ok(LlmEvent::Done {
                    stop_reason: StopReason::Stop,
                    usage: response.usage.clone(),
                    model,
                }),
            ])))
        }
    }
}

// ---------------------------------------------------------------------------
// Session persistence
// ---------------------------------------------------------------------------

#[tokio::test]
async fn building_agent_with_manager_creates_no_file() {
    let root = std::env::temp_dir().join(format!("alan-plan3-build-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let manager = Arc::new(SessionManager::new(&root));
    let _a = Arc::new(
        Agent::builder(model())
            .session_manager(manager)
            .build()
            .unwrap(),
    );

    // No session file should exist before any prompt.
    let entries: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert!(
        entries.is_empty(),
        "building must not create a session file"
    );
}

#[tokio::test]
async fn first_prompt_creates_session_and_persists_messages() {
    let root = std::env::temp_dir().join(format!("alan-plan3-buffered-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let manager = Arc::new(SessionManager::new(&root));
    let a = agent_with_manager(model(), manager.clone());

    let response = a
        .ask(a.prompt().content("hello"))
        .unwrap()
        .into_response()
        .await
        .unwrap();
    assert_eq!(response.text(), "echo: hello");

    let entries: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(entries.len(), 1, "one pwd directory created");
    let pwd_dir = entries[0].path();
    let files: Vec<_> = std::fs::read_dir(&pwd_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(files.len(), 1, "one session file created");

    let session_file = files[0].path();
    let content = std::fs::read_to_string(&session_file).unwrap();
    let lines: Vec<_> = content.lines().collect();
    assert_eq!(lines.len(), 3, "header + user + assistant");

    let header: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(header["type"], "session");

    let user_record: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(user_record["type"], "message");
    assert_eq!(user_record["message"]["kind"], "user");
    assert_eq!(user_record["message"]["content"], "hello");

    let assistant_record: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
    assert_eq!(assistant_record["type"], "message");
    assert_eq!(assistant_record["message"]["kind"], "assistant");
}

#[tokio::test]
async fn provider_is_not_called_if_session_creation_fails() {
    let _root =
        std::env::temp_dir().join(format!("alan-plan3-fail-create-{}", uuid::Uuid::new_v4()));
    let manager = Arc::new(SessionManager::new("/proc/self/mem/unwritable-dir"));
    let a = agent_with_manager(model(), manager);

    let result = a
        .ask(a.prompt().content("hello"))
        .unwrap()
        .into_response()
        .await;
    assert!(result.is_err(), "must fail when session creation fails");
}

#[tokio::test]
async fn tool_call_responses_and_results_are_persisted_in_order() {
    let root = std::env::temp_dir().join(format!("alan-plan3-tool-order-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let manager = Arc::new(SessionManager::new(&root));
    let api = Arc::new(ToolCallingApi {
        calls: AtomicUsize::new(0),
    });
    let a = Arc::new(
        Agent::builder(model_with_api(api))
            .session_manager(manager.clone())
            .with_tools([AgentTool::new(
                llm::ToolDefinition {
                    name: "bash".into(),
                    description: "Run a shell command".into(),
                    parameters: serde_json::json!({}),
                },
                tools::BashExecutor,
            )])
            .build()
            .unwrap(),
    );

    let response = a
        .ask(a.prompt().content("run echo hi"))
        .unwrap()
        .into_response()
        .await
        .unwrap();
    assert_eq!(response.text(), "done");

    let content = {
        let entries: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        let pwd_dir = entries[0].path();
        let files: Vec<_> = std::fs::read_dir(&pwd_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        std::fs::read_to_string(files[0].path()).unwrap()
    };
    let lines: Vec<_> = content.lines().collect();
    assert!(
        lines.len() >= 5,
        "expected at least 5 records, got {}",
        lines.len()
    );

    let types: Vec<_> = lines
        .iter()
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            v["type"].as_str().unwrap().to_owned()
        })
        .collect();
    assert_eq!(types[0], "session");
    assert_eq!(types[1], "message");
    assert_eq!(types[types.len() - 1], "message");
}

#[tokio::test]
async fn aggregate_usage_from_multiple_rounds_is_persisted() {
    let root = std::env::temp_dir().join(format!("alan-plan3-usage-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let manager = Arc::new(SessionManager::new(&root));
    let api = Arc::new(ToolCallingApi {
        calls: AtomicUsize::new(0),
    });
    let a = Arc::new(
        Agent::builder(model_with_api(api))
            .session_manager(manager.clone())
            .with_tools([AgentTool::new(
                llm::ToolDefinition {
                    name: "bash".into(),
                    description: "Run a shell command".into(),
                    parameters: serde_json::json!({}),
                },
                tools::BashExecutor,
            )])
            .build()
            .unwrap(),
    );

    let response = a
        .ask(a.prompt().content("run echo hi"))
        .unwrap()
        .into_response()
        .await
        .unwrap();
    assert_eq!(response.text(), "done");

    let entries: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    let pwd_dir = entries[0].path();
    let files: Vec<_> = std::fs::read_dir(&pwd_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    let session_file = files[0].path();

    let session = {
        let content = std::fs::read_to_string(&session_file).unwrap();
        let lines: Vec<_> = content.lines().collect();
        let header_line = lines[0];
        let record = SessionRecord::parse(header_line).unwrap();
        let SessionRecord::Session { id, .. } = record else {
            panic!("expected session header");
        };
        manager
            .get_session(&id, &std::env::current_dir().unwrap())
            .await
            .expect("load session for usage check")
    };

    // First round: 10 input + 5 output. Second round: 20 input + 3 output.
    assert_eq!(session.usage.input_tokens, 30);
    assert_eq!(session.usage.output_tokens, 8);
}

#[tokio::test]
async fn resumed_agent_includes_restored_messages_in_first_request() {
    let root = std::env::temp_dir().join(format!("alan-plan3-resume-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let manager = Arc::new(SessionManager::new(&root));

    let session = manager
        .create(&root, "openrouter", "test", None)
        .await
        .expect("create session");
    manager
        .append_message(
            &session.id,
            &session.pwd,
            &AgentMessage::user("restored message"),
        )
        .await
        .expect("append message");
    let session = manager
        .get_session(&session.id, &root)
        .await
        .expect("load session with restored message");

    let m = model();
    let a = Arc::new(
        Agent::builder(m)
            .session_manager(manager.clone())
            .resume_session(session)
            .build()
            .expect("build with resume"),
    );

    let response = a
        .ask(a.prompt().content("new message"))
        .unwrap()
        .into_response()
        .await
        .unwrap();
    assert_eq!(response.text(), "echo: new message");

    let messages = a.messages().await;
    assert!(
        messages.len() >= 2,
        "must include restored user + new user + assistant response, got {} messages",
        messages.len()
    );
    assert!(
        matches!(&messages[0], AgentMessage::User { text, .. } if text == "restored message"),
        "first message must be the restored message, got {:?}",
        messages[0]
    );
}

#[tokio::test]
async fn request_uses_persisted_session_id_and_cache_key() {
    let root = std::env::temp_dir().join(format!("alan-plan3-session-id-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let manager = Arc::new(SessionManager::new(&root));
    let a = agent_with_manager(model(), manager);

    a.ask(a.prompt().content("check session id"))
        .unwrap()
        .into_response()
        .await
        .unwrap();

    let entries: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    let pwd_dir = entries[0].path();
    let files: Vec<_> = std::fs::read_dir(&pwd_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    let content = std::fs::read_to_string(files[0].path()).unwrap();
    let lines: Vec<_> = content.lines().collect();

    let header: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    let session_id = header["id"].as_str().unwrap();
    let session_file_name = files[0].path().file_stem().unwrap().to_owned();
    assert_eq!(session_file_name, session_id);
}

// ---------------------------------------------------------------------------
// Image tests
// ---------------------------------------------------------------------------

struct ImageInspectApi;

#[async_trait]
impl LlmApi for ImageInspectApi {
    async fn stream(&self, request: llm::LlmRequest<'_>) -> Result<llm::LlmStream, LlmError> {
        let parts_summary = request
            .messages
            .iter()
            .rev()
            .find_map(|m| m.content_parts.as_ref())
            .map(|parts| {
                let mut has_text = false;
                let mut image_count = 0;
                for part in parts {
                    match part {
                        llm::ContentPart::Text { .. } => has_text = true,
                        llm::ContentPart::Image { .. } => image_count += 1,
                    }
                }
                format!("text={has_text},images={image_count}")
            })
            .unwrap_or_default();

        let response = LlmResponse {
            content: vec![ContentBlock::Text(parts_summary.clone())],
            stop_reason: StopReason::Stop,
            usage: None,
            model: Some(request.model_id.to_owned()),
            reasoning: None,
            reasoning_details: Vec::new(),
        };
        let text = response.text();
        let model = response.model.clone();
        Ok(Box::pin(futures_util::stream::iter([
            Ok(llm::LlmEvent::TextDelta { text }),
            Ok(llm::LlmEvent::Done {
                stop_reason: StopReason::Stop,
                usage: None,
                model,
            }),
        ])))
    }
}

#[tokio::test]
async fn prompt_with_images_sends_content_parts() {
    let api = Arc::new(ImageInspectApi);
    let a = Arc::new(Agent::builder(model_with_api(api)).build().unwrap());

    let images = vec![llm::ImageUrl {
        url: "data:image/png;base64,abc123".into(),
    }];
    let response = a
        .ask(a.prompt().content("describe this").images(images))
        .unwrap()
        .into_response()
        .await
        .unwrap();
    assert_eq!(response.text(), "text=true,images=1");
}

#[tokio::test]
async fn prompt_with_images_persists_images_in_session() {
    let root =
        std::env::temp_dir().join(format!("alan-plan5-img-persist-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let manager = Arc::new(SessionManager::new(&root));
    let api = Arc::new(ImageInspectApi);
    let a = Arc::new(
        Agent::builder(model_with_api(api))
            .session_manager(manager.clone())
            .build()
            .unwrap(),
    );

    let images = vec![
        llm::ImageUrl {
            url: "data:image/png;base64,abc".into(),
        },
        llm::ImageUrl {
            url: "https://example.com/photo.jpg".into(),
        },
    ];
    let _response = a
        .ask(a.prompt().content("describe these").images(images.clone()))
        .unwrap()
        .into_response()
        .await
        .unwrap();

    let entries: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    let pwd_dir = entries[0].path();
    let files: Vec<_> = std::fs::read_dir(pwd_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    let content = std::fs::read_to_string(files[0].path()).unwrap();
    let lines: Vec<_> = content.lines().collect();

    let user_record: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    let msg = &user_record["message"];
    assert_eq!(msg["kind"], "user");
    assert_eq!(msg["content"], "describe these");
    let persisted_images = msg["images"].as_array().unwrap();
    assert_eq!(persisted_images.len(), 2);
    assert_eq!(persisted_images[0]["url"], "data:image/png;base64,abc");
    assert_eq!(persisted_images[1]["url"], "https://example.com/photo.jpg");
}

#[tokio::test]
async fn session_images_roundtrip_through_reload() {
    let root =
        std::env::temp_dir().join(format!("alan-plan5-img-roundtrip-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let manager = Arc::new(SessionManager::new(&root));

    let session = manager
        .create(&root, "openrouter", "test", None)
        .await
        .expect("create session");
    let images = vec![llm::ImageUrl {
        url: "https://example.com/pic.png".into(),
    }];
    manager
        .append_message(
            &session.id,
            &session.pwd,
            &AgentMessage::user_with_images("look at this", images),
        )
        .await
        .expect("append image message");

    let loaded = manager
        .get_session(&session.id, &root)
        .await
        .expect("reload session");
    assert_eq!(loaded.messages.len(), 1);
    match &loaded.messages[0] {
        AgentMessage::User { text, images } => {
            assert_eq!(text, "look at this");
            assert_eq!(images.len(), 1);
            assert_eq!(images[0].url, "https://example.com/pic.png");
        }
        other => panic!("expected User message, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Message / serialization tests
// ---------------------------------------------------------------------------

#[test]
fn legacy_session_without_images_field_loads() {
    let legacy = r#"{"kind":"user","content":"hello"}"#;
    let msg: AgentMessage = serde_json::from_str(legacy).expect("legacy format deserializes");
    match msg {
        AgentMessage::User { text, images } => {
            assert_eq!(text, "hello");
            assert!(images.is_empty());
        }
        other => panic!("expected User, got {other:?}"),
    }
}

#[test]
fn legacy_tool_result_without_content_parts_loads() {
    let legacy = r#"{"kind":"tool_result","tool_call_id":"call-1","content":"output"}"#;
    let msg: AgentMessage = serde_json::from_str(legacy).expect("legacy format deserializes");
    match msg {
        AgentMessage::ToolResult {
            tool_call_id,
            content,
            content_parts,
        } => {
            assert_eq!(tool_call_id, "call-1");
            assert_eq!(content, "output");
            assert!(content_parts.is_empty());
        }
        other => panic!("expected ToolResult, got {other:?}"),
    }
}

#[test]
fn to_llm_user_with_no_images_is_plain_user_message() {
    let msg = AgentMessage::user("hello");
    let llm_msg = msg.to_llm();
    assert_eq!(llm_msg.role, llm::Role::User);
    assert_eq!(llm_msg.content.as_deref(), Some("hello"));
    assert!(llm_msg.content_parts.is_none());
}

#[test]
fn to_llm_user_with_images_uses_content_parts() {
    let msg = AgentMessage::user_with_images(
        "describe",
        vec![llm::ImageUrl {
            url: "https://example.com/img.png".into(),
        }],
    );
    let llm_msg = msg.to_llm();
    assert_eq!(llm_msg.role, llm::Role::User);
    assert!(llm_msg.content.is_none());
    let parts = llm_msg.content_parts.unwrap();
    assert_eq!(parts.len(), 2);
    assert!(matches!(&parts[0], llm::ContentPart::Text { text } if text == "describe"));
    assert!(
        matches!(&parts[1], llm::ContentPart::Image { image_url } if image_url.url == "https://example.com/img.png")
    );
}

#[test]
fn user_constructor_has_empty_images() {
    let msg = AgentMessage::user("text");
    match msg {
        AgentMessage::User { text, images } => {
            assert_eq!(text, "text");
            assert!(images.is_empty());
        }
        _ => panic!("expected User"),
    }
}

#[test]
fn to_llm_tool_result_with_parts_uses_content_parts() {
    use llm::ContentPart;
    let msg = AgentMessage::tool_result_with_parts(
        "call-1",
        "[image/png image, 100 bytes base64]",
        vec![ContentPart::Image {
            image_url: llm::ImageUrl {
                url: "data:image/png;base64,iVBOR".into(),
            },
        }],
    );
    let llm_msg = msg.to_llm();
    assert_eq!(llm_msg.role, llm::Role::Tool);
    assert_eq!(llm_msg.tool_call_id.as_deref(), Some("call-1"));
    assert!(llm_msg.content.is_none());
    let parts = llm_msg.content_parts.unwrap();
    assert_eq!(parts.len(), 1);
    assert!(
        matches!(&parts[0], ContentPart::Image { image_url } if image_url.url == "data:image/png;base64,iVBOR")
    );
}

#[test]
fn to_llm_tool_result_without_parts_uses_plain_content() {
    let msg = AgentMessage::ToolResult {
        tool_call_id: "call-2".into(),
        content: "all good".into(),
        content_parts: vec![],
    };
    let llm_msg = msg.to_llm();
    assert_eq!(llm_msg.role, llm::Role::Tool);
    assert_eq!(llm_msg.tool_call_id.as_deref(), Some("call-2"));
    assert_eq!(llm_msg.content.as_deref(), Some("all good"));
    assert!(llm_msg.content_parts.is_none());
}
