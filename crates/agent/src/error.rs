use crate::tool::ToolError;
use providers::ModelError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AgentError {
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error("tool not found: {0}")]
    ToolNotFound(String),
    #[error("maximum tool rounds exceeded")]
    MaxToolRounds,
    #[error(transparent)]
    Tool(#[from] ToolError),
}
