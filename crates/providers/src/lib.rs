mod auth;
mod catalog;
mod credentials;
mod deepseek;
mod google;
mod local;
mod model;
mod openrouter;
mod provider;
mod store;
mod zai;

pub use deepseek::{DeepSeekBuilder, DeepSeekProvider};

pub use auth::{
    ApiKeyAuth, AuthError, AuthMethod, AuthResolver, AuthResult, CredentialAuth, NoAuth,
};
pub use catalog::{ApiId, ModelCapabilities, ModelInfo, ModelPricing, ProviderId, ServerToolInfo};
pub use credentials::{
    Credential, CredentialError, CredentialInfo, CredentialKind, CredentialStore,
    FileCredentialStore, InMemoryCredentialStore, SharedCredentialStore,
};
pub use google::{GoogleBuilder, GoogleProvider};
pub use local::{LocalApi, LocalModelEntry, LocalProvider, bind_local_model, list_local_models};
pub use model::{Model, ModelError, ModelOptions};
pub use openrouter::{OpenRouterBuilder, OpenRouterProvider, Options as OpenRouterOptions};
pub use provider::{Provider, ProviderError, ProviderRegistry, bind_model};
pub use store::LocalModelStore;
pub use zai::{ZaiBuilder, ZaiProvider};
