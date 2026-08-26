use providers::ModelError;
use thiserror::Error;
use tools::ToolError;

#[derive(Debug, Error)]
pub enum AgentError {
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error("tool not found: {0}")]
    ToolNotFound(String),
    #[error("maximum tool rounds exceeded")]
    MaxToolRounds,
    #[error("agent request aborted")]
    Aborted,
    #[error("agent event stream closed")]
    EventStreamClosed,
    #[error(transparent)]
    Tool(#[from] ToolError),
    #[error(transparent)]
    Session(#[from] crate::session::SessionError),
}
