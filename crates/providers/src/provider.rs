use crate::Model;
use crate::auth::AuthResolver;
use crate::catalog::{ApiId, ModelInfo, ProviderId, ServerToolInfo};
use crate::local::LocalProvider;
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
    #[error("issued OAuth tokens did not contain a usable credential: {0}")]
    SignIn(String),
}

#[async_trait]
pub trait Auth: Send + Sync {
    /// The auth resolver used to obtain credentials for requests.
    fn auth(&self) -> Arc<dyn AuthResolver>;

    /// The authentication methods this provider supports.
    fn auth_methods(&self) -> Vec<crate::auth::AuthMethod>;

    /// Validate a credential the user just supplied: an API key, or the
    /// result of a browser sign-in.
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

pub struct ProviderRegistry {
    providers: Vec<Arc<dyn Provider>>,
    local_provider: Option<Arc<LocalProvider>>,
}

impl ProviderRegistry {
    pub fn new(providers: impl IntoIterator<Item = Arc<dyn Provider>>) -> Self {
        Self {
            providers: providers.into_iter().collect(),
            local_provider: None,
        }
    }

    pub fn with_local_provider(
        providers: impl IntoIterator<Item = Arc<dyn Provider>>,
        local_provider: Arc<LocalProvider>,
    ) -> Self {
        Self {
            providers: providers.into_iter().collect(),
            local_provider: Some(local_provider),
        }
    }

    pub fn providers(&self) -> &[Arc<dyn Provider>] {
        &self.providers
    }

    pub fn get(&self, id: &ProviderId) -> Option<Arc<dyn Provider>> {
        self.providers.iter().find(|p| p.id() == *id).cloned()
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
