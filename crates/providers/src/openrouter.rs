use crate::model::ModelOptions;
use crate::provider::bind_model;
use crate::{
    ApiId, ApiKeyAuth, AuthError, AuthResolver, CredentialAuth, Model, ModelCapabilities,
    ModelInfo, ModelPricing, Provider, ProviderError, ProviderId, ServerToolInfo,
};
use async_trait::async_trait;
use llm::{ChatCompletionsApi, HttpClient, LlmApi, ReasoningEffort};
use reqwest::StatusCode;
use serde::Deserialize;
use std::{collections::HashMap, sync::Arc};

const BASE_URL: &str = "https://openrouter.ai/api/v1";

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
    fn id(&self) -> &ProviderId {
        &self.id
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

    fn bind(&self, model_id: &str) -> Result<Model, ProviderError> {
        bind_model(
            self.models
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .as_slice(),
            &self.apis,
            self.auth.clone(),
            model_id,
            ModelOptions::default(),
        )
    }

    fn bind_with_options(
        &self,
        model_id: &str,
        options: ModelOptions,
    ) -> Result<Model, ProviderError> {
        bind_model(
            self.models
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .as_slice(),
            &self.apis,
            self.auth.clone(),
            model_id,
            options,
        )
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

    pub fn with_model(mut self, model_id: impl Into<String>) -> Self {
        let id = model_id.into();
        self.models.push(default_model(&id));
        self
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
        if self.models.is_empty() {
            return Err(ProviderError::ModelNotFound("no models configured".into()));
        }
        let api = self.api.unwrap_or_else(|| {
            Arc::new(ChatCompletionsApi::new(
                BASE_URL,
                Arc::new(HttpClient::new()),
            ))
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
        let capabilities = ModelCapabilities {
            streaming: true,
            tools: true,
            vision: entry
                .supported_parameters
                .contains(&"image_input".to_string()),
            reasoning: if entry
                .supported_parameters
                .contains(&"reasoning".to_string())
            {
                Some(ReasoningEffort::Medium)
            } else {
                None
            },
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
            api: ApiId::ChatCompletions,
            capabilities,
            pricing,
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
    #[allow(dead_code)]
    #[serde(default)]
    context_length: Option<u64>,
    pricing: Option<CatalogPricing>,
    #[serde(default)]
    supported_parameters: Vec<String>,
}

#[derive(Deserialize)]
struct CatalogPricing {
    prompt: Option<String>,
    completion: Option<String>,
}

fn default_model(id: &str) -> ModelInfo {
    ModelInfo {
        provider: ProviderId::new("openrouter"),
        id: id.into(),
        name: id.into(),
        api: ApiId::ChatCompletions,
        //TODO: Do we need this ?
        capabilities: ModelCapabilities {
            streaming: true,
            tools: true,
            vision: false,
            reasoning: None,
        },
        pricing: Some(ModelPricing::default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InMemoryCredentialStore;

    fn make_provider(models: Vec<ModelInfo>) -> OpenRouterProvider {
        OpenRouterBuilder::from_store(Arc::new(InMemoryCredentialStore::default()))
            .with_models(models)
            .build()
            .expect("build")
    }

    #[test]
    fn bind_after_catalog_replacement_serves_new_model() {
        let provider = make_provider(vec![default_model("m1")]);
        let new_models = vec![default_model("m2")];
        provider
            .models
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clone_from(&new_models);

        provider.bind("m2").expect("m2 should bind");
        assert!(matches!(
            provider.bind("m1"),
            Err(ProviderError::ModelNotFound(_))
        ));
    }

    #[test]
    fn models_reflects_replacement() {
        let provider = make_provider(vec![default_model("m1")]);
        let new_models = vec![default_model("m2")];
        provider
            .models
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clone_from(&new_models);

        let models = provider.models();
        assert_eq!(models.len(), 1);
    }

    #[test]
    fn parses_full_catalog_entry() {
        let json = r#"{
            "data": [{
                "id": "openai/gpt-4o",
                "name": "GPT-4o",
                "pricing": {"prompt": "0.0025", "completion": "0.01"},
                "supported_parameters": ["reasoning", "image_input"]
            }]
        }"#;
        let models = parse_catalog(&ProviderId::new("openrouter"), json).unwrap();
        assert_eq!(models.len(), 1);
        let m = &models[0];
        assert_eq!(m.id, "openai/gpt-4o");
        assert_eq!(m.name, "GPT-4o");
        assert_eq!(m.api, ApiId::ChatCompletions);
        assert!(m.capabilities.streaming);
        assert!(m.capabilities.tools);
        assert!(m.capabilities.vision);
        assert_eq!(m.capabilities.reasoning, Some(ReasoningEffort::Medium));
        let p = m.pricing.as_ref().unwrap();
        assert_eq!(p.input_cost_per_token, 0.0025);
        assert_eq!(p.output_cost_per_token, 0.01);
    }

    #[test]
    fn skips_entries_without_name() {
        let json = r#"{
            "data": [{"id": "x", "name": null}]
        }"#;
        let models = parse_catalog(&ProviderId::new("openrouter"), json).unwrap();
        assert!(models.is_empty());
    }

    #[test]
    fn missing_pricing_becomes_none() {
        let json = r#"{
            "data": [{"id": "x", "name": "X"}]
        }"#;
        let models = parse_catalog(&ProviderId::new("openrouter"), json).unwrap();
        assert_eq!(models.len(), 1);
        assert!(models[0].pricing.is_none());
    }

    #[test]
    fn invalid_json_is_a_fetch_error() {
        let result = parse_catalog(&ProviderId::new("openrouter"), "not json");
        assert!(matches!(result, Err(ProviderError::Fetch(_))));
    }

    #[test]
    fn empty_data_yields_empty_catalog() {
        let json = r#"{"data": []}"#;
        let models = parse_catalog(&ProviderId::new("openrouter"), json).unwrap();
        assert!(models.is_empty());
    }
}
