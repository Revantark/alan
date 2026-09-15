use crate::{
    ApiId, ApiKeyAuth, AuthError, AuthResolver, CredentialAuth, ModelInfo, ModelPricing, Provider,
    ProviderError, ProviderId, ServerToolInfo,
};
use async_trait::async_trait;
use llm::{ChatCompletionsApi, HttpClient, LlmApi};
use reqwest::StatusCode;
use std::{collections::HashMap, sync::Arc};

const BASE_URL: &str = "https://api.z.ai/api/paas/v4";

pub struct ZaiProvider {
    id: ProviderId,
    models: std::sync::RwLock<Vec<ModelInfo>>,
    server_tools: Vec<ServerToolInfo>,
    apis: HashMap<ApiId, Arc<dyn LlmApi>>,
    auth: Arc<dyn AuthResolver>,
}

impl ZaiProvider {
    pub fn builder(api_key: impl Into<String>) -> ZaiBuilder {
        ZaiBuilder {
            api_key: api_key.into(),
            models: Vec::new(),
            api: None,
            auth: None,
        }
    }

    pub fn from_store(store: Arc<dyn crate::CredentialStore>) -> ZaiBuilder {
        ZaiBuilder::from_auth(Arc::new(CredentialAuth::new(
            ProviderId::new("zai"),
            store,
            Some("ZAI_API_KEY"),
        )))
        .with_models(vec![
            ModelInfo {
                provider: ProviderId::new("zai"),
                id: "glm-5.3-flash".to_string(),
                name: "GLM-5.3-Flash".to_string(),
                pricing: Some(ModelPricing {
                    input_cost_per_token: 0.15,
                    output_cost_per_token: 0.50,
                }),
                context_length: Some(1000000),
            },
            ModelInfo {
                provider: ProviderId::new("zai"),
                id: "glm-5.3".to_string(),
                name: "GLM-5.3".to_string(),
                pricing: Some(ModelPricing {
                    input_cost_per_token: 1.4,
                    output_cost_per_token: 4.4,
                }),
                context_length: Some(1000000),
            },
            ModelInfo {
                provider: ProviderId::new("zai"),
                id: "glm-5.2".to_string(),
                name: "GLM-5.2".to_string(),
                pricing: Some(ModelPricing {
                    input_cost_per_token: 1.4,
                    output_cost_per_token: 4.4,
                }),
                context_length: Some(1000000),
            },
            ModelInfo {
                provider: ProviderId::new("zai"),
                id: "glm-4.7-flash".to_string(),
                name: "GLM-4.7-Flash".to_string(),
                pricing: Some(ModelPricing {
                    input_cost_per_token: 0.0,
                    output_cost_per_token: 0.0,
                }),
                context_length: Some(200000),
            },
            ModelInfo {
                provider: ProviderId::new("zai"),
                id: "glm-4.5-flash".to_string(),
                name: "GLM-4.5-Flash".to_string(),
                pricing: Some(ModelPricing {
                    input_cost_per_token: 0.0,
                    output_cost_per_token: 0.0,
                }),
                context_length: Some(200000),
            },
        ])
    }
}

async fn validate_api_key(key: &str) -> Result<(), AuthError> {
    let response = reqwest::Client::new()
        .get("https://api.z.ai/api/v1/key")
        .bearer_auth(key)
        .send()
        .await
        .map_err(|error| AuthError::Validation(format!("request failed: {error}")))?;

    if response.status() == StatusCode::UNAUTHORIZED {
        return Err(AuthError::Validation("API key was rejected".into()));
    }
    if !response.status().is_success() {
        return Err(AuthError::Validation(format!(
            "Zai returned HTTP {}",
            response.status()
        )));
    }
    Ok(())
}

#[async_trait]
impl Provider for ZaiProvider {
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

pub struct ZaiBuilder {
    api_key: String,
    models: Vec<ModelInfo>,
    api: Option<Arc<dyn LlmApi>>,
    auth: Option<Arc<dyn AuthResolver>>,
}

impl ZaiBuilder {
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
            ProviderId::new("zai"),
            store,
            Some("ZAI_API_KEY"),
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

    pub fn build(self) -> Result<ZaiProvider, ProviderError> {
        let api = self.api.unwrap_or_else(|| {
            Arc::new(ChatCompletionsApi::new(
                BASE_URL,
                Arc::new(HttpClient::new()),
            ))
        });
        let auth = self
            .auth
            .unwrap_or_else(|| Arc::new(ApiKeyAuth::new(self.api_key)));
        Ok(ZaiProvider {
            id: ProviderId::new("zai"),
            models: std::sync::RwLock::new(self.models),
            server_tools: vec![],
            apis: HashMap::from([(ApiId::ChatCompletions, api)]),
            auth,
        })
    }
}
