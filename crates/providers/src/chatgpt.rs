//! ChatGPT subscription provider: browser sign-in, token refresh, and a
//! stateless Responses client.
//!
//! This is an **unofficial** client for the backend the Codex CLI uses.
//! OpenAI has not published that backend, so the endpoint, client ID, and
//! header values can change or stop working without notice, and access is
//! tied to the user's own ChatGPT account.
//!
//! Tokens stay on this machine, owner-only, and are never logged or sent
//! anywhere except OpenAI.
//!
//! Sign-in mechanics live in [`crate::oauth`]; this module supplies the
//! ChatGPT configuration and turns issued tokens into stored credentials.

use crate::oauth::{self, OAuthConfig, Tokens, unix_now};
use crate::{
    ApiId, Auth, AuthError, AuthMethod, AuthResolver, Credential, CredentialStore, ModelInfo,
    Provider, ProviderError, ProviderId, Resolved, ServerToolInfo,
};
use async_trait::async_trait;
use constants::{
    ACCOUNT_ID_CLAIM, AUTH_CLAIM_KEY, OPENAI_BETA, ORIGINATOR, PROTOCOL_VERSION, RESPONSES_BASE_URL,
};
use llm::{HttpClient, LlmApi, ResponsesApi};
use std::collections::BTreeMap;
use std::sync::Arc;
use thiserror::Error;

pub fn oauth_config() -> OAuthConfig {
    OAuthConfig {
        authorize_url: constants::AUTHORIZE_URL,
        token_url: constants::TOKEN_URL,
        client_id: constants::CLIENT_ID,
        redirect_uri: constants::REDIRECT_URI,
        redirect_path: constants::REDIRECT_PATH,
        redirect_port: constants::REDIRECT_PORT,
        scopes: constants::SCOPES,
        extra_authorize_params: constants::EXTRA_AUTHORIZE_PARAMS,
    }
}

/// Credential metadata key holding the account id sent as the
/// `ChatGPT-Account-Id` request header.
const ACCOUNT_ID_METADATA_KEY: &str = "account_id";

/// Request header carrying the ChatGPT account id.
const ACCOUNT_ID_HEADER: &str = "ChatGPT-Account-Id";

pub mod constants {
    /// Base URL of the Codex-backed Responses API; `ResponsesApi` appends the
    /// `/responses` endpoint itself.
    pub const RESPONSES_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
    pub const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
    pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
    /// Unpublished client ID shared by Codex-compatible clients.
    pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
    /// Fixed by the registered client; do not pick another port.
    pub const REDIRECT_PORT: u16 = 1455;
    pub const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
    pub const REDIRECT_PATH: &str = "/auth/callback";
    pub const SCOPES: &str = "openid profile email offline_access";
    /// Extra parameters the authorization URL must carry.
    pub const EXTRA_AUTHORIZE_PARAMS: &[(&str, &str)] = &[("idv_captcha", "1")];
    /// JWT claim path holding the ChatGPT account id.
    pub const AUTH_CLAIM_KEY: &str = "https://api.openai.com/auth";
    pub const ACCOUNT_ID_CLAIM: &str = "chatgpt_account_id";
    /// Our own client name; never impersonate the Codex CLI.
    pub const ORIGINATOR: &str = "alan";
    /// Protocol revision sent in the `version` header. The backend gates newer
    /// models on a minimum client version: on 2026-10-08 `gpt-5.6-luna`
    /// rejected the crate version `0.1.0`. `originator` and `User-Agent` still
    /// identify us as `alan`. Bump when the backend demands a newer contract.
    pub const PROTOCOL_VERSION: &str = "0.161.0";
    /// Beta opt-in header sent with every request.
    pub const OPENAI_BETA: &str = "responses=experimental";
}

#[derive(Debug, Error)]
pub enum ChatGptError {
    #[error("sign-in failed: {0}")]
    SignIn(String),
    #[error("no ChatGPT account id found in the issued tokens")]
    MissingAccount,
}

/// Extract the ChatGPT account id from a JWT payload. The signature is not
/// verified: the token was just issued to us over TLS, and the value only ever
/// becomes a request header, never an authorization decision.
pub fn account_id_from_token(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let decoded = base64::Engine::decode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        payload.as_bytes(),
    )
    .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    claims
        .get(AUTH_CLAIM_KEY)
        .and_then(|auth| auth.get(ACCOUNT_ID_CLAIM))
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// Stored ChatGPT credentials with the account id resolved from the token.
#[derive(Debug, Clone)]
struct OAuthSecrets {
    pub access_token: String,
    pub refresh_token: String,
    pub id_token: String,
    pub expires_at: u64,
    pub account_id: String,
}

impl OAuthSecrets {
    pub fn from_tokens(tokens: Tokens) -> Result<Self, ChatGptError> {
        let account_id = account_id_from_token(&tokens.id_token)
            .or_else(|| account_id_from_token(&tokens.access_token))
            .ok_or(ChatGptError::MissingAccount)?;
        let expires_at = tokens.expires_at();
        Ok(Self {
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            id_token: tokens.id_token,
            expires_at,
            account_id,
        })
    }

    pub fn expired(&self) -> bool {
        unix_now() >= self.expires_at
    }

    /// Serialize the account id into generic credential metadata.
    pub fn to_metadata(&self) -> BTreeMap<String, String> {
        BTreeMap::from([(ACCOUNT_ID_METADATA_KEY.to_string(), self.account_id.clone())])
    }
}

/// Auth resolver for the ChatGPT provider: reads stored OAuth credentials and
/// refreshes them when expired, once even when callers race. Resolving returns
/// the access token plus the `ChatGPT-Account-Id` header as [`Resolved::headers`].
pub struct ChatGptAuth {
    provider: ProviderId,
    store: Arc<dyn CredentialStore>,
    /// Single-flight for refreshes; also serializes store writes.
    refresh_lock: tokio::sync::Mutex<()>,
    /// Performs the refresh-token grant; swappable in tests.
    refresh: Arc<dyn TokenRefresher>,
}

/// Performs the refresh-token grant.
#[async_trait]
pub trait TokenRefresher: Send + Sync {
    async fn refresh(&self, refresh_token: String) -> Result<Tokens, AuthError>;
}

struct LiveTokenRefresher;

#[async_trait]
impl TokenRefresher for LiveTokenRefresher {
    async fn refresh(&self, refresh_token: String) -> Result<Tokens, AuthError> {
        oauth::refresh_tokens(&oauth_config(), &refresh_token)
            .await
            .map_err(|error| AuthError::Validation(error.to_string()))
    }
}

#[async_trait]
impl AuthResolver for ChatGptAuth {
    async fn resolve(&self) -> Result<Resolved, AuthError> {
        let credential = self
            .store
            .read(&self.provider)
            .await?
            .ok_or(AuthError::Missing)?;

        let secrets = match credential {
            Credential::OAuth {
                access_token,
                refresh_token,
                id_token,
                expires_at,
                metadata,
            } => OAuthSecrets {
                access_token,
                refresh_token,
                id_token,
                expires_at,
                account_id: metadata
                    .get(ACCOUNT_ID_METADATA_KEY)
                    .cloned()
                    .ok_or(AuthError::Missing)?,
            },
            Credential::ApiKey { .. } => {
                return Err(AuthError::Missing);
            }
        };

        if secrets.expired() {
            self.refresh(secrets).await
        } else {
            Ok(secrets)
        }
        .map(|secrets| Resolved {
            credential: llm::Credential::ApiKey(secrets.access_token),
            headers: vec![(ACCOUNT_ID_HEADER.to_string(), secrets.account_id)],
        })
    }
}

impl ChatGptAuth {
    pub fn new(provider: ProviderId, store: Arc<dyn CredentialStore>) -> Self {
        Self {
            provider,
            store,
            refresh_lock: tokio::sync::Mutex::new(()),
            refresh: Arc::new(LiveTokenRefresher),
        }
    }

    /// Replace the refresh grant, e.g. for tests.
    pub fn with_refresh(mut self, refresh: Arc<dyn TokenRefresher>) -> Self {
        self.refresh = refresh;
        self
    }

    async fn refresh(&self, stale: OAuthSecrets) -> Result<OAuthSecrets, AuthError> {
        let _guard = self.refresh_lock.lock().await;

        // Another caller may have refreshed while we waited; re-read instead
        // of burning the (single-use) refresh token we hold.
        if let Some(Credential::OAuth {
            access_token,
            refresh_token,
            id_token,
            expires_at,
            metadata,
        }) = self.store.read(&self.provider).await?
            && expires_at > stale.expires_at
        {
            return Ok(OAuthSecrets {
                access_token,
                refresh_token,
                id_token,
                expires_at,
                account_id: metadata
                    .get(ACCOUNT_ID_METADATA_KEY)
                    .cloned()
                    .ok_or(AuthError::Missing)?,
            });
        }

        let tokens = self
            .refresh
            .refresh(stale.refresh_token)
            .await
            .map_err(|error| AuthError::Validation(error.to_string()))?;
        let refreshed = OAuthSecrets::from_tokens(tokens)
            .map_err(|error| AuthError::Validation(error.to_string()))?;

        self.store
            .put(
                &self.provider,
                Credential::OAuth {
                    access_token: refreshed.access_token.clone(),
                    refresh_token: refreshed.refresh_token.clone(),
                    id_token: refreshed.id_token.clone(),
                    expires_at: refreshed.expires_at,
                    metadata: refreshed.to_metadata(),
                },
            )
            .await
            .map_err(AuthError::Storage)?;

        Ok(refreshed)
    }
}

/// The ChatGPT subscription provider.
pub struct ChatGptProvider {
    id: ProviderId,
    models: Vec<ModelInfo>,
    apis: Vec<(ApiId, Arc<dyn LlmApi>)>,
    auth: Arc<ChatGptAuth>,
}

impl ChatGptProvider {
    pub fn from_store(store: Arc<dyn CredentialStore>) -> ChatGptProviderBuilder {
        let auth = Arc::new(ChatGptAuth::new(ProviderId::new("chatgpt"), store));
        ChatGptProviderBuilder {
            auth,
            models: default_models(),
        }
    }

    pub fn builder(auth: Arc<ChatGptAuth>) -> ChatGptProviderBuilder {
        ChatGptProviderBuilder {
            auth,
            models: default_models(),
        }
    }
}

fn default_models() -> Vec<ModelInfo> {
    let models = [
        ("gpt-6-astra", "GPT-6 Astra"),
        ("gpt-6.1-sol", "GPT-6.1 Sol"),
        ("gpt-6-luna", "GPT-6 Luna"),
    ];
    models
        .map(|(id, name)| ModelInfo {
            provider: ProviderId::new("chatgpt"),
            id: id.to_string(),
            name: name.to_string(),
            pricing: None,
            context_length: Some(1_000_000),
        })
        .to_vec()
}

pub struct ChatGptProviderBuilder {
    auth: Arc<ChatGptAuth>,
    models: Vec<ModelInfo>,
}

impl ChatGptProviderBuilder {
    pub fn with_models(mut self, models: impl IntoIterator<Item = ModelInfo>) -> Self {
        self.models = models.into_iter().collect();
        self
    }

    pub fn build(self) -> ChatGptProvider {
        let api: Arc<dyn LlmApi> = Arc::new(
            ResponsesApi::new(RESPONSES_BASE_URL, Arc::new(HttpClient::new()))
                .with_header("Accept", "text/event-stream")
                .with_header("OpenAI-Beta", OPENAI_BETA)
                .with_header("originator", ORIGINATOR)
                .with_header("version", PROTOCOL_VERSION)
                .with_header("User-Agent", concat!("alan/", env!("CARGO_PKG_VERSION"))),
        );
        ChatGptProvider {
            id: ProviderId::new("chatgpt"),
            models: self.models,
            apis: vec![(ApiId::OpenAiResponses, api)],
            auth: self.auth,
        }
    }
}

#[async_trait]
impl Provider for ChatGptProvider {
    fn id(&self) -> ProviderId {
        self.id.clone()
    }

    fn models(&self) -> Vec<ModelInfo> {
        self.models.clone()
    }

    fn server_tools(&self) -> &[ServerToolInfo] {
        &[]
    }

    fn apis(&self) -> &[(ApiId, Arc<dyn LlmApi>)] {
        &self.apis
    }
}

#[async_trait]
impl Auth for ChatGptProvider {
    fn auth_methods(&self) -> Vec<AuthMethod> {
        vec![AuthMethod::OAuth {
            config: oauth_config(),
        }]
    }

    fn auth(&self) -> Arc<dyn AuthResolver> {
        self.auth.clone()
    }

    async fn validate_auth(
        &self,
        auth_result: &crate::auth::AuthResult,
    ) -> Result<(), crate::auth::AuthError> {
        match auth_result {
            // The sign-in flow already proved the credentials, and every
            // request exercises the stored token; a live probe would need
            // another unpublished endpoint.
            crate::auth::AuthResult::OAuth => Ok(()),
            // ChatGPT authenticates with a browser sign-in only.
            crate::auth::AuthResult::ApiKey(_) => Err(crate::auth::AuthError::Missing),
        }
    }

    fn credential_from_tokens(
        &self,
        tokens: crate::oauth::Tokens,
    ) -> Result<Credential, ProviderError> {
        let secrets = OAuthSecrets::from_tokens(tokens)
            .map_err(|error| ProviderError::SignIn(error.to_string()))?;
        let metadata = secrets.to_metadata();
        Ok(Credential::OAuth {
            access_token: secrets.access_token,
            refresh_token: secrets.refresh_token,
            id_token: secrets.id_token,
            expires_at: secrets.expires_at,
            metadata,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InMemoryCredentialStore;
    use std::sync::atomic::{AtomicUsize, Ordering};

    async fn store_with(credential: Credential) -> Arc<dyn CredentialStore> {
        let store = InMemoryCredentialStore::new();
        store
            .put(&ProviderId::new("chatgpt"), credential)
            .await
            .expect("in-memory store write cannot fail");
        Arc::new(store)
    }

    fn oauth_credential(account_id: &str, expires_at: u64) -> Credential {
        Credential::OAuth {
            access_token: "live-access".into(),
            refresh_token: "live-refresh".into(),
            id_token: id_token_for(account_id),
            expires_at,
            metadata: BTreeMap::from([(
                ACCOUNT_ID_METADATA_KEY.to_string(),
                account_id.to_string(),
            )]),
        }
    }

    fn id_token_for(account_id: &str) -> String {
        let claims = serde_json::json!({
            AUTH_CLAIM_KEY: { ACCOUNT_ID_CLAIM: account_id },
        });
        let payload = base64::Engine::encode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            serde_json::to_vec(&claims).unwrap().as_slice(),
        );
        format!("eyJhbGciOiJSUzI1NiJ9.{payload}.not-checked")
    }

    #[test]
    fn oauth_config_matches_the_codex_endpoints() {
        let config = oauth_config();
        assert_eq!(config.authorize_url, constants::AUTHORIZE_URL);
        assert_eq!(config.token_url, constants::TOKEN_URL);
        assert_eq!(config.client_id, constants::CLIENT_ID);
        assert_eq!(config.redirect_uri, constants::REDIRECT_URI);
        assert_eq!(config.redirect_port, constants::REDIRECT_PORT);
        assert_eq!(config.extra_authorize_params, &[("idv_captcha", "1")]);
    }

    #[test]
    fn account_id_is_read_from_the_auth_claim() {
        let token = id_token_for("acc_123");
        assert_eq!(account_id_from_token(&token).as_deref(), Some("acc_123"));
        // Malformed tokens yield None instead of panicking.
        assert_eq!(account_id_from_token("not-a-jwt"), None);
        assert_eq!(
            account_id_from_token("a.eyJzdWIiOiJ4In0.c"),
            None,
            "missing claim must be None"
        );
    }

    #[tokio::test]
    async fn single_flight_refreshes_once_under_concurrency() {
        let calls = Arc::new(AtomicUsize::new(0));
        let refresh_calls = calls.clone();

        struct CountingRefresher {
            calls: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl TokenRefresher for CountingRefresher {
            async fn refresh(&self, _refresh_token: String) -> Result<Tokens, AuthError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                // Widen the race window so concurrent callers pile up behind
                // the single-flight lock.
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                Ok(Tokens {
                    access_token: "fresh-access".into(),
                    refresh_token: "fresh-refresh".into(),
                    id_token: id_token_for("acct-1"),
                    expires_in: 3600,
                })
            }
        }

        let auth = ChatGptAuth::new(
            ProviderId::new("chatgpt"),
            store_with(oauth_credential("acct-1", unix_now().saturating_sub(10))).await,
        )
        .with_refresh(Arc::new(CountingRefresher {
            calls: refresh_calls.clone(),
        }));

        let auth = Arc::new(auth);
        let mut tasks = Vec::new();
        for _ in 0..5 {
            let auth = auth.clone();
            tasks.push(tokio::spawn(async move { auth.resolve().await }));
        }
        let mut resolved = Vec::new();
        for task in tasks {
            resolved.push(task.await.unwrap().unwrap());
        }

        // Every caller sees the same fresh token, and the grant ran once.
        assert!(resolved.iter().all(
            |r| matches!(&r.credential, llm::Credential::ApiKey(value) if value == "fresh-access")
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Each resolution carries the account id as a request header.
        assert!(
            resolved
                .iter()
                .all(|r| r.headers == vec![(ACCOUNT_ID_HEADER.to_string(), "acct-1".to_string())])
        );
    }

    #[tokio::test]
    async fn valid_credentials_resolve_without_refreshing() {
        let calls = Arc::new(AtomicUsize::new(0));

        struct CountingRefresher {
            calls: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl TokenRefresher for CountingRefresher {
            async fn refresh(&self, _refresh_token: String) -> Result<Tokens, AuthError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(Tokens {
                    access_token: "should-not-be-used".into(),
                    refresh_token: "x".into(),
                    id_token: id_token_for("acct-1"),
                    expires_in: 3600,
                })
            }
        }

        let credential = oauth_credential("acct-1", unix_now() + 3600);
        let auth = ChatGptAuth::new(ProviderId::new("chatgpt"), store_with(credential).await)
            .with_refresh(Arc::new(CountingRefresher {
                calls: calls.clone(),
            }));

        let resolved = auth.resolve().await.unwrap();
        assert!(matches!(
            resolved.credential,
            llm::Credential::ApiKey(value) if value == "live-access"
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            resolved.headers,
            vec![(ACCOUNT_ID_HEADER.to_string(), "acct-1".to_string())]
        );
    }

    #[tokio::test]
    async fn credentials_without_account_metadata_are_an_error() {
        let credential = Credential::OAuth {
            access_token: "live-access".into(),
            refresh_token: "live-refresh".into(),
            id_token: id_token_for("acct-1"),
            expires_at: unix_now() + 3600,
            metadata: BTreeMap::new(),
        };
        let auth = ChatGptAuth::new(ProviderId::new("chatgpt"), store_with(credential).await);
        let error = auth.resolve().await.unwrap_err();
        assert!(error.to_string().contains("not configured"));
    }

    #[tokio::test]
    async fn missing_credentials_are_an_error() {
        let auth = ChatGptAuth::new(
            ProviderId::new("chatgpt"),
            Arc::new(InMemoryCredentialStore::new()),
        );
        let error = auth.resolve().await.unwrap_err();
        assert!(error.to_string().contains("not configured"));
    }

    #[test]
    fn oauth_secrets_resolve_account_and_margin() {
        let tokens = Tokens {
            access_token: "a".into(),
            refresh_token: "r".into(),
            id_token: id_token_for("acct-9"),
            expires_in: 7200,
        };
        let secrets = OAuthSecrets::from_tokens(tokens).unwrap();
        assert_eq!(secrets.account_id, "acct-9");
        assert!(!secrets.expired());
        // The refresh margin is applied: expires_at < now + expires_in.
        assert!(secrets.expires_at < unix_now() + 7200);
        // The account id survives the metadata round trip.
        let metadata = secrets.to_metadata();
        assert_eq!(
            metadata.get(ACCOUNT_ID_METADATA_KEY).map(String::as_str),
            Some("acct-9")
        );
    }

    #[test]
    fn missing_account_id_fails_clearly() {
        let tokens = Tokens {
            access_token: "a".into(),
            refresh_token: "r".into(),
            // JWT without the account claim.
            id_token: "e30.x.e30".into(),
            expires_in: 7200,
        };
        let error = OAuthSecrets::from_tokens(tokens).unwrap_err();
        assert!(error.to_string().contains("account id"));
    }
}
