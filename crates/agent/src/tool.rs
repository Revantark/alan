use async_trait::async_trait;
use llm::{ToolCall, ToolDefinition};
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Error)]
#[error("tool execution failed: {0}")]
pub struct ToolError(pub String);

#[async_trait]
pub trait ToolExecutor: Send + Sync {
    async fn execute(&self, call: &ToolCall) -> Result<String, ToolError>;
}

pub struct AgentTool {
    pub definition: ToolDefinition,
    pub executor: Arc<dyn ToolExecutor>,
}
