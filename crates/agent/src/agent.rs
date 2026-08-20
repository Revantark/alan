use crate::{AgentError, AgentMessage, AgentTool, Skill, build_system_prompt};
use futures_util::StreamExt;
use llm::{
    CompletionInput, LlmEvent, LlmResponse, LlmResponseBuilder, Message, RequestOptions, ToolSpec,
};
use providers::{Model, ModelError};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::{
    Mutex,
    mpsc::{self, Receiver, Sender},
    watch,
};

const AGENT_EVENT_CAPACITY: usize = 128;

/// Receiver for display-level events emitted by one agent prompt.
pub struct AgentStream {
    receiver: Receiver<Result<AgentEvent, AgentError>>,
    cancellation: watch::Sender<bool>,
}

impl AgentStream {
    pub async fn recv(&mut self) -> Option<Result<AgentEvent, AgentError>> {
        self.receiver.recv().await
    }

    pub fn try_recv(
        &mut self,
    ) -> Result<Result<AgentEvent, AgentError>, mpsc::error::TryRecvError> {
        self.receiver.try_recv()
    }

    pub fn abort(&self) {
        let _ = self.cancellation.send(true);
    }
}

impl Drop for AgentStream {
    fn drop(&mut self) {
        let _ = self.cancellation.send(true);
    }
}

/// Display-level events emitted while an agent prompt runs.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    TextDelta(String),
    ReasoningDelta(String),
    ToolCallStarted {
        id: String,
        name: String,
        arguments: String,
    },
    ToolCallFinished {
        id: String,
        output: String,
    },
    ToolCallFailed {
        id: String,
        error: String,
    },
    Finished,
}

pub struct AgentContext {
    pub system_prompt: Option<String>,
    pub skills: Vec<Skill>,
    pub messages: Vec<AgentMessage>,
    tools: Vec<AgentTool>,
    tool_indexes: HashMap<String, usize>,
}

impl AgentContext {
    fn new(system_prompt: Option<String>, skills: Vec<Skill>, tools: Vec<AgentTool>) -> Self {
        let mut tool_indexes = HashMap::with_capacity(tools.len());
        for (index, tool) in tools.iter().enumerate() {
            tool_indexes
                .entry(tool.definition.name.clone())
                .or_insert(index);
        }

        Self {
            system_prompt,
            skills,
            messages: Vec::new(),
            tools,
            tool_indexes,
        }
    }
}

pub struct Agent {
    model: Mutex<Model>,
    context: Mutex<AgentContext>,
    plan_mode: AtomicBool,
    max_tool_rounds: usize,
    session_id: String,
}

impl Agent {
    pub fn builder(model: Model) -> AgentBuilder {
        AgentBuilder {
            model,
            system_prompt: None,
            skills: Vec::new(),
            tools: Vec::new(),
            max_tool_rounds: 100,
        }
    }

    /// Buffered prompt: runs to completion and returns the final response.
    pub async fn prompt(&self, content: impl Into<String>) -> Result<LlmResponse, AgentError> {
        let model = self.model.lock().await;
        let mut context = self.context.lock().await;
        let plan_mode = self.plan_mode();
        context
            .messages
            .push(AgentMessage::user(prompt_content(content, plan_mode)));

        let (_cancellation, mut cancellation_receiver) = watch::channel(false);
        let mut partial = String::new();
        let result = self
            .run_with(
                &model,
                &mut context,
                None,
                &mut cancellation_receiver,
                &mut partial,
            )
            .await;
        if result.is_err() && !partial.is_empty() {
            context
                .messages
                .push(AgentMessage::Assistant(partial_response(&partial)));
        }
        result
    }

    /// Streaming prompt: returns a channel of incremental events.
    ///
    /// The agent runs in a background task. Tool calls execute internally
    /// without surfacing provider-level fragments to the caller.
    pub fn prompt_stream(self: &Arc<Self>, content: impl Into<String>) -> AgentStream {
        let (tx, receiver) = mpsc::channel(AGENT_EVENT_CAPACITY);
        let (cancellation, cancellation_receiver) = watch::channel(false);
        let agent = self.clone();
        let content = content.into();
        tokio::spawn(async move {
            let result = agent.run_prompt(content, &tx, cancellation_receiver).await;
            if let Err(error) = result {
                let _ = tx.send(Err(error)).await;
            }
        });
        AgentStream {
            receiver,
            cancellation,
        }
    }

    pub async fn set_model(&self, model: Model) {
        *self.model.lock().await = model;
    }

    pub fn set_plan_mode(&self, enabled: bool) {
        self.plan_mode.store(enabled, Ordering::Release);
    }

    pub fn plan_mode(&self) -> bool {
        self.plan_mode.load(Ordering::Acquire)
    }

    pub async fn messages(&self) -> Vec<AgentMessage> {
        self.context.lock().await.messages.clone()
    }

    async fn run_prompt(
        self: Arc<Self>,
        content: String,
        events: &Sender<Result<AgentEvent, AgentError>>,
        mut cancellation: watch::Receiver<bool>,
    ) -> Result<LlmResponse, AgentError> {
        let model = self.model.lock().await;
        let mut context = self.context.lock().await;
        let plan_mode = self.plan_mode();
        context
            .messages
            .push(AgentMessage::user(prompt_content(content, plan_mode)));

        let mut partial = String::new();
        let result = self
            .run_with(
                &model,
                &mut context,
                Some(events),
                &mut cancellation,
                &mut partial,
            )
            .await;
        if result.is_err() && !partial.is_empty() {
            context
                .messages
                .push(AgentMessage::Assistant(partial_response(&partial)));
        }
        result
    }

    async fn run_with(
        &self,
        model: &Model,
        context: &mut AgentContext,
        events: Option<&Sender<Result<AgentEvent, AgentError>>>,
        cancellation: &mut watch::Receiver<bool>,
        partial: &mut String,
    ) -> Result<LlmResponse, AgentError> {
        let plan = self.plan_mode();
        for _ in 0..self.max_tool_rounds {
            Self::check_cancelled(cancellation)?;
            let response = Self::stream_round(
                self.session_id.clone(),
                model,
                context,
                events,
                cancellation,
                partial,
                plan,
            )
            .await?;
            let calls: Vec<_> = response.tool_calls().cloned().collect();
            if calls.is_empty() {
                if let Some(events) = events {
                    Self::send_event(events, Ok(AgentEvent::Finished), cancellation).await?;
                }
                context
                    .messages
                    .push(AgentMessage::Assistant(response.clone()));
                return Ok(response);
            }
            context.messages.push(AgentMessage::Assistant(response));
            for call in calls {
                Self::check_cancelled(cancellation)?;
                let tool_index = context
                    .tool_indexes
                    .get(&call.name)
                    .copied()
                    .ok_or_else(|| AgentError::ToolNotFound(call.name.clone()))?;
                if plan && !context.tools[tool_index].read_only && call.name != "bash" {
                    return Err(AgentError::ToolNotFound(call.name.clone()));
                }
                let call_id = call.id.clone();
                if let Some(events) = events {
                    Self::send_event(
                        events,
                        Ok(AgentEvent::ToolCallStarted {
                            id: call_id.clone(),
                            name: call.name.clone(),
                            arguments: call.arguments.clone(),
                        }),
                        cancellation,
                    )
                    .await?;
                }

                let result = context.tools[tool_index].executor.execute(&call).await;
                match result {
                    Ok(result) => {
                        context.messages.push(AgentMessage::ToolResult {
                            tool_call_id: call_id.clone(),
                            content: result.clone(),
                        });
                        if let Some(events) = events {
                            Self::send_event(
                                events,
                                Ok(AgentEvent::ToolCallFinished {
                                    id: call_id,
                                    output: tail_lines(&result, 5),
                                }),
                                cancellation,
                            )
                            .await?;
                        }
                        Self::check_cancelled(cancellation)?;
                    }
                    Err(error) => {
                        let error = error.to_string();
                        context.messages.push(AgentMessage::ToolResult {
                            tool_call_id: call_id.clone(),
                            content: error.clone(),
                        });
                        if let Some(events) = events {
                            Self::send_event(
                                events,
                                Ok(AgentEvent::ToolCallFailed { id: call_id, error }),
                                cancellation,
                            )
                            .await?;
                        }
                        Self::check_cancelled(cancellation)?;
                    }
                }
            }
        }
        Err(AgentError::MaxToolRounds)
    }

    async fn stream_round(
        session_id: String,
        model: &Model,
        context: &AgentContext,
        events: Option<&Sender<Result<AgentEvent, AgentError>>>,
        cancellation: &mut watch::Receiver<bool>,
        partial: &mut String,
        plan: bool,
    ) -> Result<LlmResponse, AgentError> {
        let tools: Vec<_> = context
            .tools
            .iter()
            .filter(|tool| !plan || tool.read_only || tool.definition.name == "bash")
            .map(|tool| ToolSpec::Function(tool.definition.clone()))
            .collect();
        let messages = Self::build_messages(context);
        let options = RequestOptions {
            // when we add persistent sessions, we need to get these from disk
            prompt_cache_key: Some(session_id.clone()),
            session_id: Some(session_id),
            ..RequestOptions::default()
        };
        let mut stream = model
            .stream(CompletionInput {
                messages: &messages,
                tools: &tools,
                options: &options,
            })
            .await?;

        let mut builder = LlmResponseBuilder::new();
        partial.clear();
        loop {
            let next = tokio::select! {
                result = stream.next() => result,
                changed = cancellation.changed() => {
                    changed.map_err(|_| AgentError::Aborted)?;
                    if *cancellation.borrow() {
                        return Err(AgentError::Aborted);
                    }
                    continue;
                }
            };
            let Some(event) = next else { break };
            let event = event.map_err(ModelError::from)?;
            builder.apply(&event).map_err(ModelError::from)?;
            match &event {
                LlmEvent::ReasoningDelta { reasoning, .. } => {
                    if let Some(events) = events {
                        Self::send_event(
                            events,
                            Ok(AgentEvent::ReasoningDelta(reasoning.clone())),
                            cancellation,
                        )
                        .await?;
                    }
                }
                LlmEvent::TextDelta { text } => {
                    partial.push_str(text);
                    if let Some(events) = events {
                        Self::send_event(
                            events,
                            Ok(AgentEvent::TextDelta(text.clone())),
                            cancellation,
                        )
                        .await?;
                    }
                }
                _ => {}
            }
        }
        Ok(builder.finish().map_err(ModelError::from)?)
    }

    fn check_cancelled(cancellation: &watch::Receiver<bool>) -> Result<(), AgentError> {
        if *cancellation.borrow() {
            Err(AgentError::Aborted)
        } else {
            Ok(())
        }
    }

    async fn send_event(
        events: &Sender<Result<AgentEvent, AgentError>>,
        event: Result<AgentEvent, AgentError>,
        cancellation: &mut watch::Receiver<bool>,
    ) -> Result<(), AgentError> {
        tokio::select! {
            result = events.send(event) => {
                result.map_err(|_| AgentError::EventStreamClosed)
            }
            changed = cancellation.changed() => {
                changed.map_err(|_| AgentError::Aborted)?;
                Err(AgentError::Aborted)
            }
        }
    }

    fn build_messages(context: &AgentContext) -> Vec<Message> {
        let system = build_system_prompt(context.system_prompt.as_deref(), &context.skills);
        let mut messages =
            Vec::with_capacity(context.messages.len() + usize::from(system.is_some()));
        if let Some(system) = system {
            messages.push(Message::system(system));
        }
        messages.extend(context.messages.iter().map(AgentMessage::to_llm));
        messages
    }
}

fn prompt_content(content: impl Into<String>, plan_mode: bool) -> String {
    let content = content.into();
    if plan_mode {
        format!("{content}\n\nPlan mode is on, do not edit any files.")
    } else {
        content
    }
}

fn partial_response(text: &str) -> LlmResponse {
    LlmResponse {
        content: vec![llm::ContentBlock::Text(text.to_owned())],
        stop_reason: llm::StopReason::Aborted,
        usage: None,
        model: None,
        reasoning: None,
        reasoning_details: Vec::new(),
    }
}

fn tail_lines(output: &str, count: usize) -> String {
    let mut lines: Vec<_> = output.lines().rev().take(count).collect();
    lines.reverse();
    lines.join("\n")
}

pub struct AgentBuilder {
    model: Model,
    system_prompt: Option<String>,
    skills: Vec<Skill>,
    tools: Vec<AgentTool>,
    max_tool_rounds: usize,
}

impl AgentBuilder {
    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    pub fn skill(mut self, skill: Skill) -> Self {
        self.skills.push(skill);
        self
    }

    pub fn with_tools(mut self, tools: impl IntoIterator<Item = AgentTool>) -> Self {
        self.tools.extend(tools);
        self
    }

    pub fn tool(self, tool: AgentTool) -> Self {
        self.with_tools([tool])
    }

    pub fn max_tool_rounds(mut self, rounds: usize) -> Self {
        self.max_tool_rounds = rounds;
        self
    }

    pub fn build(self) -> Agent {
        let name = &self.model.info().name;
        let model_name = name
            .split_once('/')
            .map_or_else(|| name.clone(), |(_, model)| model.to_owned());
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is before Unix epoch")
            .as_nanos()
            .to_string();

        Agent {
            model: Mutex::new(self.model),
            context: Mutex::new(AgentContext::new(
                self.system_prompt,
                self.skills,
                self.tools,
            )),
            plan_mode: AtomicBool::new(false),
            max_tool_rounds: self.max_tool_rounds,
            session_id: format!("{}_{}", model_name, timestamp),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use llm::{ContentBlock, LlmApi, LlmError, LlmEvent, StopReason};
    use providers::{
        ApiId, ModelCapabilities, ModelInfo, OpenRouterProvider, Provider, ProviderId,
    };
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

    #[tokio::test]
    async fn prompt_owns_history_and_system_prompt() {
        let agent = Agent::builder(model()).system_prompt("Be helpful").build();
        let response = agent.prompt("hello").await.unwrap();
        assert_eq!(response.text(), "echo: hello");
        assert_eq!(agent.messages().await.len(), 2);
    }

    #[tokio::test]
    async fn prompt_stream_emits_text_deltas_and_finished() {
        let agent = Arc::new(Agent::builder(model()).system_prompt("Be helpful").build());
        let mut rx = agent.prompt_stream("hello");

        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event.unwrap());
        }

        assert_eq!(
            events,
            vec![
                AgentEvent::TextDelta("echo: hello".into()),
                AgentEvent::Finished,
            ]
        );
        assert_eq!(agent.messages().await.len(), 2);
    }

    struct ReasoningApi;

    #[async_trait]
    impl LlmApi for ReasoningApi {
        async fn stream(&self, _request: llm::LlmRequest<'_>) -> Result<llm::LlmStream, LlmError> {
            Ok(Box::pin(futures_util::stream::iter([
                Ok(llm::LlmEvent::ReasoningDelta {
                    reasoning: "thinking...".into(),
                    details: vec![
                        serde_json::json!({"type": "reasoning.text", "text": "thinking..."}),
                    ],
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
    async fn prompt_stream_emits_reasoning_and_text_deltas() {
        let agent = Arc::new(Agent::builder(model_with_api(Arc::new(ReasoningApi))).build());
        let mut rx = agent.prompt_stream("hello");

        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event.unwrap());
        }

        assert_eq!(
            events,
            vec![
                AgentEvent::ReasoningDelta("thinking...".into()),
                AgentEvent::TextDelta("done thinking".into()),
                AgentEvent::Finished,
            ]
        );
        let messages = agent.messages().await;
        assert_eq!(messages.len(), 2);
        if let AgentMessage::Assistant(resp) = &messages[1] {
            assert_eq!(resp.reasoning.as_deref(), Some("thinking..."));
            assert_eq!(resp.text(), "done thinking");
        } else {
            panic!("expected assistant message");
        }
    }

    #[tokio::test]
    async fn streaming_error_preserves_previous_history() {
        let api = Arc::new(FailAfterFirstApi {
            calls: AtomicUsize::new(0),
        });
        let agent = Arc::new(Agent::builder(model_with_api(api)).build());

        agent.prompt("first").await.unwrap();

        let mut stream = agent.prompt_stream("second");
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
        let messages = agent.messages().await;
        assert_eq!(messages.len(), 4);
        assert!(matches!(&messages[0], AgentMessage::User(text) if text == "first"));
        assert!(
            matches!(&messages[1], AgentMessage::Assistant(response) if response.text() == "first response")
        );
        assert!(matches!(&messages[2], AgentMessage::User(text) if text == "second"));
        assert!(
            matches!(&messages[3], AgentMessage::Assistant(response) if response.text() == "partial response")
        );
    }

    #[tokio::test]
    async fn abort_preserves_partial_assistant_output() {
        let api = Arc::new(PendingAfterFirstApi {
            calls: AtomicUsize::new(0),
        });
        let agent = Arc::new(Agent::builder(model_with_api(api)).build());

        agent.prompt("first").await.unwrap();

        let mut stream = agent.prompt_stream("second");
        assert!(matches!(
            stream.recv().await,
            Some(Ok(AgentEvent::TextDelta(text))) if text == "partial response"
        ));
        stream.abort();
        assert!(matches!(
            stream.recv().await,
            Some(Err(AgentError::Aborted))
        ));

        let messages = agent.messages().await;
        assert_eq!(messages.len(), 4);
        assert!(matches!(&messages[2], AgentMessage::User(text) if text == "second"));
        assert!(
            matches!(&messages[3], AgentMessage::Assistant(response) if response.text() == "partial response")
        );
    }
}
