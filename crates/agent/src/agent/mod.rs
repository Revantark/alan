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
use providers::{Model, ModelInfo, ModelOptions};
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
    pub(super) model_info: Mutex<ModelInfo>,
    model_id: String,
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

    /// Reset to a brand-new, empty session in place.
    pub async fn reset_session(&self) -> Result<(), AgentError> {
        let model = self.model.lock().await;
        self.clear_conversation().await;
        persistence::ensure_session(self, &model, None).await
    }

    /// Produce a summary of the current conversation, optionally focused.
    ///
    /// Runs one tool-less round over the context as-is plus a summarization
    /// instruction; the caller decides whether to persist the result.
    pub async fn summarize(&self, focus: Option<&str>) -> Result<String, AgentError> {
        let model = self.model.lock().await;
        let context = self.context.lock().await;
        prompt::summarize(&model, &context, &prompt::summary_instruction(focus)).await
    }

    /// Reset to a fresh session seeded with `seed` messages.
    ///
    /// Captures the current session id first and records it as the `parent` of
    /// the new session, so a summarized session stays traceable to its origin.
    /// Then clears state, creates the new session file, and appends the seed
    /// (persisted to the new session file).
    pub async fn reset_session_with(&self, seed: Vec<AgentMessage>) -> Result<(), AgentError> {
        // Capture before `clear_conversation` overwrites the id.
        let parent = self.session_id.lock().await.clone();
        let model = self.model.lock().await;
        self.clear_conversation().await;
        persistence::ensure_session(self, &model, Some(parent)).await?;
        if seed.is_empty() {
            return Ok(());
        }
        let mut context = self.context.lock().await;
        for message in seed {
            persistence::append_context_message(self, &mut context, message).await?;
        }
        Ok(())
    }

    /// Fresh id, no active session, empty conversation and usage.
    async fn clear_conversation(&self) {
        *self.session_id.lock().await = uuid::Uuid::new_v4().to_string();
        *self.active_session.lock().await = None;
        let mut context = self.context.lock().await;
        context.messages.clear();
        context.usage = llm::Usage::default();
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

    pub async fn model_options(&self) -> ModelOptions {
        let model = self.model.lock().await.clone();
        ModelOptions::from(&model)
    }

    pub async fn usage(&self) -> Usage {
        self.context.lock().await.usage.clone()
    }

    pub async fn context_tokens(&self) -> u64 {
        let u = self.context.lock().await.usage.clone();
        u.input_tokens + u.output_tokens
    }

    pub async fn info(&self) -> ModelInfo {
        self.model_info.lock().await.clone()
    }

    pub fn model_id(&self) -> String {
        self.model_id.clone()
    }

    /// Replace the bound model. The next prompt uses the new model;
    /// the reported model info is updated to match. Fails only if
    /// another prompt currently holds the model lock (i.e. a run is
    /// streaming), in which case the agent is left untouched.
    pub async fn set_model(&self, model: Model) -> Result<(), AgentError> {
        let mut current = self.model.try_lock().map_err(|_| {
            AgentError::Model(providers::ModelError::Llm(llm::LlmError::Configuration(
                "agent is busy: cannot switch models mid-run".into(),
            )))
        })?;
        let info = model.info().clone();
        let reasoning = model.reasoning_effort();
        *current = model;
        *self.model_info.lock().await = info.clone();

        let mut active = self.active_session.lock().await;
        if let (Some(manager), Some(session)) = (&self.session_manager, &mut *active) {
            session.set_model(&info.provider.0, &info.id, reasoning);
            manager.update_header_model(session).await?;
        }
        Ok(())
    }
}
