use crate::ProviderId;
use crate::credentials::{Credential, CredentialError, CredentialStore};
use async_trait::async_trait;
use llm::Credential as RequestCredential;
use std::env;
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthMethod {
    ApiKey,
    OAuth,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthResult {
    ApiKey(String),
    OAuth,
}

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("credential is not configured")]
    Missing,
    #[error("credential validation failed: {0}")]
    Validation(String),
    #[error("credential storage failed: {0}")]
    Storage(#[from] CredentialError),
}

/// Credentials resolved for one request: the request credential plus any
/// provider-specific headers to send (for example a ChatGPT account id learned
/// at sign-in). Resolving is a pure read — callers attach the headers to the
/// request; nothing is mutated in shared state.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub credential: RequestCredential,
    pub headers: Vec<(String, String)>,
}

#[async_trait]
pub trait AuthResolver: Send + Sync {
    async fn resolve(&self) -> Result<Resolved, AuthError>;
}

pub struct ApiKeyAuth {
    key: String,
}

impl ApiKeyAuth {
    pub fn new(key: impl Into<String>) -> Self {
        Self { key: key.into() }
    }
}

#[async_trait]
impl AuthResolver for ApiKeyAuth {
    async fn resolve(&self) -> Result<Resolved, AuthError> {
        if self.key.is_empty() {
            Err(AuthError::Missing)
        } else {
            Ok(Resolved {
                credential: RequestCredential::ApiKey(self.key.clone()),
                headers: Vec::new(),
            })
        }
    }
}

pub struct NoAuth;

#[async_trait]
impl AuthResolver for NoAuth {
    async fn resolve(&self) -> Result<Resolved, AuthError> {
        Ok(Resolved {
            credential: RequestCredential::None,
            headers: Vec::new(),
        })
    }
}

pub struct CredentialAuth {
    provider: ProviderId,
    store: Arc<dyn CredentialStore>,
    environment_variable: Option<&'static str>,
}

impl CredentialAuth {
    pub fn new(
        provider: ProviderId,
        store: Arc<dyn CredentialStore>,
        environment_variable: Option<&'static str>,
    ) -> Self {
        Self {
            provider,
            store,
            environment_variable,
        }
    }
}

#[async_trait]
impl AuthResolver for CredentialAuth {
    async fn resolve(&self) -> Result<Resolved, AuthError> {
        if let Some(credential) = self.store.read(&self.provider).await? {
            return match credential {
                Credential::ApiKey { key } if !key.is_empty() => Ok(Resolved {
                    credential: RequestCredential::ApiKey(key),
                    headers: Vec::new(),
                }),
                Credential::ApiKey { .. } => Err(AuthError::Missing),
                // OAuth credentials are resolved by their provider's own
                // resolver (`ChatGptAuth` for ChatGPT), never by this
                // API-key resolver.
                Credential::OAuth { .. } => Err(AuthError::Missing),
            };
        }

        if let Some(variable) = self.environment_variable
            && let Ok(key) = env::var(variable)
            && !key.is_empty()
        {
            return Ok(Resolved {
                credential: RequestCredential::ApiKey(key),
                headers: Vec::new(),
            });
        }

        Err(AuthError::Missing)
    }
}
