use crate::session::{Session, SessionError, SessionManager};
use crate::{AgentError, AgentTool, Skill, context::AgentContext};
use llm::Usage;
use providers::Model;
use std::{
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
};
use tokio::sync::Mutex;

use super::Agent;

pub struct AgentBuilder {
    pub(super) model: Model,
    pub(super) system_prompt: Option<String>,
    pub(super) skills: Vec<Skill>,
    pub(super) tools: Vec<AgentTool>,
    pub(super) max_tool_rounds: usize,
    pub(super) session_manager: Option<Arc<SessionManager>>,
    pub(super) resumed_session: Option<Session>,
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
        let mut session_id = uuid::Uuid::new_v4().to_string();
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
