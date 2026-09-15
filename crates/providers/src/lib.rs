mod auth;
mod catalog;
mod credentials;
mod google;
mod model;
mod openrouter;
mod provider;
mod zai;

pub use auth::{ApiKeyAuth, AuthError, AuthMethod, AuthResolver, AuthResult, CredentialAuth};
pub use catalog::{ApiId, ModelCapabilities, ModelInfo, ModelPricing, ProviderId, ServerToolInfo};
pub use credentials::{
    Credential, CredentialError, CredentialInfo, CredentialKind, CredentialStore,
    FileCredentialStore, InMemoryCredentialStore, SharedCredentialStore,
};
pub use google::{GoogleBuilder, GoogleProvider};
pub use model::{Model, ModelError, ModelOptions};
pub use openrouter::{OpenRouterBuilder, OpenRouterProvider};
pub use provider::{Provider, ProviderError, ProviderRegistry, bind_model};
pub use zai::{ZaiBuilder, ZaiProvider};
