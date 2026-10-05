use crate::auth::{AuthError, AuthResolver};
use crate::catalog::ModelInfo;
use llm::{
    CompletionInput, Extensions, LlmApi, LlmError, LlmRequest, LlmResponse, LlmStream,
    ReasoningEffort, ServerTool, ToolSpec,
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
    pub reasoning_effort: ReasoningEffort,
    /// Provider-specific options (like OpenRouter routing) forwarded to
    /// codecs with every request.
    pub extensions: llm::Extensions,
}

impl From<&Model> for ModelOptions {
    fn from(value: &Model) -> Self {
        ModelOptions {
            server_tools: value.server_tools.clone(),
            reasoning_effort: value.reasoning_effort(),
            extensions: value.extensions.clone(),
        }
    }
}

#[derive(Clone)]
pub struct Model {
    info: ModelInfo,
    api: Arc<dyn LlmApi>,
    auth: Arc<dyn AuthResolver>,
    server_tools: Vec<ServerTool>,
    reasoning_effort: ReasoningEffort,
    extensions: Extensions,
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
            reasoning_effort: options.reasoning_effort,
            extensions: options.extensions,
        }
    }

    pub fn info(&self) -> &ModelInfo {
        &self.info
    }

    pub fn reasoning_effort(&self) -> ReasoningEffort {
        self.reasoning_effort
    }

    /// Update the reasoning effort on a bound model. The change takes effect
    /// on the next prompt; in-flight runs already hold the model and are
    /// unaffected.
    pub fn set_reasoning_effort(&mut self, reasoning_effort: ReasoningEffort) {
        self.reasoning_effort = reasoning_effort;
    }

    pub fn set_extensions(&mut self, extensions: Extensions) {
        self.extensions = extensions;
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
        let mut extensions = self.extensions.clone();
        extensions.merge(input.extensions);
        let request = LlmRequest {
            model_id: &self.info.id,
            messages: input.messages,
            tools: &tools,
            options: input.options,
            credential: Some(&credential),
            reasoning_effort: self.reasoning_effort,
            extensions,
        };
        Ok(self.api.complete(request).await?)
    }

    pub async fn stream(&self, input: CompletionInput<'_>) -> Result<LlmStream, ModelError> {
        let credential = self.auth.resolve().await?;
        let tools = self.tools(input.tools);
        let mut extensions = self.extensions.clone();
        extensions.merge(input.extensions);
        let request = LlmRequest {
            model_id: &self.info.id,
            messages: input.messages,
            tools: &tools,
            options: input.options,
            credential: Some(&credential),
            reasoning_effort: self.reasoning_effort,
            extensions,
        };
        Ok(self.api.stream(request).await?)
    }
}
