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
    #[error("this provider does not support browser sign-in")]
    NoBrowserSignIn,
    #[error("browser sign-in failed: {0}")]
    SignIn(#[source] Box<dyn std::error::Error + Send + Sync>),
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

    /// One-line notice shown before a browser sign-in starts, e.g. an
    /// unofficial-client disclaimer. `None` renders nothing.
    fn sign_in_notice(&self) -> Option<&'static str> {
        None
    }

    /// Bind the provider's redirect port and build the authorization URL.
    /// The default implementation rejects: only a provider that declares
    /// [`crate::AuthMethod::OAuth`] supports the browser flow.
    async fn start_sign_in(&self) -> Result<crate::oauth::SignInStart, ProviderError> {
        Err(ProviderError::NoBrowserSignIn)
    }

    /// Open the browser, await the callback, exchange the code, and return the
    /// credential to store.
    async fn finish_sign_in(
        &self,
        _start: crate::oauth::SignInStart,
    ) -> Result<crate::Credential, ProviderError> {
        Err(ProviderError::NoBrowserSignIn)
    }

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
