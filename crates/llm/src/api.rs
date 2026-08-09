use crate::{AssistantMessage, LlmError, LlmRequest};
use async_trait::async_trait;

#[async_trait]
pub trait LlmApi: Send + Sync {
    async fn complete(&self, request: LlmRequest<'_>) -> Result<AssistantMessage, LlmError>;
}
