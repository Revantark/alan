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
    fn id(&self) -> &ProviderId;

    fn models(&self) -> Vec<ModelInfo>;

    fn server_tools(&self) -> &[ServerToolInfo];

    fn auth_methods(&self) -> Vec<crate::auth::AuthMethod>;

    async fn validate_auth(
        &self,
        auth_result: &crate::auth::AuthResult,
    ) -> Result<(), crate::auth::AuthError>;

    fn bind(&self, model_id: &str) -> Result<Model, ProviderError>;

    fn bind_with_options(
        &self,
        model_id: &str,
        options: ModelOptions,
    ) -> Result<Model, ProviderError> {
        let _ = options;
        self.bind(model_id)
    }

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
        self.providers.iter().find(|p| p.id() == id).cloned()
    }
}

pub(crate) fn bind_model(
    models: &[ModelInfo],
    apis: &HashMap<ApiId, Arc<dyn LlmApi>>,
    auth: Arc<dyn AuthResolver>,
    model_id: &str,
    options: ModelOptions,
) -> Result<Model, ProviderError> {
    let info = models
        .iter()
        .find(|model| model.id == model_id)
        .cloned()
        .ok_or_else(|| ProviderError::ModelNotFound(model_id.into()))?;
    let api = apis
        .get(&info.api)
        .cloned()
        .ok_or_else(|| ProviderError::MissingApi {
            model: info.id.clone(),
        })?;
    Ok(Model::new_with_options(info, api, auth, options))
}
