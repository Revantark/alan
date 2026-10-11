mod auth;
mod catalog;
mod chatgpt;
mod credentials;
mod deepseek;
mod google;
mod local;
mod model;
mod oauth;
mod openrouter;
mod provider;
mod store;
mod zai;

pub use deepseek::{DeepSeekBuilder, DeepSeekProvider};

pub use auth::{
    ApiKeyAuth, AuthError, AuthMethod, AuthResolver, AuthResult, CredentialAuth, NoAuth, Resolved,
};
pub use catalog::{ApiId, ModelCapabilities, ModelInfo, ModelPricing, ProviderId, ServerToolInfo};
pub use chatgpt::{
    ChatGptAuth, ChatGptError, ChatGptProvider, ChatGptProviderBuilder, account_id_from_token,
    constants,
};
pub use credentials::{
    Credential, CredentialError, CredentialInfo, CredentialKind, CredentialStore,
    FileCredentialStore, InMemoryCredentialStore, SharedCredentialStore,
};
pub use google::{GoogleBuilder, GoogleProvider};
pub use local::{LocalApi, LocalModelEntry, LocalProvider, bind_local_model, list_local_models};
pub use model::{Model, ModelError, ModelOptions};
pub use oauth::{
    OAuthConfig, OAuthError, SignInStart, Tokens as OAuthTokens, exchange_code,
    finish_sign_in as finish_oauth_sign_in, parse_callback, refresh_tokens,
    start_sign_in as start_oauth_sign_in,
};
pub use openrouter::{OpenRouterBuilder, OpenRouterProvider, Options as OpenRouterOptions};
pub use provider::{Auth, Provider, ProviderError, ProviderRegistry, bind_model};
pub use store::LocalModelStore;
pub use zai::{ZaiBuilder, ZaiProvider};
