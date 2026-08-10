use crate::provider::bind_model;
use crate::{
    ApiId, ApiKeyAuth, AuthResolver, Model, ModelCapabilities, ModelInfo, ModelPricing, Provider,
    ProviderError, ProviderId,
};
use llm::{ChatCompletionsApi, HttpClient, LlmApi};
use std::{collections::HashMap, sync::Arc};

const BASE_URL: &str = "https://openrouter.ai/api/v1";

pub struct OpenRouterProvider {
    id: ProviderId,
    models: Vec<ModelInfo>,
    apis: HashMap<ApiId, Arc<dyn LlmApi>>,
    auth: Arc<dyn AuthResolver>,
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
}

impl Provider for OpenRouterProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }
    fn models(&self) -> &[ModelInfo] {
        &self.models
    }
    fn bind(&self, model_id: &str) -> Result<Model, ProviderError> {
        bind_model(&self.models, &self.apis, self.auth.clone(), model_id)
    }
}

pub struct OpenRouterBuilder {
    api_key: String,
    models: Vec<ModelInfo>,
    api: Option<Arc<dyn LlmApi>>,
    auth: Option<Arc<dyn AuthResolver>>,
}

impl OpenRouterBuilder {
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
            models: self.models,
            apis: HashMap::from([(ApiId::ChatCompletions, api)]),
            auth,
        })
    }
}

fn default_model(id: &str) -> ModelInfo {
    ModelInfo {
        provider: ProviderId::new("openrouter"),
        id: id.into(),
        name: id.into(),
        api: ApiId::ChatCompletions,
        capabilities: ModelCapabilities {
            streaming: true,
            tools: true,
            vision: false,
            reasoning: false,
        },
        pricing: Some(ModelPricing::default()),
    }
}
