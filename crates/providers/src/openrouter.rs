use crate::{
    ApiId, ApiKeyAuth, AuthError, AuthResolver, CredentialAuth, ModelInfo, ModelPricing, Provider,
    ProviderError, ProviderId, ServerToolInfo,
};
use async_trait::async_trait;
use llm::apis::chat_completions::{
    BaseRequest, BaseResponse, WireProvider, WireUsage, decode_stream_response,
};
use llm::{ChatCompletionsApi, CodecChunk, HttpClient, LlmApi, LlmApiCodec, LlmError, LlmRequest};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};

const BASE_URL: &str = "https://openrouter.ai/api/v1";

/// OpenRouter's `/chat/completions` wire format: the canonical body plus
/// provider routing and a sticky-session key.
struct OpenRouterCodec;

/// OpenRouter routing options, attached to a request's extensions by the
/// application.
pub struct Options {
    /// Providers to prioritize, in order.
    pub provider_order: Vec<String>,
}

#[derive(Serialize)]
struct OpenRouterRequest<'a> {
    #[serde(flatten)]
    base: BaseRequest<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<WireProvider>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
}

#[derive(Deserialize)]
struct OpenRouterStreamChunk {
    model: Option<String>,
    choices: Vec<llm::apis::chat_completions::StreamChoice>,
    usage: Option<OpenRouterUsage>,
}

/// OpenRouter extends the standard usage object with cost fields.
#[derive(Deserialize)]
struct OpenRouterUsage {
    #[serde(flatten)]
    base: WireUsage,
    cost: Option<f64>,
    cost_details: Option<CostDetails>,
}

#[derive(Deserialize)]
struct CostDetails {
    upstream_inference_cost: Option<f64>,
}

impl LlmApiCodec for OpenRouterCodec {
    fn request(&self, request: &LlmRequest<'_>) -> Result<String, LlmError> {
        let provider = request
            .extensions
            .get::<Options>()
            .map(|options| options.provider_order.as_slice())
            .filter(|order| !order.is_empty())
            .map(|order| WireProvider {
                only: order.to_vec(),
                allow_fallbacks: order.is_empty(),
            });
        let session_id = request.extensions.get::<llm::SessionId>();
        let wire = OpenRouterRequest {
            base: BaseRequest::new(request),
            provider,
            session_id: session_id.map(|session| session.0.as_str()),
        };
        serde_json::to_string(&wire).map_err(LlmError::Serialization)
    }

    fn response(&self, data: &str) -> Result<CodecChunk, LlmError> {
        let chunk: OpenRouterStreamChunk =
            serde_json::from_str(data).map_err(LlmError::Serialization)?;
        let OpenRouterUsage {
            base,
            cost,
            cost_details,
        } = match chunk.usage {
            Some(usage) => usage,
            None => {
                return decode_stream_response(&BaseResponse {
                    model: chunk.model,
                    choices: chunk.choices,
                    usage: None,
                });
            }
        };
        let mut decoded = decode_stream_response(&BaseResponse {
            model: chunk.model,
            choices: chunk.choices,
            usage: Some(base),
        })?;
        if let Some(usage) = &mut decoded.usage {
            usage.cost = cost;
            usage.upstream_inference_cost = cost_details
                .as_ref()
                .and_then(|details| details.upstream_inference_cost);
        }
        Ok(decoded)
    }
}

#[cfg(test)]
mod codec_tests {
    use super::*;
    use llm::{Extensions, Message, RequestOptions};

    fn wire_request(order: Option<Vec<String>>, session: Option<&str>) -> (String, Vec<Message>) {
        let mut extensions = Extensions::default();
        if let Some(order) = order {
            extensions.insert(Options {
                provider_order: order,
            });
        }
        if let Some(session) = session {
            extensions.insert(llm::SessionId(session.to_string()));
        }
        let messages = vec![Message::user("hello")];
        let options = RequestOptions::default();
        let request = LlmRequest {
            model_id: "model-a",
            messages: &messages,
            tools: &[],
            options: &options,
            credential: None,
            reasoning_effort: llm::ReasoningEffort::None,
            extensions,
        };
        let body = OpenRouterCodec.request(&request).unwrap();
        (body, messages)
    }

    #[test]
    fn serializes_provider_order_and_session_id() {
        let (body, _keep) = wire_request(
            Some(vec!["deepseek".into(), "fireworks".into()]),
            Some("s-1"),
        );
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();

        assert_eq!(
            json["provider"]["only"],
            serde_json::json!(["deepseek", "fireworks"])
        );
        assert_eq!(json["session_id"], "s-1");
        assert_eq!(json["model"], "model-a");
    }

    #[test]
    fn omits_provider_and_session_when_unconfigured() {
        let (body, _keep) = wire_request(None, None);
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();

        assert!(json.get("provider").is_none());
        assert!(json.get("session_id").is_none());
    }

    #[test]
    fn fills_cost_from_response() {
        let chunk = OpenRouterCodec
            .response(
                r#"{"choices":[],"usage":{"prompt_tokens":11,"completion_tokens":7,"cost":0.0021,"cost_details":{"upstream_inference_cost":0.0019}}}"#,
            )
            .unwrap();
        let usage = chunk.usage.unwrap();
        assert_eq!(usage.input_tokens, 11);
        assert_eq!(usage.output_tokens, 7);
        assert_eq!(usage.cost, Some(0.0021));
        assert_eq!(usage.upstream_inference_cost, Some(0.0019));
    }

    #[test]
    fn leaves_cost_unset_when_response_has_none() {
        let chunk = OpenRouterCodec
            .response(r#"{"choices":[],"usage":{"prompt_tokens":2,"completion_tokens":3}}"#)
            .unwrap();
        let usage = chunk.usage.unwrap();
        assert_eq!(usage.cost, None);
        assert_eq!(usage.upstream_inference_cost, None);
    }
}

pub struct OpenRouterProvider {
    id: ProviderId,
    models: std::sync::RwLock<Vec<ModelInfo>>,
    server_tools: Vec<ServerToolInfo>,
    apis: HashMap<ApiId, Arc<dyn LlmApi>>,
    auth: Arc<dyn AuthResolver>,
    fetch_lock: tokio::sync::Mutex<()>,
}

impl OpenRouterProvider {
    pub fn builder(api_key: impl Into<String>) -> OpenRouterBuilder {
        OpenRouterBuilder {
            api_key: api_key.into(),
            models: Vec::new(),
            api: None,
            auth: None,
        }
    }

    pub fn from_store(store: Arc<dyn crate::CredentialStore>) -> OpenRouterBuilder {
        OpenRouterBuilder::from_auth(Arc::new(CredentialAuth::new(
            ProviderId::new("openrouter"),
            store,
            Some("OPENROUTER_API_KEY"),
        )))
    }
}

async fn validate_api_key(key: &str) -> Result<(), AuthError> {
    let response = reqwest::Client::new()
        .get("https://openrouter.ai/api/v1/key")
        .bearer_auth(key)
        .send()
        .await
        .map_err(|error| AuthError::Validation(format!("request failed: {error}")))?;

    if response.status() == StatusCode::UNAUTHORIZED {
        return Err(AuthError::Validation("API key was rejected".into()));
    }
    if !response.status().is_success() {
        return Err(AuthError::Validation(format!(
            "OpenRouter returned HTTP {}",
            response.status()
        )));
    }
    Ok(())
}

#[async_trait]
impl Provider for OpenRouterProvider {
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
        &self.server_tools
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
                "OpenRouter returned HTTP {status}"
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

pub struct OpenRouterBuilder {
    api_key: String,
    models: Vec<ModelInfo>,
    api: Option<Arc<dyn LlmApi>>,
    auth: Option<Arc<dyn AuthResolver>>,
}

impl OpenRouterBuilder {
    fn from_auth(auth: Arc<dyn AuthResolver>) -> Self {
        Self {
            api_key: String::new(),
            models: Vec::new(),
            api: None,
            auth: Some(auth),
        }
    }

    #[cfg(test)]
    pub fn from_store(store: Arc<dyn crate::CredentialStore>) -> Self {
        Self::from_auth(Arc::new(CredentialAuth::new(
            ProviderId::new("openrouter"),
            store,
            Some("OPENROUTER_API_KEY"),
        )))
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

    pub fn build(self) -> Result<OpenRouterProvider, ProviderError> {
        let api = self.api.unwrap_or_else(|| {
            Arc::new(
                ChatCompletionsApi::new(BASE_URL, Arc::new(HttpClient::new()))
                    .with_codec(Arc::new(OpenRouterCodec)),
            )
        });
        let auth = self
            .auth
            .unwrap_or_else(|| Arc::new(ApiKeyAuth::new(self.api_key)));
        Ok(OpenRouterProvider {
            id: ProviderId::new("openrouter"),
            models: std::sync::RwLock::new(self.models),
            server_tools: vec![
                ServerToolInfo {
                    id: "openrouter:web_fetch".into(),
                    description: "Fetch a web page".into(),
                },
                ServerToolInfo {
                    id: "openrouter:web_search".into(),
                    description: "Search the web".into(),
                },
            ],
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
        let name = match entry.name {
            Some(name) => name,
            None => continue,
        };
        let pricing = match &entry.pricing {
            Some(p) => {
                let input = p.prompt.as_ref().unwrap().parse::<f64>().ok();
                let output = p.completion.as_ref().unwrap().parse::<f64>().ok();
                match (input, output) {
                    (Some(input), Some(output)) => Some(ModelPricing {
                        input_cost_per_token: input,
                        output_cost_per_token: output,
                    }),
                    _ => None,
                }
            }
            None => None,
        };
        models.push(ModelInfo {
            provider: provider.clone(),
            id: entry.id,
            name,
            pricing,
            context_length: entry.context_length,
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
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    context_length: Option<u64>,
    pricing: Option<CatalogPricing>,
}

#[derive(Deserialize)]
struct CatalogPricing {
    prompt: Option<String>,
    completion: Option<String>,
}
