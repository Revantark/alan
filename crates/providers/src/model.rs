use crate::auth::{AuthError, AuthResolver};
use crate::catalog::{ModelInfo, ProviderId, ServerToolInfo};
use llm::{
    CompletionInput, Extensions, Headers, LlmApi, LlmError, LlmRequest, LlmResponse, LlmStream,
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

/// Drop server tools `provider` does not advertise.
pub(crate) fn supported_server_tools(
    server_tools: Vec<ServerTool>,
    advertised: &[ServerToolInfo],
    provider: &ProviderId,
) -> Vec<ServerTool> {
    server_tools
        .into_iter()
        .filter(|tool| {
            let supported = advertised.iter().any(|info| info.id == tool.kind);
            if !supported {
                tracing::warn!(
                    provider = %provider,
                    tool = %tool.kind,
                    "dropping server tool not advertised by the provider"
                );
            }
            supported
        })
        .collect()
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
        let resolved = self.auth.resolve().await?;
        let tools = self.tools(input.tools);
        let request = self.build_request(&resolved, input, &tools);
        Ok(self.api.complete(request).await?)
    }

    pub async fn stream(&self, input: CompletionInput<'_>) -> Result<LlmStream, ModelError> {
        let resolved = self.auth.resolve().await?;
        let tools = self.tools(input.tools);
        let request = self.build_request(&resolved, input, &tools);
        Ok(self.api.stream(request).await?)
    }

    fn build_request<'a>(
        &'a self,
        resolved: &'a crate::auth::Resolved,
        input: CompletionInput<'a>,
        tools: &'a [ToolSpec],
    ) -> LlmRequest<'a> {
        let mut extensions = self.extensions.clone();
        extensions.merge(input.extensions);
        if !resolved.headers.is_empty() {
            extensions.insert(Headers(resolved.headers.clone()));
        }
        LlmRequest {
            model_id: &self.info.id,
            messages: input.messages,
            tools,
            options: input.options,
            credential: Some(&resolved.credential),
            reasoning_effort: self.reasoning_effort,
            extensions,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::bind_model;
    use crate::{ApiId, CustomProvider, InMemoryCredentialStore};

    fn advertised(id: &str) -> ServerToolInfo {
        ServerToolInfo {
            id: id.to_owned(),
            description: String::new(),
        }
    }

    fn server_tool(kind: &str) -> ServerTool {
        ServerTool {
            kind: kind.to_owned(),
        }
    }

    #[test]
    fn keeps_only_advertised_server_tools() {
        let tools = vec![
            server_tool("openrouter:web_fetch"),
            server_tool("other:web_search"),
        ];
        let available = [advertised("openrouter:web_fetch")];

        let kept = supported_server_tools(tools, &available, &ProviderId::new("openrouter"));

        assert_eq!(kept, vec![server_tool("openrouter:web_fetch")]);
    }

    #[test]
    fn drops_every_server_tool_when_the_provider_advertises_nothing() {
        let tools = vec![server_tool("openrouter:web_fetch")];

        let kept = supported_server_tools(tools, &[], &ProviderId::new("deepseek"));

        assert!(kept.is_empty());
    }

    #[test]
    fn bind_model_drops_server_tools_the_provider_does_not_advertise() {
        let provider = CustomProvider::new(
            ProviderId::new("custom"),
            ApiId::ChatCompletions,
            "http://127.0.0.1:1/v1",
            Arc::new(InMemoryCredentialStore::new()),
            vec![ModelInfo::new("m", ProviderId::new("custom"))],
        )
        .unwrap();
        let options = ModelOptions {
            server_tools: vec![server_tool("openrouter:web_fetch")],
            ..ModelOptions::default()
        };

        let model = bind_model(&provider, "m", options).unwrap();

        assert!(
            !model
                .tools(&[])
                .iter()
                .any(|tool| matches!(tool, ToolSpec::Server(_)))
        );
    }
}
