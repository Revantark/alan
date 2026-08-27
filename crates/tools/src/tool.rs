use async_trait::async_trait;
use llm::ToolCall;
use thiserror::Error;

#[derive(Debug, Error)]
#[error("tool execution failed: {0}")]
pub struct ToolError(pub String);

/// The output produced by a tool executor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolOutput {
    Text(String),
    Image {
        mime_type: String,
        /// Raw base64-encoded image data (no `data:` prefix).
        data: String,
    },
}

#[async_trait]
pub trait ToolExecutor: Send + Sync {
    async fn execute(&self, call: &ToolCall) -> Result<ToolOutput, ToolError>;
}
