use crate::Model;
use crate::auth::AuthResolver;
use crate::catalog::{ApiId, ModelInfo, ProviderId, ServerToolInfo};
use crate::local::LocalProvider;
use crate::model::ModelOptions;
use async_trait::async_trait;
use llm::LlmApi;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
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

pub struct ProviderRegistry {
    providers: RwLock<Vec<Arc<dyn Provider>>>,
    local_provider: Option<Arc<LocalProvider>>,
}

impl ProviderRegistry {
    pub fn new(providers: impl IntoIterator<Item = Arc<dyn Provider>>) -> Self {
        Self {
            providers: RwLock::new(providers.into_iter().collect()),
            local_provider: None,
        }
    }

    pub fn with_local_provider(
        providers: impl IntoIterator<Item = Arc<dyn Provider>>,
        local_provider: Arc<LocalProvider>,
    ) -> Self {
        Self {
            providers: RwLock::new(providers.into_iter().collect()),
            local_provider: Some(local_provider),
        }
    }

    /// A snapshot: `/provider` can change the set while Alan runs.
    pub fn providers(&self) -> Vec<Arc<dyn Provider>> {
        self.providers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn get(&self, id: &ProviderId) -> Option<Arc<dyn Provider>> {
        self.providers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|p| p.id() == *id)
            .cloned()
    }

    /// Register `provider`, replacing one already registered under its id.
    pub fn insert(&self, provider: Arc<dyn Provider>) {
        let mut providers = self.providers.write().unwrap_or_else(|e| e.into_inner());
        match providers
            .iter_mut()
            .find(|existing| existing.id() == provider.id())
        {
            Some(existing) => *existing = provider,
            None => providers.push(provider),
        }
    }

    pub fn remove(&self, id: &ProviderId) -> bool {
        let mut providers = self.providers.write().unwrap_or_else(|e| e.into_inner());
        let before = providers.len();
        providers.retain(|provider| provider.id() != *id);
        providers.len() < before
    }

    /// Return an `Arc` clone of the local provider, if registered.
    pub fn local(&self) -> Option<Arc<LocalProvider>> {
        self.local_provider.clone()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CustomProvider, InMemoryCredentialStore};

    fn provider(name: &str, model: &str) -> Arc<dyn Provider> {
        Arc::new(CustomProvider::new(
            ProviderId::new(name),
            "http://localhost:11434/v1",
            Arc::new(InMemoryCredentialStore::new()),
            vec![ModelInfo::new(model, ProviderId::new(name))],
        ))
    }

    #[test]
    fn insert_adds_new_ids_and_replaces_existing_ones() {
        let registry = ProviderRegistry::new([provider("a", "old")]);
        registry.insert(provider("a", "new"));
        registry.insert(provider("b", "m"));

        assert_eq!(registry.providers().len(), 2);
        let a = registry.get(&ProviderId::new("a")).unwrap();
        assert_eq!(a.models()[0].id, "new");
    }

    #[test]
    fn remove_reports_whether_a_provider_went() {
        let registry = ProviderRegistry::new([provider("a", "m")]);
        assert!(registry.remove(&ProviderId::new("a")));
        assert!(!registry.remove(&ProviderId::new("a")));
        assert!(registry.get(&ProviderId::new("a")).is_none());
    }
}
