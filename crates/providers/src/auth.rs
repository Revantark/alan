use crate::ProviderId;
use crate::credentials::{Credential, CredentialError, CredentialStore};
use async_trait::async_trait;
use llm::Credential as RequestCredential;
use std::env;
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum InteractionError {
    #[error("authentication interaction cancelled")]
    Cancelled,
    #[error("authentication interaction failed: {0}")]
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthPrompt {
    Secret {
        message: String,
    },
    Text {
        message: String,
    },
    Select {
        message: String,
        options: Vec<AuthOption>,
    },
    ManualCode {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthOption {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthEvent {
    Info(String),
    Progress(String),
    AuthUrl {
        url: String,
        instructions: Option<String>,
    },
}

#[async_trait]
pub trait AuthInteraction: Send {
    async fn prompt(&mut self, prompt: AuthPrompt) -> Result<String, InteractionError>;

    fn notify(&mut self, event: AuthEvent);
}

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("credential is not configured")]
    Missing,
    #[error("authentication interaction failed: {0}")]
    Interaction(#[from] InteractionError),
    #[error("credential validation failed: {0}")]
    Validation(String),
    #[error("credential storage failed: {0}")]
    Storage(#[from] CredentialError),
}

#[async_trait]
pub trait ProviderAuth: Send + Sync {
    async fn login(&self, interaction: &mut dyn AuthInteraction) -> Result<Credential, AuthError>;
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
