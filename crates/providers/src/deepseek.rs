use crate::{
    ApiId, ApiKeyAuth, AuthError, AuthResolver, CredentialAuth, ModelInfo, Provider, ProviderError,
    ProviderId, ServerToolInfo,
};
use async_trait::async_trait;
use llm::apis::chat_completions::{BaseRequest, BaseResponse, decode_stream_response};
use llm::{ChatCompletionsApi, CodecChunk, HttpClient, LlmApi, LlmApiCodec, LlmError, LlmRequest};
use reqwest::StatusCode;
use serde::Deserialize;
use std::{collections::HashMap, sync::Arc};

const BASE_URL: &str = "https://api.deepseek.com";

/// DeepSeek's thinking mode: the model emits a chain of thought before the
/// final answer and, when the request carries tools, requires that reasoning
/// to be sent back on subsequent turns.
struct DeepSeekCodec;

/// DeepSeek's reasoning-effort mapping from the requested level.
fn wire_reasoning_effort(effort: llm::ReasoningEffort) -> Option<&'static str> {
    match effort {
        llm::ReasoningEffort::None => None,
        llm::ReasoningEffort::Minimal | llm::ReasoningEffort::Low => Some("low"),
        llm::ReasoningEffort::Medium | llm::ReasoningEffort::High | llm::ReasoningEffort::XHigh => {
            Some("high")
        }
        llm::ReasoningEffort::Max => Some("max"),
    }
}

impl LlmApiCodec for DeepSeekCodec {
    fn request(&self, request: &LlmRequest<'_>) -> Result<String, LlmError> {
        let base = BaseRequest::new(request);
        let mut json = serde_json::to_value(&base).map_err(LlmError::Serialization)?;

        // The canonical body uses the OpenAI `reasoning` object and cache
        // fields DeepSeek does not support; replace them with DeepSeek's
        // thinking toggle and top-level effort.
        let effort = wire_reasoning_effort(request.reasoning_effort);
        let object = json
            .as_object_mut()
            .ok_or_else(|| LlmError::InvalidResponse("request body is not a JSON object".into()))?;
        object.remove("reasoning");
        object.remove("prompt_cache_key");
        object.remove("cache_control");
        object.insert(
            "thinking".into(),
            serde_json::json!({"type": if effort.is_some() { "enabled" } else { "disabled" }}),
        );
        if let Some(effort) = effort {
            object.insert("reasoning_effort".into(), serde_json::json!(effort));
        }

        // Assistant messages may carry reasoning from earlier turns; DeepSeek
        // requires it back when tools are present.
        if !base.tools.is_empty() {
            for (wire, message) in object
                .get_mut("messages")
                .and_then(serde_json::Value::as_array_mut)
                .ok_or_else(|| LlmError::InvalidResponse("messages must be an array".into()))?
                .iter_mut()
                .zip(request.messages)
            {
                if wire["role"] != "assistant" {
                    continue;
                }
                if let Some(reasoning) = &message.reasoning
                    && !reasoning.is_empty()
                {
                    wire["reasoning_content"] = serde_json::Value::String(reasoning.clone());
                }
            }
        }

        serde_json::to_string(&json).map_err(LlmError::Serialization)
    }

    fn response(&self, data: &str) -> Result<CodecChunk, LlmError> {
        let chunk: DeepSeekStreamChunk =
            serde_json::from_str(data).map_err(LlmError::Serialization)?;
        // DeepSeek reports cache hits as flat fields next to the standard
        // usage object; the canonical `WireUsage` ignores unknown fields.
        let cached_tokens = chunk
            .usage
            .as_ref()
            .and_then(|usage| usage.get("prompt_cache_hit_tokens"))
            .and_then(serde_json::Value::as_u64);
        let usage = chunk
            .usage
            .map(Deserialize::deserialize)
            .transpose()
            .map_err(LlmError::Serialization)?;
        let mut decoded = decode_stream_response(&BaseResponse {
            model: chunk.model,
            choices: chunk.choices,
            usage,
        })?;
        if let (Some(cached), Some(usage)) = (cached_tokens, &mut decoded.usage) {
            usage.cached_tokens = Some(cached);
        }
        Ok(decoded)
    }
}

#[derive(Deserialize)]
struct DeepSeekStreamChunk {
    model: Option<String>,
    choices: Vec<llm::apis::chat_completions::StreamChoice>,
    usage: Option<serde_json::Value>,
}

pub struct DeepSeekProvider {
    id: ProviderId,
    models: std::sync::RwLock<Vec<ModelInfo>>,
    apis: HashMap<ApiId, Arc<dyn LlmApi>>,
    auth: Arc<dyn AuthResolver>,
    fetch_lock: tokio::sync::Mutex<()>,
}

impl DeepSeekProvider {
    pub fn builder(api_key: impl Into<String>) -> DeepSeekBuilder {
        DeepSeekBuilder {
            api_key: api_key.into(),
            models: Vec::new(),
            api: None,
            auth: None,
        }
    }

    pub fn from_store(store: Arc<dyn crate::CredentialStore>) -> DeepSeekBuilder {
        DeepSeekBuilder::from_auth(Arc::new(CredentialAuth::new(
            ProviderId::new("deepseek"),
            store,
            Some("DEEPSEEK_API_KEY"),
        )))
    }
}

async fn validate_api_key(key: &str) -> Result<(), AuthError> {
    let response = reqwest::Client::new()
        .get(format!("{BASE_URL}/models"))
        .bearer_auth(key)
        .send()
        .await
        .map_err(|error| AuthError::Validation(format!("request failed: {error}")))?;

    if response.status() == StatusCode::UNAUTHORIZED {
        return Err(AuthError::Validation("API key was rejected".into()));
    }
    if !response.status().is_success() {
        return Err(AuthError::Validation(format!(
            "DeepSeek returned HTTP {}",
            response.status()
        )));
    }
    Ok(())
}

#[async_trait]
impl Provider for DeepSeekProvider {
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

    fn auth_methods(&self) -> Vec<crate::auth::AuthMethod> {
        vec![crate::auth::AuthMethod::ApiKey]
    }

    fn apis(&self) -> &HashMap<ApiId, Arc<dyn LlmApi>> {
        &self.apis
    }

    fn auth(&self) -> Arc<dyn AuthResolver> {
        self.auth.clone()
    }

    async fn validate_auth(
        &self,
        auth_result: &crate::auth::AuthResult,
    ) -> Result<(), crate::auth::AuthError> {
        match auth_result {
            crate::auth::AuthResult::ApiKey(key) => validate_api_key(key).await,
        }
    }

    async fn fetch_models(&self) -> Result<(), ProviderError> {
        let _guard = self.fetch_lock.lock().await;
        let response = reqwest::Client::new()
            .get(format!("{BASE_URL}/models"))
            .send()
            .await
            .map_err(|error| ProviderError::Fetch(format!("request failed: {error}")))?;
        let status = response.status();
        if !status.is_success() {
            return Err(ProviderError::Fetch(format!(
                "DeepSeek returned HTTP {status}"
            )));
        }
        let payload = response
            .text()
            .await
            .map_err(|error| ProviderError::Fetch(format!("request failed: {error}")))?;
        let models = parse_catalog(&self.id, &payload)?;
        *self.models.write().unwrap_or_else(|e| e.into_inner()) = models;
        Ok(())
    }
}

pub struct DeepSeekBuilder {
    api_key: String,
    models: Vec<ModelInfo>,
    api: Option<Arc<dyn LlmApi>>,
    auth: Option<Arc<dyn AuthResolver>>,
}

impl DeepSeekBuilder {
    fn from_auth(auth: Arc<dyn AuthResolver>) -> Self {
        Self {
            api_key: String::new(),
            models: Vec::new(),
            api: None,
            auth: Some(auth),
        }
    }

    pub fn with_models(mut self, models: impl IntoIterator<Item = ModelInfo>) -> Self {
        self.models.extend(models);
        self
    }

    pub fn with_api(mut self, api: Arc<dyn LlmApi>) -> Self {
        self.api = Some(api);
        self
    }

    pub fn with_auth(mut self, auth: Arc<dyn AuthResolver>) -> Self {
        self.auth = Some(auth);
        self
    }

    pub fn build(self) -> Result<DeepSeekProvider, ProviderError> {
        let api = self.api.unwrap_or_else(|| {
            Arc::new(
                ChatCompletionsApi::new(BASE_URL, Arc::new(HttpClient::new()))
                    .with_codec(Arc::new(DeepSeekCodec)),
            )
        });
        let auth = self
            .auth
            .unwrap_or_else(|| Arc::new(ApiKeyAuth::new(self.api_key)));
        Ok(DeepSeekProvider {
            id: ProviderId::new("deepseek"),
            models: std::sync::RwLock::new(self.models),
            apis: HashMap::from([(ApiId::ChatCompletions, api)]),
            auth,
            fetch_lock: tokio::sync::Mutex::new(()),
        })
    }
}

fn parse_catalog(provider: &ProviderId, payload: &str) -> Result<Vec<ModelInfo>, ProviderError> {
    let catalog: CatalogResponse =
        serde_json::from_str(payload).map_err(|error| ProviderError::Fetch(error.to_string()))?;
    let mut models = Vec::new();
    for entry in catalog.data {
        let name = entry.name.unwrap_or_else(|| entry.id.clone());
        models.push(ModelInfo {
            provider: provider.clone(),
            id: entry.id,
            name,
            pricing: None,
            context_length: entry.context_window,
        });
    }
    Ok(models)
}

#[derive(Deserialize)]
struct CatalogResponse {
    data: Vec<CatalogModel>,
}

#[derive(Deserialize)]
struct CatalogModel {
    id: String,
    name: Option<String>,
    context_window: Option<u64>,
}

#[cfg(test)]
mod codec_tests {
    use super::*;
    use llm::{Extensions, Message, ReasoningEffort, RequestOptions, ToolDefinition, ToolSpec};

    fn wire_request(
        effort: ReasoningEffort,
        messages: Vec<Message>,
        tools: &[ToolSpec],
    ) -> serde_json::Value {
        let options = RequestOptions::default();
        let request = LlmRequest {
            model_id: "deepseek-flash",
            messages: &messages,
            tools,
            options: &options,
            credential: None,
            reasoning_effort: effort,
            extensions: Extensions::default(),
        };
        serde_json::from_str(&DeepSeekCodec.request(&request).unwrap()).unwrap()
    }

    #[test]
    fn maps_thinking_and_effort_per_level() {
        for (effort, thinking, wire_effort) in [
            (ReasoningEffort::None, "disabled", None),
            (ReasoningEffort::Low, "enabled", Some("low")),
            (ReasoningEffort::Medium, "enabled", Some("high")),
            (ReasoningEffort::Max, "enabled", Some("max")),
        ] {
            let json = wire_request(effort, vec![Message::user("hi")], &[]);
            assert_eq!(json["thinking"]["type"], thinking);
            match wire_effort {
                Some(level) => assert_eq!(json["reasoning_effort"], level),
                None => assert!(json.get("reasoning_effort").is_none()),
            }
            assert!(json.get("reasoning").is_none(), "canonical field removed");
            assert!(
                json.get("prompt_cache_key").is_none(),
                "DeepSeek has no prompt_cache_key"
            );
        }
    }

    #[test]
    fn sends_reasoning_content_back_only_with_tools() {
        let messages = vec![Message::assistant_with_reasoning(
            Some("answer".into()),
            Some("chain of thought".into()),
            Vec::new(),
        )];
        let with_tools = wire_request(
            ReasoningEffort::High,
            messages.clone(),
            &[ToolSpec::Function(ToolDefinition {
                name: "bash".into(),
                description: "run".into(),
                parameters: serde_json::json!({"type": "object"}),
            })],
        );
        assert_eq!(
            with_tools["messages"][0]["reasoning_content"],
            "chain of thought"
        );

        let without_tools = wire_request(ReasoningEffort::High, messages, &[]);
        assert!(
            without_tools["messages"][0]
                .get("reasoning_content")
                .is_none()
        );
    }

    #[test]
    fn fills_cached_tokens_from_cache_hit_field() {
        let chunk = DeepSeekCodec
            .response(
                r#"{"choices":[],"usage":{"prompt_tokens":20,"completion_tokens":5,
                    "prompt_cache_hit_tokens":14,"prompt_cache_miss_tokens":6}}"#,
            )
            .unwrap();
        let usage = chunk.usage.unwrap();
        assert_eq!(usage.input_tokens, 20);
        assert_eq!(usage.cached_tokens, Some(14));
    }
}

#[cfg(test)]
mod catalog_tests {
    use super::*;

    const SAMPLE: &str = r#"{"object": "list", "data": [{
        "id": "deepseek-flash", "object": "model", "owned_by": "deepseek",
        "name": "DeepSeek-V4.1-Flash",
        "context_window": 1048576, "max_output_tokens": 393216,
        "input_modalities": ["text", "image"], "output_modalities": ["text"],
        "effort": {"supported_levels": ["low", "high", "max"], "default_level": "high"}
    }]}"#;

    #[test]
    fn parses_sample_catalog_entry() {
        let models = parse_catalog(&ProviderId::new("deepseek"), SAMPLE).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "deepseek-flash");
        assert_eq!(models[0].name, "DeepSeek-V4.1-Flash");
        assert_eq!(models[0].context_length, Some(1048576));
        assert!(models[0].pricing.is_none());
        assert_eq!(models[0].provider, ProviderId::new("deepseek"));
    }

    #[test]
    fn rejects_invalid_payload() {
        assert!(parse_catalog(&ProviderId::new("deepseek"), "not json").is_err());
    }
}
