use crate::{Message, ToolSpec};
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    #[default]
    Auto,
    None,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl ReasoningEffort {
    /// Every level, in the order a user interface should offer them.
    pub const ALL: &'static [Self] = &[
        Self::Auto,
        Self::None,
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::XHigh,
        Self::Max,
    ];

    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// The inverse of [`as_str`](Self::as_str), so the two cannot drift.
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|effort| effort.as_str() == raw)
    }
}

impl fmt::Display for ReasoningEffort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod reasoning_effort_tests {
    use super::ReasoningEffort;

    /// `as_str` and the `rename_all` derive spell each level independently, and
    /// a settings file written by one is read by the other.
    #[test]
    fn every_level_is_spelled_the_same_by_serde() {
        for effort in ReasoningEffort::ALL {
            let written = serde_json::to_string(effort).expect("serialize");
            assert_eq!(written, format!("\"{}\"", effort.as_str()));
            assert_eq!(ReasoningEffort::parse(effort.as_str()), Some(*effort));
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credential {
    ApiKey(String),
    BearerToken(String),
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptCacheControlType {
    Ephemeral,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PromptCacheTtl {
    #[serde(rename = "1h")]
    OneHour,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptCacheControl {
    #[serde(rename = "type")]
    pub kind: PromptCacheControlType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl: Option<PromptCacheTtl>,
}

impl PromptCacheControl {
    pub const fn five_minutes() -> Self {
        Self {
            kind: PromptCacheControlType::Ephemeral,
            ttl: None,
        }
    }

    pub const fn one_hour() -> Self {
        Self {
            kind: PromptCacheControlType::Ephemeral,
            ttl: Some(PromptCacheTtl::OneHour),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RequestOptions {
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    /// OpenRouter sticky-session key for keeping a conversation on one provider.
    pub session_id: Option<String>,
    /// cache routing key for requests sharing a prefix.
    pub prompt_cache_key: Option<String>,
    /// Provider-translated cache breakpoint/TTL configuration.
    pub cache_control: Option<PromptCacheControl>,
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
    pub reasoning_effort: ReasoningEffort,
}
