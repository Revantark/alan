use llm::ReasoningEffort;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProviderId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ServerToolInfo {
    pub id: String,
    pub description: String,
}

impl ProviderId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ApiId {
    ChatCompletions,
    AnthropicMessages,
    OpenAiResponses,
    Custom(String),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelCapabilities {
    pub streaming: bool,
    pub tools: bool,
    pub vision: bool,
    #[serde(default)]
    pub reasoning: Option<ReasoningEffort>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelPricing {
    pub input_cost_per_token: f64,
    pub output_cost_per_token: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub provider: ProviderId,
    pub id: String,
    pub name: String,
    pub pricing: Option<ModelPricing>,
    pub context_length: Option<u64>,
}

impl ModelInfo {
    pub fn new(model_id: &str, provider_id: ProviderId) -> Self {
        ModelInfo {
            provider: provider_id,
            id: model_id.into(),
            name: model_id.into(),
            pricing: Some(crate::ModelPricing::default()),
            context_length: None,
        }
    }
}
