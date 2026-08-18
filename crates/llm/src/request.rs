use crate::{Message, ToolSpec};
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl ReasoningEffort {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }
}

impl fmt::Display for ReasoningEffort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credential {
    ApiKey(String),
    BearerToken(String),
    None,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RequestOptions {
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
}

pub struct CompletionInput<'a> {
    pub messages: &'a [Message],
    pub tools: &'a [ToolSpec],
    pub options: &'a RequestOptions,
}

pub struct LlmRequest<'a> {
    pub model_id: &'a str,
    pub messages: &'a [Message],
    pub tools: &'a [ToolSpec],
    pub options: &'a RequestOptions,
    pub credential: Option<&'a Credential>,
    pub reasoning_effort: Option<ReasoningEffort>,
}
