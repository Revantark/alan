use crate::auth::{AuthError, AuthResolver};
use crate::catalog::ModelInfo;
use llm::{
    CompletionInput, LlmApi, LlmError, LlmRequest, LlmResponse, LlmStream, ServerTool, ToolSpec,
};
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ModelError {
    #[error("authentication failed: {0}")]
    Auth(#[from] AuthError),
    #[error(transparent)]
    Llm(#[from] LlmError),
}

#[derive(Clone, Default)]
pub struct ModelOptions {
    pub server_tools: Vec<ServerTool>,
}

#[derive(Clone)]
pub struct Model {
    info: ModelInfo,
    api: Arc<dyn LlmApi>,
    auth: Arc<dyn AuthResolver>,
    server_tools: Vec<ServerTool>,
}

impl Model {
    pub(crate) fn new_with_options(
        info: ModelInfo,
        api: Arc<dyn LlmApi>,
        auth: Arc<dyn AuthResolver>,
        options: ModelOptions,
    ) -> Self {
        Self {
            info,
            api,
            auth,
            server_tools: options.server_tools,
        }
    }

    pub fn info(&self) -> &ModelInfo {
        &self.info
    }

    fn tools<'a>(&'a self, local: &'a [ToolSpec]) -> Vec<ToolSpec> {
        local
            .iter()
            .cloned()
            .chain(self.server_tools.iter().cloned().map(ToolSpec::Server))
            .collect()
    }

    pub async fn complete(&self, input: CompletionInput<'_>) -> Result<LlmResponse, ModelError> {
        let credential = self.auth.resolve().await?;
        let tools = self.tools(input.tools);
        let request = LlmRequest {
            model_id: &self.info.id,
            messages: input.messages,
            tools: &tools,
            options: input.options,
            credential: Some(&credential),
        };
        Ok(self.api.complete(request).await?)
    }

    pub async fn stream(&self, input: CompletionInput<'_>) -> Result<LlmStream, ModelError> {
        let credential = self.auth.resolve().await?;
        let tools = self.tools(input.tools);
        let request = LlmRequest {
            model_id: &self.info.id,
            messages: input.messages,
            tools: &tools,
            options: input.options,
            credential: Some(&credential),
        };
        Ok(self.api.stream(request).await?)
    }
}
