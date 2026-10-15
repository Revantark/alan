mod auth;
mod catalog;
mod credentials;
mod custom;
mod deepseek;
mod google;
mod model;
mod openrouter;
mod provider;
mod zai;

pub use custom::{CustomProvider, fetch_catalog};
pub use deepseek::{DeepSeekBuilder, DeepSeekProvider};

pub use auth::{ApiKeyAuth, AuthError, AuthMethod, AuthResolver, AuthResult, CredentialAuth};
pub use catalog::{ApiId, ModelCapabilities, ModelInfo, ModelPricing, ProviderId, ServerToolInfo};
pub use credentials::{
    Credential, CredentialError, CredentialInfo, CredentialKind, CredentialStore,
    FileCredentialStore, InMemoryCredentialStore, SharedCredentialStore,
};
pub use google::{GoogleBuilder, GoogleProvider};
pub use model::{Model, ModelError, ModelOptions};
pub use openrouter::{OpenRouterBuilder, OpenRouterProvider, Options as OpenRouterOptions};
pub use provider::{Provider, ProviderError, ProviderRegistry, bind_model};
pub use zai::{ZaiBuilder, ZaiProvider};
