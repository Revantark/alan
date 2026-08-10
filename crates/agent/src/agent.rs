use crate::{AgentError, AgentMessage, AgentTool, Skill, build_system_prompt};
use futures_util::StreamExt;
use llm::{
    CompletionInput, LlmEvent, LlmResponse, LlmResponseBuilder, Message, RequestOptions,
    ToolDefinition,
};
use providers::{Model, ModelError};
use std::sync::Arc;
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
///
/// Tool-call fragments and provider protocol details stay internal to the
/// agent. Consumers receive only text deltas and a final completion signal.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    TextDelta(String),
    Finished,
}

pub struct AgentContext {
    pub system_prompt: Option<String>,
    pub skills: Vec<Skill>,
    pub messages: Vec<AgentMessage>,
    pub tools: Vec<AgentTool>,
}

pub struct Agent {
    model: Mutex<Model>,
    context: Mutex<AgentContext>,
    max_tool_rounds: usize,
}

impl Agent {
    pub fn builder(model: Model) -> AgentBuilder {
        AgentBuilder {
            model,
            system_prompt: None,
            skills: Vec::new(),
            tools: Vec::new(),
            max_tool_rounds: 8,
        }
    }

    /// Buffered prompt: runs to completion and returns the final response.
    pub async fn prompt(&self, content: impl Into<String>) -> Result<LlmResponse, AgentError> {
        let model = self.model.lock().await;
        let mut context = self.context.lock().await;
        let original_len = context.messages.len();
        context.messages.push(AgentMessage::user(content));

        let (_cancellation, mut cancellation_receiver) = watch::channel(false);
        let result = self
            .run_with(&model, &mut context, None, &mut cancellation_receiver)
            .await;
        if result.is_err() {
            context.messages.truncate(original_len);
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
        let original_len = context.messages.len();
        context.messages.push(AgentMessage::user(content));

        let result = self
            .run_with(&model, &mut context, Some(events), &mut cancellation)
            .await;
        if result.is_err() {
            context.messages.truncate(original_len);
        }
        result
    }

    async fn run_with(
        &self,
        model: &Model,
        context: &mut AgentContext,
        events: Option<&Sender<Result<AgentEvent, AgentError>>>,
        cancellation: &mut watch::Receiver<bool>,
    ) -> Result<LlmResponse, AgentError> {
        for _ in 0..self.max_tool_rounds {
            Self::check_cancelled(cancellation)?;
            let response = Self::stream_round(model, context, events, cancellation).await?;
            context
                .messages
                .push(AgentMessage::Assistant(response.clone()));
            let calls: Vec<_> = response.tool_calls().cloned().collect();
            if calls.is_empty() {
                if let Some(events) = events {
                    Self::send_event(events, Ok(AgentEvent::Finished), cancellation).await?;
                }
                return Ok(response);
            }
            for call in calls {
                Self::check_cancelled(cancellation)?;
                let tool = context
                    .tools
                    .iter()
                    .find(|tool| tool.definition.name == call.name)
                    .ok_or_else(|| AgentError::ToolNotFound(call.name.clone()))?;
                let result = tool.executor.execute(&call).await?;
                Self::check_cancelled(cancellation)?;
                context.messages.push(AgentMessage::ToolResult {
                    tool_call_id: call.id,
                    content: result,
                });
            }
        }
        Err(AgentError::MaxToolRounds)
    }

    async fn stream_round(
        model: &Model,
        context: &AgentContext,
        events: Option<&Sender<Result<AgentEvent, AgentError>>>,
        cancellation: &mut watch::Receiver<bool>,
    ) -> Result<LlmResponse, AgentError> {
        let messages = Self::build_messages(context);
        let definitions: Vec<ToolDefinition> = context
            .tools
            .iter()
            .map(|tool| tool.definition.clone())
            .collect();
        let options = RequestOptions::default();
        let mut stream = model
            .stream(CompletionInput {
                messages: &messages,
                tools: &definitions,
                options: &options,
            })
            .await?;

        let mut builder = LlmResponseBuilder::new();
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
            if let LlmEvent::TextDelta { text } = &event
                && let Some(events) = events
            {
                Self::send_event(
                    events,
                    Ok(AgentEvent::TextDelta(text.clone())),
                    cancellation,
                )
                .await?;
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

    pub fn tool(mut self, tool: AgentTool) -> Self {
        self.tools.push(tool);
        self
    }

    pub fn max_tool_rounds(mut self, rounds: usize) -> Self {
        self.max_tool_rounds = rounds;
        self
    }

    pub fn build(self) -> Agent {
        Agent {
            model: Mutex::new(self.model),
            context: Mutex::new(AgentContext {
                system_prompt: self.system_prompt,
                skills: self.skills,
                messages: Vec::new(),
                tools: self.tools,
            }),
            max_tool_rounds: self.max_tool_rounds,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use llm::{ContentBlock, LlmApi, LlmError, StopReason};
    use providers::{
        ApiId, ModelCapabilities, ModelInfo, OpenRouterProvider, Provider, ProviderId,
    };
    use std::sync::Arc;

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

    fn model() -> Model {
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
            .with_api(Arc::new(FakeApi))
            .build()
            .unwrap()
            .bind("test")
            .unwrap()
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
}
