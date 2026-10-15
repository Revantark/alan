use crate::{
    ApiId, AuthError, AuthMethod, AuthResolver, AuthResult, Credential, CredentialStore, ModelInfo,
    Provider, ProviderError, ProviderId, ServerToolInfo,
};
use async_trait::async_trait;
use llm::{
    ChatCompletionsApi, Credential as RequestCredential, HttpClient, LlmApi, LlmError, LlmRequest,
    LlmStream,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// Where servers report a model's context window. No field is standard, so
/// the first one holding a positive integer wins.
const CONTEXT_FIELDS: [&str; 4] = [
    "context_length",
    "context_window",
    "max_context_length",
    "max_model_len",
];

/// A provider the user adds at runtime: any server that speaks Chat
/// Completions, known by a name and a base URL.
pub struct CustomProvider {
    id: ProviderId,
    api: ApiId,
    /// Shared with the API client, so models already bound to this provider
    /// follow [`set_base_url`](Self::set_base_url), as they follow key changes.
    base_url: Arc<RwLock<String>>,
    models: RwLock<Vec<ModelInfo>>,
    apis: HashMap<ApiId, Arc<dyn LlmApi>>,
    auth: Arc<dyn AuthResolver>,
    fetch_lock: tokio::sync::Mutex<()>,
}

impl CustomProvider {
    /// `models` seeds the catalog so a provider checked a moment ago needs no
    /// second `/models` call; an empty list is filled on the next fetch.
    pub fn new(
        id: ProviderId,
        api: ApiId,
        base_url: impl Into<String>,
        credentials: Arc<dyn CredentialStore>,
        models: Vec<ModelInfo>,
    ) -> Result<Self, ProviderError> {
        let base_url = Arc::new(RwLock::new(base_url.into()));
        let client: Arc<dyn LlmApi> = match &api {
            ApiId::ChatCompletions => Arc::new(LiveUrlApi {
                base_url: Arc::clone(&base_url),
                http: Arc::new(HttpClient::new()),
            }),
            other => return Err(ProviderError::UnsupportedApi(other.clone())),
        };
        Ok(Self {
            auth: Arc::new(StoredKeyAuth {
                provider: id.clone(),
                credentials,
            }),
            apis: HashMap::from([(api.clone(), client)]),
            api,
            models: RwLock::new(models),
            fetch_lock: tokio::sync::Mutex::new(()),
            base_url,
            id,
        })
    }

    /// Point the provider, and every model already bound to it, at `base_url`.
    pub fn set_base_url(&self, base_url: impl Into<String>) {
        *self.base_url.write().unwrap_or_else(|e| e.into_inner()) = base_url.into();
    }

    fn base_url(&self) -> String {
        self.base_url
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// Chat Completions against the provider's URL as it is when each request
/// starts.
struct LiveUrlApi {
    base_url: Arc<RwLock<String>>,
    http: Arc<HttpClient>,
}

#[async_trait]
impl LlmApi for LiveUrlApi {
    async fn stream(&self, request: LlmRequest<'_>) -> Result<LlmStream, LlmError> {
        let base_url = self
            .base_url
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        ChatCompletionsApi::new(base_url, Arc::clone(&self.http))
            .stream(request)
            .await
    }
}

#[async_trait]
impl Provider for CustomProvider {
    fn id(&self) -> ProviderId {
        self.id.clone()
    }

    fn models(&self) -> Vec<ModelInfo> {
        self.models
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn server_tools(&self) -> &[ServerToolInfo] {
        &[]
    }

    fn auth_methods(&self) -> Vec<AuthMethod> {
        vec![AuthMethod::ApiKey]
    }

    fn apis(&self) -> &HashMap<ApiId, Arc<dyn LlmApi>> {
        &self.apis
    }

    fn auth(&self) -> Arc<dyn AuthResolver> {
        self.auth.clone()
    }

    async fn validate_auth(&self, auth_result: &AuthResult) -> Result<(), AuthError> {
        match auth_result {
            AuthResult::ApiKey(key) => {
                fetch_catalog(&self.id, &self.api, &self.base_url(), Some(key))
                    .await
                    .map(drop)
                    .map_err(|error| {
                        AuthError::Validation(match error {
                            ProviderError::Fetch(reason) => reason,
                            other => other.to_string(),
                        })
                    })
            }
        }
    }

    async fn fetch_models(&self) -> Result<(), ProviderError> {
        let _guard = self.fetch_lock.lock().await;
        let credential = self
            .auth
            .resolve()
            .await
            .map_err(|error| ProviderError::Fetch(format!("authentication failed: {error}")))?;
        let key = match credential {
            RequestCredential::ApiKey(key) => Some(key),
            _ => None,
        };
        let models = fetch_catalog(&self.id, &self.api, &self.base_url(), key.as_deref()).await?;
        *self.models.write().unwrap_or_else(|e| e.into_inner()) = models;
        Ok(())
    }
}

/// Reads the provider's key from the credential store. Without one, requests
/// carry no `Authorization` header, which keyless local servers expect.
struct StoredKeyAuth {
    provider: ProviderId,
    credentials: Arc<dyn CredentialStore>,
}

#[async_trait]
impl AuthResolver for StoredKeyAuth {
    async fn resolve(&self) -> Result<RequestCredential, AuthError> {
        Ok(match self.credentials.read(&self.provider).await? {
            Some(Credential::ApiKey { key }) if !key.is_empty() => RequestCredential::ApiKey(key),
            _ => RequestCredential::None,
        })
    }
}

/// The models a server lists. No API standard has a health endpoint, so this
/// is how a URL and key are checked before a provider is saved.
pub async fn fetch_catalog(
    provider: &ProviderId,
    api: &ApiId,
    base_url: &str,
    api_key: Option<&str>,
) -> Result<Vec<ModelInfo>, ProviderError> {
    match api {
        ApiId::ChatCompletions => fetch_chat_completions_catalog(provider, base_url, api_key).await,
        other => Err(ProviderError::UnsupportedApi(other.clone())),
    }
}

/// `GET {base_url}/models` in the Chat Completions format.
async fn fetch_chat_completions_catalog(
    provider: &ProviderId,
    base_url: &str,
    api_key: Option<&str>,
) -> Result<Vec<ModelInfo>, ProviderError> {
    let mut request = reqwest::Client::new()
        .get(format!("{}/models", base_url.trim().trim_end_matches('/')))
        .timeout(std::time::Duration::from_secs(10));
    if let Some(key) = api_key.filter(|key| !key.is_empty()) {
        request = request.bearer_auth(key);
    }
    // reqwest's own message ("error sending request") hides why.
    let response = request.send().await.map_err(|error| {
        ProviderError::Fetch(if error.is_timeout() {
            "timed out".into()
        } else if error.is_connect() {
            "could not connect".into()
        } else if error.is_builder() {
            "invalid URL".into()
        } else {
            format!("request failed: {error}")
        })
    })?;
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(ProviderError::Fetch(format!(
            "API key was rejected (HTTP {status})"
        )));
    }
    if !status.is_success() {
        return Err(ProviderError::Fetch(format!("HTTP {status}")));
    }
    let payload = response
        .text()
        .await
        .map_err(|error| ProviderError::Fetch(format!("request failed: {error}")))?;
    parse_catalog(provider, &payload)
}

fn parse_catalog(provider: &ProviderId, payload: &str) -> Result<Vec<ModelInfo>, ProviderError> {
    #[derive(Deserialize)]
    struct Listing {
        data: Vec<serde_json::Value>,
    }

    let listing: Listing = serde_json::from_str(payload).map_err(|error| {
        ProviderError::Fetch(format!("not an OpenAI-style model list: {error}"))
    })?;
    let mut models: Vec<ModelInfo> = listing
        .data
        .iter()
        .filter_map(|entry| {
            let id = entry.get("id")?.as_str()?.trim();
            (!id.is_empty()).then(|| ModelInfo {
                provider: provider.clone(),
                id: id.to_owned(),
                name: id.to_owned(),
                pricing: None,
                context_length: context_length(entry),
            })
        })
        .collect();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    models.dedup_by(|a, b| a.id == b.id);
    if models.is_empty() {
        return Err(ProviderError::Fetch("server listed no models".into()));
    }
    Ok(models)
}

/// A zero window would paint the context badge full from the first token, so
/// it counts as missing, like a string or a float.
fn context_length(entry: &serde_json::Value) -> Option<u64> {
    CONTEXT_FIELDS
        .iter()
        .find_map(|field| entry.get(field)?.as_u64().filter(|&tokens| tokens > 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InMemoryCredentialStore;

    const PROVIDER: &str = "grok";

    fn id() -> ProviderId {
        ProviderId::new(PROVIDER)
    }

    // one-shot server: replies `response`, hands back the raw request.
    async fn serve_once(response: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = socket.read(&mut buffer).await.unwrap();
                assert!(read > 0);
                request.extend_from_slice(&buffer[..read]);
            }
            socket.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8(request).unwrap()
        });
        (format!("http://{address}/v1/"), server)
    }

    const TWO_MODELS: &str = "HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n{\"data\":[{\"id\":\"m2\"},{\"id\":\"m1\",\"context_window\":8192}]}";

    #[test]
    fn parse_catalog_reads_each_known_context_field() {
        let models = parse_catalog(
            &id(),
            r#"{"data":[
                {"id":"a","context_length":128000},
                {"id":"b","context_window":131072},
                {"id":"c","max_context_length":32768},
                {"id":"d","max_model_len":8192},
                {"id":"e"}]}"#,
        )
        .unwrap();
        let contexts: Vec<_> = models
            .iter()
            .map(|m| (m.id.as_str(), m.context_length))
            .collect();
        assert_eq!(
            contexts,
            [
                ("a", Some(128000)),
                ("b", Some(131072)),
                ("c", Some(32768)),
                ("d", Some(8192)),
                ("e", None)
            ]
        );
        assert!(models.iter().all(|m| m.provider == id() && m.name == m.id));
    }

    #[test]
    fn context_length_takes_the_first_field_present() {
        let models = parse_catalog(
            &id(),
            r#"{"data":[{"id":"a","context_length":1000,"context_window":2000}]}"#,
        )
        .unwrap();
        assert_eq!(models[0].context_length, Some(1000));
    }

    #[test]
    fn context_length_ignores_zero_strings_and_floats() {
        let models = parse_catalog(
            &id(),
            r#"{"data":[
                {"id":"a","context_length":0,"context_window":"4096","max_context_length":4096.5,"max_model_len":2048},
                {"id":"b","context_length":"128k"}]}"#,
        )
        .unwrap();
        assert_eq!(models[0].context_length, Some(2048));
        assert_eq!(models[1].context_length, None);
    }

    #[test]
    fn parse_catalog_sorts_dedupes_and_skips_blank_ids() {
        let models = parse_catalog(
            &id(),
            r#"{"data":[{"id":"b"},{"id":"a","owned_by":"x"},{"id":"b"},{"id":" "},{"object":"model"}]}"#,
        )
        .unwrap();
        let ids: Vec<_> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["a", "b"]);
    }

    #[test]
    fn parse_catalog_rejects_empty_and_other_shapes() {
        assert!(parse_catalog(&id(), r#"{"data":[]}"#).is_err());
        assert!(parse_catalog(&id(), r#"{"models":[{"name":"models/gemini"}]}"#).is_err());
        let error = parse_catalog(&id(), "<html>404</html>").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("not an OpenAI-style model list: "),
            "{error}"
        );
    }

    #[tokio::test]
    async fn unsupported_apis_are_refused_before_any_request() {
        let provider = CustomProvider::new(
            id(),
            ApiId::AnthropicMessages,
            "http://localhost:11434/v1",
            Arc::new(InMemoryCredentialStore::new()),
            Vec::new(),
        );
        assert!(matches!(
            provider,
            Err(ProviderError::UnsupportedApi(ApiId::AnthropicMessages))
        ));

        // "not a url" would fail as "invalid URL" if a request were attempted.
        let error = fetch_catalog(&id(), &ApiId::OpenAiResponses, "not a url", None)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ProviderError::UnsupportedApi(ApiId::OpenAiResponses)
        ));
    }

    #[tokio::test]
    async fn fetch_catalog_sends_no_auth_without_key() {
        let (url, server) = serve_once(TWO_MODELS).await;
        fetch_catalog(&id(), &ApiId::ChatCompletions, &url, None)
            .await
            .unwrap();
        assert!(
            !server
                .await
                .unwrap()
                .to_lowercase()
                .contains("authorization")
        );
    }

    #[tokio::test]
    async fn fetch_catalog_reports_unreachable() {
        // bind then drop: port is closed.
        let address = std::net::TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap();
        let error = fetch_catalog(
            &id(),
            &ApiId::ChatCompletions,
            &format!("http://{address}/v1"),
            None,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("could not connect"), "{error}");
    }

    #[tokio::test]
    async fn fetch_catalog_reports_invalid_url() {
        let error = fetch_catalog(&id(), &ApiId::ChatCompletions, "not a url", None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("invalid URL"), "{error}");
    }

    #[tokio::test]
    async fn fetch_models_uses_the_stored_key() {
        let (url, server) = serve_once(TWO_MODELS).await;
        let credentials = Arc::new(InMemoryCredentialStore::new());
        credentials
            .put(
                &id(),
                Credential::ApiKey {
                    key: "sk-stored".into(),
                },
            )
            .await
            .unwrap();
        let provider =
            CustomProvider::new(id(), ApiId::ChatCompletions, url, credentials, Vec::new())
                .unwrap();
        provider.fetch_models().await.unwrap();
        assert_eq!(provider.models().len(), 2);
        let request = server.await.unwrap();
        assert!(request.starts_with("GET /v1/models HTTP/1.1"), "{request}");
        assert!(
            request
                .to_lowercase()
                .contains("authorization: bearer sk-stored")
        );
    }

    #[tokio::test]
    async fn provider_without_a_key_sends_no_credential() {
        let provider = CustomProvider::new(
            id(),
            ApiId::ChatCompletions,
            "http://localhost:11434/v1",
            Arc::new(InMemoryCredentialStore::new()),
            Vec::new(),
        )
        .unwrap();
        let credential = provider.auth().resolve().await.unwrap();
        assert!(matches!(credential, RequestCredential::None));
    }

    #[tokio::test]
    async fn validate_auth_checks_the_key_against_models() {
        let (url, _server) =
            serve_once("HTTP/1.1 401 Unauthorized\r\nconnection: close\r\n\r\n{}").await;
        let provider = CustomProvider::new(
            id(),
            ApiId::ChatCompletions,
            url,
            Arc::new(InMemoryCredentialStore::new()),
            Vec::new(),
        )
        .unwrap();
        let result = provider
            .validate_auth(&AuthResult::ApiKey("bad".into()))
            .await;
        assert!(
            matches!(&result, Err(AuthError::Validation(reason)) if reason == "API key was rejected (HTTP 401 Unauthorized)"),
            "{result:?}"
        );
    }
}
