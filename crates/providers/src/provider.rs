use crate::Model;
use crate::auth::AuthResolver;
use crate::catalog::{ApiId, ModelInfo, ProviderId, ServerToolInfo};
use crate::custom::CustomProvider;
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
    #[error("{0:?} is not supported for custom providers yet")]
    UnsupportedApi(ApiId),
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

/// Every provider a model can bind to, in two tracks: the built-ins fixed at
/// construction, and custom providers added and removed at runtime. The
/// custom track keeps their type so an edit can reach them.
pub struct ProviderRegistry {
    builtin: Vec<Arc<dyn Provider>>,
    custom: RwLock<Vec<Arc<CustomProvider>>>,
}

impl ProviderRegistry {
    pub fn new(builtin: impl IntoIterator<Item = Arc<dyn Provider>>) -> Self {
        Self {
            builtin: builtin.into_iter().collect(),
            custom: RwLock::new(Vec::new()),
        }
    }

    /// A snapshot of both tracks, built-ins first.
    pub fn providers(&self) -> Vec<Arc<dyn Provider>> {
        let custom = self.custom.read().unwrap_or_else(|e| e.into_inner());
        self.builtin
            .iter()
            .cloned()
            .chain(
                custom
                    .iter()
                    .map(|provider| Arc::clone(provider) as Arc<dyn Provider>),
            )
            .collect()
    }

    pub fn get(&self, id: &ProviderId) -> Option<Arc<dyn Provider>> {
        self.builtin
            .iter()
            .find(|p| p.id() == *id)
            .cloned()
            .or_else(|| self.custom(id).map(|p| p as Arc<dyn Provider>))
    }

    pub fn is_builtin(&self, id: &ProviderId) -> bool {
        self.builtin.iter().any(|p| p.id() == *id)
    }

    pub fn custom(&self, id: &ProviderId) -> Option<Arc<CustomProvider>> {
        self.custom
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|p| p.id() == *id)
            .cloned()
    }

    pub fn add_custom(&self, provider: Arc<CustomProvider>) {
        self.custom
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .push(provider);
    }

    pub fn remove_custom(&self, id: &ProviderId) -> bool {
        let mut custom = self.custom.write().unwrap_or_else(|e| e.into_inner());
        let before = custom.len();
        custom.retain(|provider| provider.id() != *id);
        custom.len() < before
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

    fn custom(name: &str, model: &str) -> Arc<CustomProvider> {
        Arc::new(
            CustomProvider::new(
                ProviderId::new(name),
                ApiId::ChatCompletions,
                "http://localhost:11434/v1",
                Arc::new(InMemoryCredentialStore::new()),
                vec![ModelInfo::new(model, ProviderId::new(name))],
            )
            .unwrap(),
        )
    }

    // Any provider stands in for a built-in: only the track it sits in matters.
    fn builtin(name: &str) -> Arc<dyn Provider> {
        custom(name, "m")
    }

    #[test]
    fn providers_lists_builtins_before_custom() {
        let registry = ProviderRegistry::new([builtin("openrouter")]);
        registry.add_custom(custom("grok", "grok-4"));

        let ids: Vec<_> = registry.providers().iter().map(|p| p.id().0).collect();

        assert_eq!(ids, ["openrouter", "grok"]);
    }

    #[test]
    fn get_finds_both_tracks_and_is_builtin_only_the_first() {
        let registry = ProviderRegistry::new([builtin("openrouter")]);
        registry.add_custom(custom("grok", "grok-4"));

        assert!(registry.get(&ProviderId::new("openrouter")).is_some());
        assert!(registry.get(&ProviderId::new("grok")).is_some());
        assert!(registry.is_builtin(&ProviderId::new("openrouter")));
        assert!(!registry.is_builtin(&ProviderId::new("grok")));
    }

    #[test]
    fn custom_returns_the_typed_provider_from_the_custom_track_only() {
        let registry = ProviderRegistry::new([builtin("openrouter")]);
        registry.add_custom(custom("grok", "grok-4"));

        let grok = registry.custom(&ProviderId::new("grok")).unwrap();

        assert_eq!(grok.models()[0].id, "grok-4");
        assert!(registry.custom(&ProviderId::new("openrouter")).is_none());
    }

    #[test]
    fn remove_custom_leaves_builtins_alone() {
        let registry = ProviderRegistry::new([builtin("openrouter")]);
        registry.add_custom(custom("grok", "grok-4"));

        assert!(registry.remove_custom(&ProviderId::new("grok")));
        assert!(!registry.remove_custom(&ProviderId::new("grok")));
        assert!(!registry.remove_custom(&ProviderId::new("openrouter")));
        assert!(registry.get(&ProviderId::new("openrouter")).is_some());
    }
}
