use crate::provider::bind_model;
use crate::{
    ApiId, ApiKeyAuth, AuthError, AuthEvent, AuthInteraction, AuthPrompt, AuthResolver, Credential,
    CredentialAuth, Model, ModelCapabilities, ModelInfo, ModelPricing, Provider, ProviderAuth,
    ProviderError, ProviderId,
};
use async_trait::async_trait;
use llm::{ChatCompletionsApi, HttpClient, LlmApi};
use reqwest::StatusCode;
use std::{collections::HashMap, sync::Arc};

const BASE_URL: &str = "https://openrouter.ai/api/v1";

pub struct OpenRouterProvider {
    id: ProviderId,
    models: Vec<ModelInfo>,
    apis: HashMap<ApiId, Arc<dyn LlmApi>>,
    auth: Arc<dyn AuthResolver>,
    login: OpenRouterAuth,
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

pub struct OpenRouterAuth;

#[async_trait]
impl ProviderAuth for OpenRouterAuth {
    async fn login(&self, interaction: &mut dyn AuthInteraction) -> Result<Credential, AuthError> {
        let value = interaction
            .prompt(AuthPrompt::Secret {
                message: "OpenRouter API key".into(),
            })
            .await?;
        let key = value.trim();
        if key.is_empty() {
            return Err(AuthError::Validation("API key cannot be empty".into()));
        }

        interaction.notify(AuthEvent::Progress("Validating OpenRouter API key".into()));
        validate_api_key(key).await?;
        Ok(Credential::ApiKey { key: key.into() })
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

    fn auth(&self) -> &dyn ProviderAuth {
        &self.login
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
            login: OpenRouterAuth,
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
