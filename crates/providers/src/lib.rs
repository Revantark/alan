mod auth;
mod catalog;
mod credentials;
mod model;
mod openrouter;
mod provider;

pub use auth::{
    ApiKeyAuth, AuthError, AuthEvent, AuthInteraction, AuthOption, AuthPrompt, AuthResolver,
    CredentialAuth, InteractionError, ProviderAuth,
};
pub use catalog::{ApiId, ModelCapabilities, ModelInfo, ModelPricing, ProviderId, ServerToolInfo};
pub use credentials::{
    Credential, CredentialError, CredentialInfo, CredentialKind, CredentialStore,
    FileCredentialStore, InMemoryCredentialStore, SharedCredentialStore,
};
pub use model::{Model, ModelError, ModelOptions};
pub use openrouter::{OpenRouterAuth, OpenRouterBuilder, OpenRouterProvider};
pub use provider::{Provider, ProviderError, ProviderRegistry};

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use llm::{
        CompletionInput, ContentBlock, LlmError, LlmEvent, LlmResponse, RequestOptions, StopReason,
    };
    use std::sync::Arc;

    struct FakeApi;

    #[async_trait]
    impl llm::LlmApi for FakeApi {
        async fn stream(&self, request: llm::LlmRequest<'_>) -> Result<llm::LlmStream, LlmError> {
            let content = request
                .messages
                .last()
                .and_then(|message| message.content.clone())
                .unwrap_or_default();
            let response = LlmResponse {
                content: vec![ContentBlock::Text(format!("echo: {content}"))],
                stop_reason: StopReason::Stop,
                usage: None,
                model: Some(request.model_id.to_owned()),
                reasoning: None,
                reasoning_details: Vec::new(),
            };
            let text = response.text();
            let model = response.model.clone();
            Ok(Box::pin(futures_util::stream::iter([
                Ok(LlmEvent::TextDelta { text }),
                Ok(LlmEvent::Done {
                    stop_reason: StopReason::Stop,
                    usage: None,
                    model,
                }),
            ])))
        }
    }

    #[tokio::test]
    async fn provider_binds_catalog_model_and_model_completes() {
        let provider = OpenRouterProvider::builder("key")
            .with_model("test-model")
            .with_api(Arc::new(FakeApi))
            .build()
            .unwrap();
        let model = provider.bind("test-model").unwrap();
        let messages = [llm::Message::user("hello")];
        let options = RequestOptions::default();
        let response = model
            .complete(CompletionInput {
                messages: &messages,
                tools: &[],
                options: &options,
            })
            .await
            .unwrap();

        assert_eq!(response.text(), "echo: hello");
        assert_eq!(response.model.as_deref(), Some("test-model"));
        assert_eq!(provider.models().len(), 1);
    }

    #[test]
    fn missing_model_is_reported() {
        let provider = OpenRouterProvider::builder("key")
            .with_model("known")
            .build()
            .unwrap();
        assert!(matches!(
            provider.bind("missing"),
            Err(ProviderError::ModelNotFound(_))
        ));
    }
}
