use crate::{
    ApiId, Auth, AuthError, AuthMethod, AuthResolver, AuthResult, Credential, CredentialStore,
    ModelInfo, Provider, ProviderError, ProviderId, Resolved, ServerToolInfo,
};
use async_trait::async_trait;
use llm::apis::chat_completions::{BaseRequest, DefaultChatCompletionsCodec};
use llm::{
    ChatCompletionsApi, CodecChunk, Credential as RequestCredential, HttpClient, LlmApi,
    LlmApiCodec, LlmError, LlmRequest,
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
    base_url: String,
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
        let base_url = base_url.into();
        let client: Arc<dyn LlmApi> = match &api {
            ApiId::ChatCompletions => Arc::new(
                ChatCompletionsApi::new(base_url.clone(), Arc::new(HttpClient::new()))
                    .with_codec(Arc::new(PlainChatCompletionsCodec)),
            ),
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
}

/// The canonical body without OpenRouter's `reasoning` object and the cache
/// fields: strict servers such as OpenAI, Mistral and Groq refuse any request
/// carrying a field they do not know.
struct PlainChatCompletionsCodec;

impl LlmApiCodec for PlainChatCompletionsCodec {
    fn request(&self, request: &LlmRequest<'_>) -> Result<String, LlmError> {
        let mut json =
            serde_json::to_value(BaseRequest::new(request)).map_err(LlmError::Serialization)?;
        if let Some(object) = json.as_object_mut() {
            for field in ["reasoning", "prompt_cache_key", "cache_control"] {
                object.remove(field);
            }
        }
        serde_json::to_string(&json).map_err(LlmError::Serialization)
    }

    fn response(&self, data: &str) -> Result<CodecChunk, LlmError> {
        DefaultChatCompletionsCodec.response(data)
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

    fn apis(&self) -> &HashMap<ApiId, Arc<dyn LlmApi>> {
        &self.apis
    }

    async fn fetch_models(&self) -> Result<(), ProviderError> {
        let _guard = self.fetch_lock.lock().await;
        let credential = self
            .auth
            .resolve()
            .await
            .map_err(|error| ProviderError::Fetch(format!("authentication failed: {error}")))?
            .credential;
        let key = match credential {
            RequestCredential::ApiKey(key) => Some(key),
            _ => None,
        };
        let models = fetch_catalog(&self.id, &self.api, &self.base_url, key.as_deref()).await?;
        *self.models.write().unwrap_or_else(|e| e.into_inner()) = models;
        Ok(())
    }
}

#[async_trait]
impl Auth for CustomProvider {
    fn auth_methods(&self) -> Vec<AuthMethod> {
        vec![AuthMethod::ApiKey]
    }

    fn auth(&self) -> Arc<dyn AuthResolver> {
        self.auth.clone()
    }

    async fn validate_auth(&self, auth_result: &AuthResult) -> Result<(), AuthError> {
        match auth_result {
            AuthResult::ApiKey(key) => {
                fetch_catalog(&self.id, &self.api, &self.base_url, Some(key))
                    .await
                    .map(drop)
                    .map_err(|error| {
                        AuthError::Validation(match error {
                            ProviderError::Fetch(reason) => reason,
                            other => other.to_string(),
                        })
                    })
            }
            AuthResult::OAuth => Err(AuthError::Missing),
        }
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
    async fn resolve(&self) -> Result<Resolved, AuthError> {
        let credential = match self.credentials.read(&self.provider).await? {
            Some(Credential::ApiKey { key }) if !key.is_empty() => RequestCredential::ApiKey(key),
            _ => RequestCredential::None,
        };
        Ok(Resolved {
            credential,
            headers: Vec::new(),
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
    let base = base_url.trim().trim_end_matches('/');
    let url = format!("{base}/models");
    let mut request = reqwest::Client::new()
        .get(&url)
        .timeout(std::time::Duration::from_secs(10));
    if let Some(key) = api_key.filter(|key| !key.is_empty()) {
        request = request.bearer_auth(key);
    }
    // reqwest's own message ("error sending request") hides why.
    let response = request.send().await.map_err(|error| {
        ProviderError::Fetch(if error.is_timeout() {
            format!("{base} did not answer within 10 seconds")
        } else if error.is_connect() {
            format!("could not reach {base}; is the server running?")
        } else if error.is_builder() {
            format!("\"{base}\" is not a valid URL; it should look like http://localhost:11434/v1")
        } else {
            format!("could not reach {base}: {error}")
        })
    })?;
    let status = response.status();
    match status {
        reqwest::StatusCode::UNAUTHORIZED => {
            return Err(ProviderError::Fetch(format!(
                "the API key was rejected (HTTP {status})"
            )));
        }
        // The usual mistake: a base URL without the `/v1` the API lives under.
        reqwest::StatusCode::NOT_FOUND => {
            return Err(ProviderError::Fetch(format!(
                "{url} was not found; the URL should end where /chat/completions would start, e.g. …/v1"
            )));
        }
        status if !status.is_success() => {
            return Err(ProviderError::Fetch(format!(
                "{url} answered HTTP {status}"
            )));
        }
        _ => {}
    }
    let payload = response
        .text()
        .await
        .map_err(|error| ProviderError::Fetch(format!("could not read {url}: {error}")))?;
    parse_catalog(provider, base, &payload)
}

/// `source` is the base URL, for the error messages.
fn parse_catalog(
    provider: &ProviderId,
    source: &str,
    payload: &str,
) -> Result<Vec<ModelInfo>, ProviderError> {
    #[derive(Deserialize)]
    struct Listing {
        data: Vec<serde_json::Value>,
    }

    let listing: Listing = serde_json::from_str(payload).map_err(|error| {
        ProviderError::Fetch(format!(
            "{source}/models did not return a Chat Completions model list: {error}"
        ))
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
        return Err(ProviderError::Fetch(format!("{source} lists no models")));
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

    const SOURCE: &str = "http://x/v1";

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
            SOURCE,
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
            SOURCE,
            r#"{"data":[{"id":"a","context_length":1000,"context_window":2000}]}"#,
        )
        .unwrap();
        assert_eq!(models[0].context_length, Some(1000));
    }

    #[test]
    fn context_length_ignores_zero_strings_and_floats() {
        let models = parse_catalog(&id(), SOURCE, r#"{"data":[
                {"id":"a","context_length":0,"context_window":"4096","max_context_length":4096.5,"max_model_len":2048},
                {"id":"b","context_length":"128k"}]}"#,
        )
        .unwrap();
        assert_eq!(models[0].context_length, Some(2048));
        assert_eq!(models[1].context_length, None);
    }

    #[test]
    fn parse_catalog_sorts_dedupes_and_skips_blank_ids() {
        let models = parse_catalog(&id(), SOURCE, r#"{"data":[{"id":"b"},{"id":"a","owned_by":"x"},{"id":"b"},{"id":" "},{"object":"model"}]}"#,
        )
        .unwrap();
        let ids: Vec<_> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["a", "b"]);
    }

    #[test]
    fn requests_carry_no_fields_strict_servers_reject() {
        let messages = [llm::Message::user("hi")];
        let options = llm::RequestOptions {
            cache_control: Some(llm::PromptCacheControl::one_hour()),
            ..llm::RequestOptions::default()
        };
        let mut extensions = llm::Extensions::default();
        extensions.insert(llm::SessionId("session".into()));
        let request = LlmRequest {
            model_id: "m1",
            messages: &messages,
            tools: &[],
            options: &options,
            credential: None,
            reasoning_effort: llm::ReasoningEffort::High,
            extensions,
        };

        let json: serde_json::Value =
            serde_json::from_str(&PlainChatCompletionsCodec.request(&request).unwrap()).unwrap();

        let mut keys: Vec<_> = json.as_object().unwrap().keys().collect();
        keys.sort();
        assert_eq!(keys, ["messages", "model", "stream"]);
    }

    #[test]
    fn parse_catalog_rejects_empty_and_other_shapes() {
        let empty = parse_catalog(&id(), SOURCE, r#"{"data":[]}"#).unwrap_err();
        assert!(
            empty.to_string().contains("http://x/v1 lists no models"),
            "{empty}"
        );
        assert!(parse_catalog(&id(), SOURCE, r#"{"models":[{"name":"models/gemini"}]}"#).is_err());
        let error = parse_catalog(&id(), SOURCE, "<html>404</html>").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("http://x/v1/models did not return a Chat Completions model list: "),
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
        assert!(
            error.to_string().contains(&format!(
                "could not reach http://{address}/v1; is the server running?"
            )),
            "{error}"
        );
    }

    #[tokio::test]
    async fn fetch_catalog_reports_invalid_url() {
        let error = fetch_catalog(&id(), &ApiId::ChatCompletions, "not a url", None)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains(
                "\"not a url\" is not a valid URL; it should look like http://localhost:11434/v1"
            ),
            "{error}"
        );
    }

    #[tokio::test]
    async fn fetch_catalog_explains_a_wrong_path() {
        let (url, _server) =
            serve_once("HTTP/1.1 404 Not Found\r\nconnection: close\r\n\r\n").await;
        let error = fetch_catalog(&id(), &ApiId::ChatCompletions, &url, None)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains(
                "/v1/models was not found; the URL should end where /chat/completions would start, e.g. …/v1"
            ),
            "{error}"
        );
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
        let credential = provider.auth().resolve().await.unwrap().credential;
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
            matches!(&result, Err(AuthError::Validation(reason)) if reason == "the API key was rejected (HTTP 401 Unauthorized)"),
            "{result:?}"
        );
    }
}
