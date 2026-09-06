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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthResult {
    ApiKey(String),
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

#[async_trait]
pub trait AuthResolver: Send + Sync {
    async fn resolve(&self) -> Result<RequestCredential, AuthError>;
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
    async fn resolve(&self) -> Result<RequestCredential, AuthError> {
        if self.key.is_empty() {
            Err(AuthError::Missing)
        } else {
            Ok(RequestCredential::ApiKey(self.key.clone()))
        }
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
    async fn resolve(&self) -> Result<RequestCredential, AuthError> {
        if let Some(credential) = self.store.read(&self.provider).await? {
            return match credential {
                Credential::ApiKey { key } if !key.is_empty() => Ok(RequestCredential::ApiKey(key)),
                Credential::ApiKey { .. } => Err(AuthError::Missing),
            };
        }

        if let Some(variable) = self.environment_variable
            && let Ok(key) = env::var(variable)
            && !key.is_empty()
        {
            return Ok(RequestCredential::ApiKey(key));
        }

        Err(AuthError::Missing)
    }
}
