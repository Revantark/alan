use crate::auth::{AuthError, AuthResolver};
use crate::catalog::ModelInfo;
use llm::{
    CompletionInput, LlmApi, LlmError, LlmRequest, LlmResponse, LlmStream, ReasoningEffort,
    ServerTool, ToolSpec,
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
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Ordered provider list forwarded to the API. Empty means the request
    /// carries no `provider` block at all.
    pub provider_order: Vec<String>,
}

#[derive(Clone)]
pub struct Model {
    info: ModelInfo,
    api: Arc<dyn LlmApi>,
    auth: Arc<dyn AuthResolver>,
    server_tools: Vec<ServerTool>,
    reasoning_effort: Option<ReasoningEffort>,
    provider_order: Vec<String>,
}

impl Model {
    pub(crate) fn new_with_options(
        info: ModelInfo,
        api: Arc<dyn LlmApi>,
        auth: Arc<dyn AuthResolver>,
        options: ModelOptions,
    ) -> Self {
        let reasoning_effort = options.reasoning_effort.or(info.capabilities.reasoning);
        Self {
            info,
            api,
            auth,
            server_tools: options.server_tools,
            reasoning_effort,
            provider_order: options.provider_order,
        }
    }

    pub fn info(&self) -> &ModelInfo {
        &self.info
    }

    pub fn reasoning_effort(&self) -> Option<ReasoningEffort> {
        self.reasoning_effort
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
            reasoning_effort: self.reasoning_effort,
            provider_order: (!self.provider_order.is_empty())
                .then_some(self.provider_order.as_slice()),
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
            reasoning_effort: self.reasoning_effort,
            provider_order: (!self.provider_order.is_empty())
                .then_some(self.provider_order.as_slice()),
        };
        Ok(self.api.stream(request).await?)
    }
}
