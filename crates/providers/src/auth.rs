use async_trait::async_trait;
use llm::Credential;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("credential is not configured")]
    Missing,
}

#[async_trait]
pub trait AuthResolver: Send + Sync {
    async fn resolve(&self) -> Result<Credential, AuthError>;
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
    async fn resolve(&self) -> Result<Credential, AuthError> {
        if self.key.is_empty() {
            Err(AuthError::Missing)
        } else {
            Ok(Credential::ApiKey(self.key.clone()))
        }
    }
}
