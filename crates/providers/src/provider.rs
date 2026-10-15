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
    #[error("API is not configured for model {model}")]
    MissingApi { model: String },
    #[error("authentication is not configured")]
    MissingAuth,
    #[error("failed to fetch models: {0}")]
    Fetch(String),
    #[error("{0:?} is not supported for custom providers yet")]
    UnsupportedApi(ApiId),
    #[error("issued OAuth tokens did not contain a usable credential: {0}")]
    SignIn(String),
}

#[async_trait]
pub trait Auth: Send + Sync {
    /// The auth resolver used to obtain credentials for requests.
    fn auth(&self) -> Arc<dyn AuthResolver>;

    /// The authentication methods this provider supports.
    fn auth_methods(&self) -> Vec<crate::auth::AuthMethod>;

    async fn validate_auth(
        &self,
        result: &crate::auth::AuthResult,
    ) -> Result<(), crate::auth::AuthError>;

    /// Turn issued OAuth tokens into the credential to store. The default
    /// stores the raw tokens; a provider with provider-specific claims
    /// (e.g. an account id claim) overrides it to resolve them.
    fn credential_from_tokens(
        &self,
        tokens: crate::oauth::Tokens,
    ) -> Result<crate::Credential, ProviderError> {
        let expires_at = tokens.expires_at();
        Ok(crate::Credential::OAuth {
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            id_token: tokens.id_token,
            expires_at,
            metadata: Default::default(),
        })
    }
}

/// A model provider: identity, model catalog, and the API routing used to
/// bind a catalog entry to a live [`crate::Model`].
///
/// Every provider is also an [`Auth`] — binding a model requires its
/// credential resolver — but the two responsibilities are kept in separate
/// traits.
#[async_trait]
pub trait Provider: Auth + Send + Sync {
    fn id(&self) -> ProviderId;

    fn models(&self) -> Vec<ModelInfo>;

    fn server_tools(&self) -> &[ServerToolInfo];

    /// The LLM API implementations this provider can route to, keyed by API id.
    fn apis(&self) -> &HashMap<ApiId, Arc<dyn LlmApi>>;

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

    pub fn remove_custom(&self, id: &ProviderId) {
        self.custom
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|provider| provider.id() != *id);
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
    fn each_lookup_sees_the_right_track() {
        let registry = ProviderRegistry::new([builtin("openrouter")]);
        registry.add_custom(custom("grok", "grok-4"));

        assert!(registry.get(&ProviderId::new("openrouter")).is_some());
        assert!(registry.get(&ProviderId::new("grok")).is_some());
        assert!(registry.is_builtin(&ProviderId::new("openrouter")));
        assert!(!registry.is_builtin(&ProviderId::new("grok")));
        assert!(registry.custom(&ProviderId::new("grok")).is_some());
        assert!(registry.custom(&ProviderId::new("openrouter")).is_none());
    }

    #[test]
    fn remove_custom_leaves_builtins_alone() {
        let registry = ProviderRegistry::new([builtin("openrouter")]);
        registry.add_custom(custom("grok", "grok-4"));

        registry.remove_custom(&ProviderId::new("grok"));
        registry.remove_custom(&ProviderId::new("openrouter"));

        assert!(registry.get(&ProviderId::new("grok")).is_none());
        assert!(registry.get(&ProviderId::new("openrouter")).is_some());
    }
}
