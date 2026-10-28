use crate::auth::{AuthError, AuthResolver};
use crate::catalog::ModelInfo;
use llm::{
    CompletionInput, Extensions, Headers, LlmApi, LlmError, LlmRequest, LlmResponse, LlmStream,
    ReasoningEffort,
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
    pub reasoning_effort: ReasoningEffort,
    /// Provider-specific options (like OpenRouter routing) forwarded to
    /// codecs with every request.
    pub extensions: llm::Extensions,
}

impl From<&Model> for ModelOptions {
    fn from(value: &Model) -> Self {
        ModelOptions {
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

    pub async fn complete(&self, input: CompletionInput<'_>) -> Result<LlmResponse, ModelError> {
        let resolved = self.auth.resolve().await?;
        let request = self.build_request(&resolved, input);
        Ok(self.api.complete(request).await?)
    }

    pub async fn stream(&self, input: CompletionInput<'_>) -> Result<LlmStream, ModelError> {
        let resolved = self.auth.resolve().await?;
        let request = self.build_request(&resolved, input);
        Ok(self.api.stream(request).await?)
    }

    fn build_request<'a>(
        &'a self,
        resolved: &'a crate::auth::Resolved,
        input: CompletionInput<'a>,
    ) -> LlmRequest<'a> {
        let mut extensions = self.extensions.clone();
        extensions.merge(input.extensions);
        if !resolved.headers.is_empty() {
            extensions.insert(Headers(resolved.headers.clone()));
        }
        LlmRequest {
            model_id: &self.info.id,
            messages: input.messages,
            tools: input.tools,
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
    use crate::{
        ApiId, ApiKeyAuth, Auth, AuthMethod, AuthResult, Provider, ProviderError, ProviderId,
    };
    use tools::ProviderTool;

    struct OrderedProvider {
        apis: Vec<(ApiId, Arc<dyn LlmApi>)>,
    }

    #[async_trait::async_trait]
    impl Auth for OrderedProvider {
        fn auth(&self) -> Arc<dyn AuthResolver> {
            Arc::new(ApiKeyAuth::new("test-key"))
        }

        fn auth_methods(&self) -> Vec<AuthMethod> {
            vec![AuthMethod::ApiKey]
        }

        async fn validate_auth(&self, _result: &AuthResult) -> Result<(), AuthError> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl Provider for OrderedProvider {
        fn id(&self) -> ProviderId {
            ProviderId::new("ordered")
        }

        fn models(&self) -> Vec<ModelInfo> {
            Vec::new()
        }

        fn tools(&self) -> &[ProviderTool] {
            &[]
        }

        fn apis(&self) -> &[(ApiId, Arc<dyn LlmApi>)] {
            &self.apis
        }
    }

    #[test]
    fn bind_model_uses_the_first_api_in_provider_order() {
        let http = Arc::new(llm::HttpClient::new());
        let chat: Arc<dyn LlmApi> =
            Arc::new(llm::ChatCompletionsApi::new("http://unused", http.clone()));
        let responses: Arc<dyn LlmApi> = Arc::new(llm::ResponsesApi::new("http://unused", http));
        let mut provider = OrderedProvider {
            apis: vec![
                (ApiId::OpenAiResponses, responses.clone()),
                (ApiId::ChatCompletions, chat.clone()),
            ],
        };
        let model = bind_model(&provider, "model", ModelOptions::default()).unwrap();
        assert!(Arc::ptr_eq(&model.api, &responses));

        provider.apis.swap(0, 1);
        let model = bind_model(&provider, "model", ModelOptions::default()).unwrap();
        assert!(Arc::ptr_eq(&model.api, &chat));
    }

    #[test]
    fn bind_model_reports_missing_api_for_an_empty_list() {
        let provider = OrderedProvider { apis: Vec::new() };
        assert!(matches!(
            bind_model(&provider, "missing", ModelOptions::default()),
            Err(ProviderError::MissingApi { model }) if model == "missing"
        ));
    }
}
