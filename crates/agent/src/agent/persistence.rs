use crate::AgentError;
use crate::AgentMessage;
use crate::context::AgentContext;
use crate::session::{SessionError, StoreError};
use llm::Usage;
use providers::Model;
use std::path::PathBuf;

use super::Agent;

/// Ensure a session exists. On the first call this creates and persists a
/// session file; subsequent calls are no-ops.
pub(super) async fn ensure_session(agent: &Agent, model: &Model) -> Result<(), AgentError> {
    let mut active_session = agent.active_session.lock().await;
    if active_session.is_some() {
        return Ok(());
    }

    let manager = match &agent.session_manager {
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

    *agent.session_id.lock().await = session.id.clone();
    *active_session = Some(session);

    Ok(())
}

/// Append a message to the in-memory context and persist it to disk.
pub(super) async fn append_context_message(
    agent: &Agent,
    context: &mut AgentContext,
    message: AgentMessage,
) -> Result<(), AgentError> {
    persist_message(agent, &message).await?;
    context.messages.push(message);
    Ok(())
}

async fn persist_message(agent: &Agent, message: &AgentMessage) -> Result<(), AgentError> {
    let active_session = agent.active_session.lock().await;
    if let (Some(manager), Some(session)) = (&agent.session_manager, &*active_session) {
        manager
            .append_message(&session.id, &session.pwd, message)
            .await?;
    }
    Ok(())
}

/// Applies the change in memory too, so both describe the same thing.
pub(super) async fn persist_model(
    agent: &Agent,
    provider: String,
    model: String,
    reasoning_effort: llm::ReasoningEffort,
) -> Result<(), AgentError> {
    let mut active_session = agent.active_session.lock().await;
    let (Some(manager), Some(session)) = (&agent.session_manager, &mut *active_session) else {
        return Ok(());
    };

    let record = manager
        .append_model(&session.id, &session.pwd, provider, model, reasoning_effort)
        .await?;
    session.record(record);

    Ok(())
}

pub(super) async fn persist_usage(agent: &Agent, usage: &Usage) -> Result<(), AgentError> {
    let active_session = agent.active_session.lock().await;
    if let (Some(manager), Some(session)) = (&agent.session_manager, &*active_session) {
        manager
            .append_usage(&session.id, &session.pwd, usage)
            .await?;
    }
    Ok(())
}
