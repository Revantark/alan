use crate::{AgentError, AgentMessage, AgentTool, Skill, build_system_prompt};
use llm::{AssistantMessage, CompletionInput, Message, RequestOptions, ToolDefinition};
use providers::{Model, ModelError};
use tokio::sync::Mutex;

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

    pub async fn prompt(&self, content: impl Into<String>) -> Result<AssistantMessage, AgentError> {
        let model = self.model.lock().await;
        let mut context = self.context.lock().await;
        let original_len = context.messages.len();
        context.messages.push(AgentMessage::user(content));

        let result = self.run(&model, &mut context).await;
        if result.is_err() {
            context.messages.truncate(original_len);
        }
        result
    }

    pub async fn set_model(&self, model: Model) {
        *self.model.lock().await = model;
    }

    pub async fn messages(&self) -> Vec<AgentMessage> {
        self.context.lock().await.messages.clone()
    }

    async fn run(
        &self,
        model: &Model,
        context: &mut AgentContext,
    ) -> Result<AssistantMessage, AgentError> {
        for _ in 0..self.max_tool_rounds {
            let response = self.complete(model, context).await?;
            context
                .messages
                .push(AgentMessage::Assistant(response.clone()));
            let calls: Vec<_> = response.tool_calls().cloned().collect();
            if calls.is_empty() {
                return Ok(response);
            }
            for call in calls {
                let tool = context
                    .tools
                    .iter()
                    .find(|tool| tool.definition.name == call.name)
                    .ok_or_else(|| AgentError::ToolNotFound(call.name.clone()))?;
                let result = tool.executor.execute(&call).await?;
                context.messages.push(AgentMessage::ToolResult {
                    tool_call_id: call.id,
                    content: result,
                });
            }
        }
        Err(AgentError::MaxToolRounds)
    }

    async fn complete(
        &self,
        model: &Model,
        context: &AgentContext,
    ) -> Result<AssistantMessage, ModelError> {
        let system = build_system_prompt(context.system_prompt.as_deref(), &context.skills);
        let mut messages =
            Vec::with_capacity(context.messages.len() + usize::from(system.is_some()));
        if let Some(system) = system {
            messages.push(Message::system(system));
        }
        messages.extend(context.messages.iter().map(AgentMessage::to_llm));
        let definitions: Vec<ToolDefinition> = context
            .tools
            .iter()
            .map(|tool| tool.definition.clone())
            .collect();
        let options = RequestOptions::default();
        model
            .complete(CompletionInput {
                messages: &messages,
                tools: &definitions,
                options: &options,
            })
            .await
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
        async fn complete(
            &self,
            request: llm::LlmRequest<'_>,
        ) -> Result<AssistantMessage, LlmError> {
            let user = request
                .messages
                .iter()
                .rev()
                .find_map(|message| message.content.clone())
                .unwrap_or_default();
            Ok(AssistantMessage {
                content: vec![ContentBlock::Text(format!("echo: {user}"))],
                stop_reason: StopReason::Stop,
                usage: None,
                model: Some(request.model_id.to_owned()),
            })
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
}
