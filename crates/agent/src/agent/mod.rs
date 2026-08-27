mod builder;
mod event;
mod persistence;
mod prompt;
mod prompt_builder;
mod tool_loop;

#[cfg(test)]
mod tests;

use crate::session::{Session, SessionManager};
use crate::{AgentError, AgentMessage};
use llm::Usage;
use providers::Model;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::Mutex;

pub use builder::AgentBuilder;
pub use event::{AgentEvent, AgentStream};
pub use prompt_builder::PromptBuilder;

pub(crate) const AGENT_EVENT_CAPACITY: usize = 128;

pub struct Agent {
    pub(super) model: Mutex<Model>,
    pub(super) context: Mutex<crate::context::AgentContext>,
    pub(super) plan_mode: AtomicBool,
    pub(super) max_tool_rounds: usize,
    /// Stable identifier used for LLM prompt caching.
    /// When no session manager is configured this is a random UUID;
    /// once a session is created it matches `active_session.id`.
    pub(super) session_id: Mutex<String>,
    pub(super) session_manager: Option<Arc<SessionManager>>,
    pub(super) active_session: Mutex<Option<Session>>,
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

    /// Start building a prompt request.
    ///
    /// Returns a [`PromptBuilder`] that can be configured with chained
    /// setter calls, then passed to [`ask`](Self::ask) to execute.
    pub fn prompt(&self) -> PromptBuilder {
        PromptBuilder::new()
    }

    /// Execute a prompt request and return an [`AgentStream`] for
    /// receiving events.
    ///
    /// The agent runs the full prompt lifecycle (including tool-call
    /// rounds) in a background task. Events are streamed through the
    /// returned channel.
    ///
    /// Use [`AgentStream::into_response`] to drain the stream and
    /// extract the final [`LlmResponse`](llm::LlmResponse).
    pub fn ask(self: &Arc<Self>, builder: PromptBuilder) -> Result<AgentStream, AgentError> {
        let content = builder.content.ok_or_else(|| {
            AgentError::Model(providers::ModelError::Llm(llm::LlmError::Configuration(
                "empty prompt".into(),
            )))
        })?;
        prompt::validate_prompt(&content, &builder.images)?;
        Ok(prompt::spawn_prompt_task(
            self,
            content,
            builder.images,
            builder.stream,
        ))
    }

    pub async fn session_id(&self) -> Option<String> {
        self.active_session
            .lock()
            .await
            .as_ref()
            .map(|session| session.id.clone())
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

    pub async fn usage(&self) -> Usage {
        self.context.lock().await.usage.clone()
    }
}
