use crate::Model;
use crate::auth::{AuthResolver, ProviderAuth};
use crate::catalog::{ApiId, ModelInfo, ProviderId, ServerToolInfo};
use crate::model::ModelOptions;
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
}

pub trait Provider: Send + Sync {
    fn id(&self) -> &ProviderId;
    fn models(&self) -> &[ModelInfo];
    fn server_tools(&self) -> &[ServerToolInfo];
    fn bind(&self, model_id: &str) -> Result<Model, ProviderError>;
    fn bind_with_options(
        &self,
        model_id: &str,
        options: ModelOptions,
    ) -> Result<Model, ProviderError> {
        let _ = options;
        self.bind(model_id)
    }
    fn auth(&self) -> &dyn ProviderAuth;
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
