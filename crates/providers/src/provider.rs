use crate::Model;
use crate::auth::AuthResolver;
use crate::catalog::{ApiId, ModelInfo, ProviderId, ServerToolInfo};
use crate::model::ModelOptions;
use async_trait::async_trait;
use llm::LlmApi;
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("model not found: {0}")]
    ModelNotFound(String),
    #[error("API is not configured for model {model}")]
    MissingApi { model: String },
    #[error("authentication is not configured")]
    MissingAuth,
    #[error("failed to fetch models: {0}")]
    Fetch(String),
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> ProviderId;

    fn models(&self) -> Vec<ModelInfo>;

    fn server_tools(&self) -> &[ServerToolInfo];

    fn auth_methods(&self) -> Vec<crate::auth::AuthMethod>;

    /// The LLM API implementations this provider can route to, keyed by API id.
    fn apis(&self) -> &HashMap<ApiId, Arc<dyn LlmApi>>;

    /// The auth resolver used to obtain credentials for requests.
    fn auth(&self) -> Arc<dyn AuthResolver>;

    async fn validate_auth(
        &self,
        auth_result: &crate::auth::AuthResult,
    ) -> Result<(), crate::auth::AuthError>;

    /// Refresh the provider's model catalog from its source, updating the
    /// catalog returned by [`Provider::models`]. Providers without a live
    /// catalog keep their static list and the default body is a no-op.
    async fn fetch_models(&self) -> Result<(), ProviderError> {
        Ok(())
    }
}

#[derive(Default)]
pub struct ProviderRegistry {
    providers: Vec<Arc<dyn Provider>>,
}

impl ProviderRegistry {
    pub fn new(providers: impl IntoIterator<Item = Arc<dyn Provider>>) -> Self {
        Self {
            providers: providers.into_iter().collect(),
        }
    }

    pub fn providers(&self) -> &[Arc<dyn Provider>] {
        &self.providers
    }

    pub fn get(&self, id: &ProviderId) -> Option<Arc<dyn Provider>> {
        self.providers.iter().find(|p| p.id() == *id).cloned()
    }
}

/// Construct a [`Model`] bound to `model_id` from the provider's catalog,
/// apis, and auth resolver.
pub fn bind_model(
    provider: &dyn Provider,
    model_id: &str,
    options: ModelOptions,
) -> Result<Model, ProviderError> {
    let model_info = ModelInfo::new(model_id, provider.id());
    let api =
        provider
            .apis()
            .values()
            .next()
            .cloned()
            .ok_or_else(|| ProviderError::MissingApi {
                model: model_info.id.clone(),
            })?;
    Ok(Model::new_with_options(
        model_info,
        api,
        provider.auth(),
        options,
    ))
}
