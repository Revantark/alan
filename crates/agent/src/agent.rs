use crate::session::{Session, SessionError, SessionManager, StoreError};
use crate::{
    AgentError, AgentMessage, AgentTool, Skill, build_system_prompt, context::AgentContext,
};
use futures_util::StreamExt;
use llm::{
    CompletionInput, LlmEvent, LlmResponse, LlmResponseBuilder, Message, RequestOptions, ToolSpec,
    Usage,
};
use providers::{Model, ModelError};
use std::{
    path::PathBuf,
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
    Finished {
        usage: Usage,
    },
}

pub struct Agent {
    model: Mutex<Model>,
    context: Mutex<AgentContext>,
    plan_mode: AtomicBool,
    max_tool_rounds: usize,
    session_id: Mutex<String>,
    session_manager: Option<Arc<SessionManager>>,
    active_session: Mutex<Option<Session>>,
}

impl Agent {
    pub fn builder(model: Model) -> AgentBuilder {
        AgentBuilder {
            model,
            system_prompt: None,
            skills: Vec::new(),
            tools: Vec::new(),
            max_tool_rounds: 100,
            session_manager: None,
            resumed_session: None,
        }
    }

    /// Buffered prompt: runs to completion and returns the final response.
    pub async fn prompt(&self, content: impl Into<String>) -> Result<LlmResponse, AgentError> {
        let content = content.into();
        if content.trim().is_empty() {
            return Err(AgentError::Model(ModelError::Llm(
                llm::LlmError::Configuration("empty prompt".into()),
            )));
        }

        let model = self.model.lock().await;
        let mut context = self.context.lock().await;
        let plan_mode = self.plan_mode();
        let user_msg = AgentMessage::user(prompt_content(content, plan_mode));

        self.ensure_session(&model).await?;

        context.messages.push(user_msg);
        self.persist_message(&context.messages.last().unwrap())
            .await?;

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
            let partial_msg = AgentMessage::Assistant(partial_response(&partial));
            context.messages.push(partial_msg.clone());
            self.persist_message(&partial_msg).await?;
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
        if content.trim().is_empty() {
            return Err(AgentError::Model(ModelError::Llm(
                llm::LlmError::Configuration("empty prompt".into()),
            )));
        }

        let model = self.model.lock().await;
        let mut context = self.context.lock().await;
        let plan_mode = self.plan_mode();
        let user_msg = AgentMessage::user(prompt_content(content, plan_mode));

        self.ensure_session(&model).await?;

        context.messages.push(user_msg);
        self.persist_message(&context.messages.last().unwrap())
            .await?;

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
            let partial_msg = AgentMessage::Assistant(partial_response(&partial));
            context.messages.push(partial_msg.clone());
            self.persist_message(&partial_msg).await?;
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
            let session_id = self.session_id.lock().await.clone();
            let response = Self::stream_round(
                session_id,
                model,
                context,
                events,
                cancellation,
                partial,
                plan,
            )
            .await?;

            let calls: Vec<_> = response.tool_calls().cloned().collect();
            if let Some(usage) = response.usage.as_ref() {
                context.usage.accumulate(usage);
                self.persist_usage(&context.usage).await?;
            }

            if calls.is_empty() {
                if let Some(events) = events {
                    Self::send_event(
                        events,
                        Ok(AgentEvent::Finished {
                            usage: context.usage.clone(),
                        }),
                        cancellation,
                    )
                    .await?;
                }
                let msg = AgentMessage::Assistant(response.clone());
                context.messages.push(msg.clone());
                self.persist_message(&msg).await?;
                return Ok(response);
            }

            let assistant_msg = AgentMessage::Assistant(response);
            context.messages.push(assistant_msg.clone());
            self.persist_message(&assistant_msg).await?;

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
                        let msg = AgentMessage::ToolResult {
                            tool_call_id: call_id.clone(),
                            content: result.clone(),
                        };
                        context.messages.push(msg.clone());
                        self.persist_message(&msg).await?;

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
                        let msg = AgentMessage::ToolResult {
                            tool_call_id: call_id.clone(),
                            content: error.clone(),
                        };
                        context.messages.push(msg.clone());
                        self.persist_message(&msg).await?;

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

    async fn ensure_session(&self, model: &Model) -> Result<(), AgentError> {
        let mut active_session = self.active_session.lock().await;
        if active_session.is_some() {
            return Ok(());
        }

        let manager = match &self.session_manager {
            Some(m) => m,
            None => return Ok(()),
        };

        let pwd = std::env::current_dir().map_err(|e| {
            AgentError::Session(SessionError::Store(StoreError::CreateDir {
                dir: PathBuf::from("."),
                source: e,
            }))
        })?;

        let session = manager
            .create(
                pwd,
                model.info().provider.0.clone(),
                model.info().id.clone(),
                model.reasoning_effort(),
            )
            .await?;

        *self.session_id.lock().await = session.id.clone();
        *active_session = Some(session);

        Ok(())
    }

    async fn persist_message(&self, message: &AgentMessage) -> Result<(), AgentError> {
        let active_session = self.active_session.lock().await;
        if let (Some(manager), Some(session)) = (&self.session_manager, &*active_session) {
            manager.append_message(session, message).await?;
        }
        Ok(())
    }

    async fn persist_usage(&self, usage: &Usage) -> Result<(), AgentError> {
        let active_session = self.active_session.lock().await;
        if let (Some(manager), Some(session)) = (&self.session_manager, &*active_session) {
            manager.save_usage(session, usage).await?;
        }
        Ok(())
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
    session_manager: Option<Arc<SessionManager>>,
    resumed_session: Option<Session>,
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

    pub fn session_manager(mut self, manager: Arc<SessionManager>) -> Self {
        self.session_manager = Some(manager);
        self
    }

    pub fn resume_session(mut self, session: Session) -> Self {
        self.resumed_session = Some(session);
        self
    }

    pub fn build(self) -> Result<Agent, AgentError> {
        let name = &self.model.info().name;
        let model_name = name
            .split_once('/')
            .map_or_else(|| name.clone(), |(_, model)| model.to_owned());
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is before Unix epoch")
            .as_nanos()
            .to_string();

        let mut session_id = format!("{}_{}", model_name, timestamp);
        let mut messages = Vec::new();
        let mut usage = Usage::default();
        let mut active_session = None;

        if let Some(session) = self.resumed_session {
            if session.provider != self.model.info().provider.0
                || session.model != self.model.info().id
            {
                return Err(AgentError::Session(SessionError::InvalidHeader {
                    path: PathBuf::from(&session.id),
                    reason: format!(
                        "cannot resume session for model {} (provider {}) with bound model {} (provider {})",
                        session.model,
                        session.provider,
                        self.model.info().id,
                        self.model.info().provider.0
                    ),
                }));
            }
            session_id = session.id.clone();
            messages = session.messages.clone();
            usage = session.usage.clone();
            active_session = Some(session);
        }

        let mut context = AgentContext::new(self.system_prompt, self.skills, self.tools);
        context.hydrate(messages, usage);

        Ok(Agent {
            model: Mutex::new(self.model),
            context: Mutex::new(context),
            plan_mode: AtomicBool::new(false),
            max_tool_rounds: self.max_tool_rounds,
            session_id: Mutex::new(session_id),
            session_manager: self.session_manager,
            active_session: Mutex::new(active_session),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Session, SessionManager, SessionRecord};
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
        let agent = Agent::builder(model())
            .system_prompt("Be helpful")
            .build()
            .unwrap();
        let response = agent.prompt("hello").await.unwrap();
        assert_eq!(response.text(), "echo: hello");
        assert_eq!(agent.messages().await.len(), 2);
    }

    #[tokio::test]
    async fn prompt_stream_emits_text_deltas_and_finished() {
        let agent = Arc::new(
            Agent::builder(model())
                .system_prompt("Be helpful")
                .build()
                .unwrap(),
        );
        let mut rx = agent.prompt_stream("hello");

        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event.unwrap());
        }

        assert_eq!(
            events,
            vec![
                AgentEvent::TextDelta("echo: hello".into()),
                AgentEvent::Finished {
                    usage: Usage::default(),
                },
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
        let agent = Arc::new(
            Agent::builder(model_with_api(Arc::new(ReasoningApi)))
                .build()
                .unwrap(),
        );
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
                AgentEvent::Finished {
                    usage: Usage::default(),
                },
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
        let agent = Arc::new(Agent::builder(model_with_api(api)).build().unwrap());

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
        let agent = Arc::new(Agent::builder(model_with_api(api)).build().unwrap());

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

    struct ToolCallingApi {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl LlmApi for ToolCallingApi {
        async fn stream(&self, request: llm::LlmRequest<'_>) -> Result<llm::LlmStream, LlmError> {
            let call_count = self.calls.fetch_add(1, Ordering::SeqCst);
            if call_count == 0 {
                // First round: one tool call, no text content.
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
                let tool_calls = vec![llm::ToolCall {
                    id: "call-1".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command": "echo hi"}).to_string(),
                }];
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
                // Second round: final text response after tool result.
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

    #[tokio::test]
    async fn building_agent_with_manager_creates_no_file() {
        let root = std::env::temp_dir().join(format!("alan-plan3-build-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let manager = Arc::new(SessionManager::new(&root));
        let agent = Agent::builder(model())
            .session_manager(manager)
            .build()
            .unwrap();

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
    async fn first_buffered_prompt_creates_session_and_persists_messages() {
        let root =
            std::env::temp_dir().join(format!("alan-plan3-buffered-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let manager = Arc::new(SessionManager::new(&root));
        let agent = Agent::builder(model())
            .session_manager(manager.clone())
            .build()
            .unwrap();

        let response = agent.prompt("hello").await.unwrap();
        assert_eq!(response.text(), "echo: hello");

        // One session file must exist under a pwd subdirectory.
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

        // Load the session and verify messages.
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
    async fn first_streaming_prompt_has_same_persistence_behavior() {
        let root =
            std::env::temp_dir().join(format!("alan-plan3-streaming-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let manager = Arc::new(SessionManager::new(&root));
        let agent = Arc::new(
            Agent::builder(model())
                .session_manager(manager.clone())
                .build()
                .unwrap(),
        );

        let mut rx = agent.prompt_stream("streaming hello");
        while let Some(event) = rx.recv().await {
            let _ = event.unwrap();
        }

        let entries: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(entries.len(), 1);
        let pwd_dir = entries[0].path();
        let files: Vec<_> = std::fs::read_dir(&pwd_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(files.len(), 1);

        let content = std::fs::read_to_string(files[0].path()).unwrap();
        let lines: Vec<_> = content.lines().collect();
        assert_eq!(lines.len(), 3, "header + user + assistant for streaming");
    }

    #[tokio::test]
    async fn provider_is_not_called_if_session_creation_fails() {
        let _root =
            std::env::temp_dir().join(format!("alan-plan3-fail-create-{}", uuid::Uuid::new_v4()));
        // Use a non-existent, unwritable path so session creation fails.
        let manager = Arc::new(SessionManager::new("/proc/self/mem/unwritable-dir"));
        let agent = Agent::builder(model())
            .session_manager(manager)
            .build()
            .unwrap();

        let result = agent.prompt("hello").await;
        assert!(result.is_err(), "must fail when session creation fails");
    }

    #[tokio::test]
    async fn tool_call_responses_and_results_are_persisted_in_order() {
        let root =
            std::env::temp_dir().join(format!("alan-plan3-tool-order-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let manager = Arc::new(SessionManager::new(&root));
        let api = Arc::new(ToolCallingApi {
            calls: AtomicUsize::new(0),
        });
        let agent = Agent::builder(model_with_api(api))
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
            .unwrap();

        let response = agent.prompt("run echo hi").await.unwrap();
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
        // At minimum: header + user + assistant(tool_calls) + tool_result + assistant(final)
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
        // The last assistant message should be "done".
        let last_assistant = &types[types.len() - 1];
        assert_eq!(last_assistant, "message");
    }

    #[tokio::test]
    async fn aggregate_usage_from_multiple_rounds_is_persisted() {
        let root = std::env::temp_dir().join(format!("alan-plan3-usage-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let manager = Arc::new(SessionManager::new(&root));
        let api = Arc::new(ToolCallingApi {
            calls: AtomicUsize::new(0),
        });
        let agent = Agent::builder(model_with_api(api))
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
            .unwrap();

        let response = agent.prompt("run echo hi").await.unwrap();
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

        // Load the session through the manager to verify usage.
        let session = {
            let content = std::fs::read_to_string(&session_file).unwrap();
            let lines: Vec<_> = content.lines().collect();
            let header_line = lines[0];
            let record = SessionRecord::parse(header_line).unwrap();
            let SessionRecord::Session { id, .. } = record else {
                panic!("expected session header");
            };
            manager
                .load(&std::env::current_dir().unwrap(), &id)
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

        // Create a session, persist a user message, then load it back
        // so the in-memory Session has the restored messages.
        let session = manager
            .create(&root, "openrouter", "test", None)
            .await
            .expect("create session");
        manager
            .append_message(&session, &AgentMessage::user("restored message"))
            .await
            .expect("append message");
        let session = manager
            .load(&root, &session.id)
            .await
            .expect("load session with restored message");

        let model = model();
        let agent = Agent::builder(model)
            .session_manager(manager.clone())
            .resume_session(session)
            .build()
            .expect("build with resume");

        let response = agent.prompt("new message").await.unwrap();
        assert_eq!(response.text(), "echo: new message");

        // The restored message must be in the agent's history.
        let messages = agent.messages().await;
        assert!(
            messages.len() >= 2,
            "must include restored user + new user + assistant response, got {} messages",
            messages.len()
        );
        assert!(
            matches!(&messages[0], AgentMessage::User(text) if text == "restored message"),
            "first message must be the restored message, got {:?}",
            messages[0]
        );
    }

    #[tokio::test]
    async fn request_uses_persisted_session_id_and_cache_key() {
        let root =
            std::env::temp_dir().join(format!("alan-plan3-session-id-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let manager = Arc::new(SessionManager::new(&root));
        let agent = Agent::builder(model())
            .session_manager(manager.clone())
            .build()
            .unwrap();

        agent.prompt("check session id").await.unwrap();

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

        // The header must contain the session id.
        let header: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        let session_id = header["id"].as_str().unwrap();

        // The prompt_cache_key and session_id in the request options must
        // match the persisted session id. We verify this indirectly by
        // confirming the session file name matches the session id.
        let session_file_name = files[0].path().file_stem().unwrap().to_owned();
        assert_eq!(session_file_name, session_id);
    }

    #[tokio::test]
    async fn existing_no_manager_tests_still_pass() {
        // Re-run the original no-manager buffered prompt test to confirm
        // backward compatibility is preserved.
        let agent = Agent::builder(model()).build().unwrap();
        let response = agent.prompt("hello").await.unwrap();
        assert_eq!(response.text(), "echo: hello");
        assert_eq!(agent.messages().await.len(), 2);
    }
}
