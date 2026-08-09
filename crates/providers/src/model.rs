use crate::auth::{AuthError, AuthResolver};
use crate::catalog::ModelInfo;
use llm::{AssistantMessage, CompletionInput, LlmApi, LlmError, LlmRequest};
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ModelError {
    #[error("authentication failed: {0}")]
    Auth(#[from] AuthError),
    #[error(transparent)]
    Llm(#[from] LlmError),
}

#[derive(Clone)]
pub struct Model {
    info: ModelInfo,
    api: Arc<dyn LlmApi>,
    auth: Arc<dyn AuthResolver>,
}

impl Model {
    pub(crate) fn new(info: ModelInfo, api: Arc<dyn LlmApi>, auth: Arc<dyn AuthResolver>) -> Self {
        Self { info, api, auth }
    }

    pub fn info(&self) -> &ModelInfo {
        &self.info
    }

    pub async fn complete(
        &self,
        input: CompletionInput<'_>,
    ) -> Result<AssistantMessage, ModelError> {
        let credential = self.auth.resolve().await?;
        let request = LlmRequest {
            model_id: &self.info.id,
            messages: input.messages,
            tools: input.tools,
            options: input.options,
            credential: Some(&credential),
        };
        Ok(self.api.complete(request).await?)
    }
}
