use crate::Model;
use crate::auth::AuthResolver;
use crate::catalog::{ApiId, ModelInfo, ProviderId, ServerToolInfo};
use crate::model::ModelOptions;
use async_trait::async_trait;
use llm::LlmApi;
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

    /// API bindings in preference order; [`bind_model`] uses the first entry.
    fn apis(&self) -> &[(ApiId, Arc<dyn LlmApi>)];

    /// Refresh the provider's model catalog from its source, updating the
    /// catalog returned by [`Provider::models`]. Providers without a live
    /// catalog keep their static list and the default body is a no-op.
    async fn fetch_models(&self) -> Result<(), ProviderError> {
        Ok(())
    }
}

/// Every provider a model can bind to: the built-ins, then the custom
/// providers `/provider add` registers at runtime.
pub struct ProviderRegistry {
    providers: RwLock<Vec<Arc<dyn Provider>>>,
}

impl ProviderRegistry {
    pub fn new(providers: impl IntoIterator<Item = Arc<dyn Provider>>) -> Self {
        Self {
            providers: RwLock::new(providers.into_iter().collect()),
        }
    }

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

    /// Add `provider`, or replace the one with its id in place.
    pub fn insert(&self, provider: Arc<dyn Provider>) {
        let mut providers = self.providers.write().unwrap_or_else(|e| e.into_inner());
        match providers.iter_mut().find(|p| p.id() == provider.id()) {
            Some(saved) => *saved = provider,
            None => providers.push(provider),
        }
    }

    pub fn remove(&self, id: &ProviderId) {
        self.providers
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|provider| provider.id() != *id);
    }
}

/// Bind `model_id` to the provider's first API and auth resolver.
pub fn bind_model(
    provider: &dyn Provider,
    model_id: &str,
    options: ModelOptions,
) -> Result<Model, ProviderError> {
    let model_info = ModelInfo::new(model_id, provider.id());
    let api = provider
        .apis()
        .first()
        .map(|(_, api)| api.clone())
        .ok_or_else(|| ProviderError::MissingApi {
            model: model_info.id.clone(),
        })?;

    let ModelOptions {
        server_tools,
        reasoning_effort,
        extensions,
    } = options;

    let options = ModelOptions {
        server_tools: crate::model::supported_server_tools(
            server_tools,
            provider.server_tools(),
            &model_info.provider,
        ),
        reasoning_effort,
        extensions,
    };

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

    fn provider(name: &str, url: &str) -> Arc<dyn Provider> {
        Arc::new(
            CustomProvider::new(
                ProviderId::new(name),
                ApiId::ChatCompletions,
                url,
                Arc::new(InMemoryCredentialStore::new()),
                vec![ModelInfo::new(url, ProviderId::new(name))],
            )
            .unwrap(),
        )
    }

    fn ids_and_models(registry: &ProviderRegistry) -> Vec<(String, String)> {
        registry
            .providers()
            .iter()
            .map(|p| (p.id().0, p.models()[0].id.clone()))
            .collect()
    }

    #[test]
    fn insert_replaces_a_provider_in_place_and_remove_drops_it() {
        let registry = ProviderRegistry::new([provider("openrouter", "a")]);
        registry.insert(provider("grok", "b"));
        registry.insert(provider("groq", "c"));

        registry.insert(provider("grok", "d"));
        assert_eq!(
            ids_and_models(&registry),
            [
                ("openrouter".into(), "a".into()),
                ("grok".into(), "d".into()),
                ("groq".into(), "c".into())
            ]
        );

        registry.remove(&ProviderId::new("grok"));
        assert!(registry.get(&ProviderId::new("grok")).is_none());
        assert!(registry.get(&ProviderId::new("groq")).is_some());
    }
}
