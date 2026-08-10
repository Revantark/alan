use crate::{LlmError, LlmRequest, LlmResponse, LlmResponseBuilder, LlmStream};
use async_trait::async_trait;
use futures_util::StreamExt;

#[async_trait]
pub trait LlmApi: Send + Sync {
    async fn stream(&self, request: LlmRequest<'_>) -> Result<LlmStream, LlmError>;

    async fn complete(&self, request: LlmRequest<'_>) -> Result<LlmResponse, LlmError> {
        let mut stream = self.stream(request).await?;
        let mut builder = LlmResponseBuilder::new();
        while let Some(event) = stream.next().await {
            builder.apply(&event?)?;
        }
        builder.finish()
    }
}
