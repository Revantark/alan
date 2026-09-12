mod auth;
mod catalog;
mod credentials;
mod model;
mod openrouter;
mod provider;

pub use auth::{ApiKeyAuth, AuthError, AuthMethod, AuthResolver, AuthResult, CredentialAuth};
pub use catalog::{ApiId, ModelCapabilities, ModelInfo, ModelPricing, ProviderId, ServerToolInfo};
pub use credentials::{
    Credential, CredentialError, CredentialInfo, CredentialKind, CredentialStore,
    FileCredentialStore, InMemoryCredentialStore, SharedCredentialStore,
};
pub use model::{Model, ModelError, ModelOptions};
pub use openrouter::{OpenRouterBuilder, OpenRouterProvider};
pub use provider::{Provider, ProviderError, ProviderRegistry, bind_model};
