mod builder;
mod event;
mod mode;
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
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU8, Ordering},
};
use tokio::sync::Mutex;

pub use builder::AgentBuilder;
pub use event::{AgentEvent, AgentStream};
pub use mode::Mode;
pub use prompt_builder::PromptBuilder;

pub(crate) const AGENT_EVENT_CAPACITY: usize = 128;

pub struct Agent {
    pub(super) model: Mutex<Model>,
    pub(super) context: Mutex<crate::context::AgentContext>,
    pub(super) mode: AtomicU8,
    pub(super) review_intro_pending: AtomicBool,
    pub(super) max_tool_rounds: usize,
    /// Stable identifier used for LLM prompt caching.
    /// When no session manager is configured this is a random UUID;
    /// once a session is created it matches `active_session.id`.
    pub(super) session_id: Mutex<String>,
    pub(super) session_manager: Option<Arc<SessionManager>>,
    pub(super) active_session: Mutex<Option<Session>>,
    /// Working directory reported in the conversation's first message.
    pub(super) working_directory: Option<PathBuf>,
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
            working_directory: None,
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

    pub fn set_mode(&self, mode: Mode) {
        self.mode.store(mode.as_u8(), Ordering::Release);
        self.review_intro_pending
            .store(mode == Mode::Review, std::sync::atomic::Ordering::Release);
    }

    pub fn mode(&self) -> Mode {
        Mode::from_u8(self.mode.load(Ordering::Acquire))
    }

    /// Take the pending review-guidelines flag. Returns true only for the
    /// first prompt after review mode was entered.
    pub(super) fn take_review_intro(&self) -> bool {
        self.review_intro_pending
            .swap(false, std::sync::atomic::Ordering::AcqRel)
    }

    pub async fn messages(&self) -> Vec<AgentMessage> {
        self.context.lock().await.messages.clone()
    }

    pub async fn usage(&self) -> Usage {
        self.context.lock().await.usage.clone()
    }
}
