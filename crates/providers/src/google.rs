use crate::{
    ApiId, ApiKeyAuth, AuthError, AuthResolver, CredentialAuth, ModelInfo, ModelPricing, Provider,
    ProviderError, ProviderId, ServerToolInfo,
};
use async_trait::async_trait;
use llm::{HttpClient, InteractionsApi, LlmApi};
use reqwest::StatusCode;
use std::{collections::HashMap, sync::Arc};

const BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

pub struct GoogleProvider {
    id: ProviderId,
    models: std::sync::RwLock<Vec<ModelInfo>>,
    server_tools: Vec<ServerToolInfo>,
    apis: HashMap<ApiId, Arc<dyn LlmApi>>,
    auth: Arc<dyn AuthResolver>,
}

impl GoogleProvider {
    pub fn builder(api_key: impl Into<String>) -> GoogleBuilder {
        GoogleBuilder {
            api_key: api_key.into(),
            models: Vec::new(),
            api: None,
            auth: None,
        }
    }

    pub fn from_store(store: Arc<dyn crate::CredentialStore>) -> GoogleBuilder {
        GoogleBuilder::from_auth(Arc::new(CredentialAuth::new(
            ProviderId::new("google"),
            store,
            Some("GEMINI_API_KEY"),
        )))
        .with_models(vec![
            ModelInfo {
                provider: ProviderId::new("google"),
                id: "gemini-3.1-flash-lite".to_string(),
                name: "Gemini 3.1 Flash-Lite".to_string(),
                pricing: Some(ModelPricing {
                    input_cost_per_token: 0.0,
                    output_cost_per_token: 0.0,
                }),
                context_length: Some(1048576),
            },
            ModelInfo {
                provider: ProviderId::new("google"),
                id: "gemini-3.5-flash-lite".to_string(),
                name: "Gemini 3.5 Flash-Lite".to_string(),
                pricing: Some(ModelPricing {
                    input_cost_per_token: 0.0,
                    output_cost_per_token: 0.0,
                }),
                context_length: Some(1048576),
            },
        ])
    }
}

async fn validate_api_key(key: &str) -> Result<(), AuthError> {
    let response = reqwest::Client::new()
        .get(format!("{BASE_URL}/models"))
        .header("X-goog-api-key", key)
        .send()
        .await
        .map_err(|error| AuthError::Validation(format!("request failed: {error}")))?;

    if response.status() == StatusCode::UNAUTHORIZED
        || response.status() == StatusCode::FORBIDDEN
        || response.status() == StatusCode::BAD_REQUEST
    {
        return Err(AuthError::Validation("API key was rejected".into()));
    }
    if !response.status().is_success() {
        return Err(AuthError::Validation(format!(
            "Google returned HTTP {}",
            response.status()
        )));
    }
    Ok(())
}

#[async_trait]
impl Provider for GoogleProvider {
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
        // Models are pre-configured in from_store, no need to fetch
        Ok(())
    }
}

pub struct GoogleBuilder {
    api_key: String,
    models: Vec<ModelInfo>,
    api: Option<Arc<dyn LlmApi>>,
    auth: Option<Arc<dyn AuthResolver>>,
}

impl GoogleBuilder {
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
            ProviderId::new("google"),
            store,
            Some("GEMINI_API_KEY"),
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

    pub fn build(self) -> Result<GoogleProvider, ProviderError> {
        let api = self.api.unwrap_or_else(|| {
            Arc::new(InteractionsApi::new(BASE_URL, Arc::new(HttpClient::new())))
        });
        let auth = self
            .auth
            .unwrap_or_else(|| Arc::new(ApiKeyAuth::new(self.api_key)));
        Ok(GoogleProvider {
            id: ProviderId::new("google"),
            models: std::sync::RwLock::new(self.models),
            server_tools: vec![],
            apis: HashMap::from([(ApiId::Custom("interactions".to_string()), api)]),
            auth,
        })
    }
}
