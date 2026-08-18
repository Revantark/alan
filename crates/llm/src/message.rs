use serde::{Deserialize, Serialize};

use crate::ToolCall;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_details: Option<Vec<serde_json::Value>>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self::text(Role::System, content)
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::text(Role::User, content)
    }

    pub fn assistant_with_reasoning(
        content: Option<String>,
        reasoning: Option<String>,
        reasoning_details: Vec<serde_json::Value>,
    ) -> Self {
        Self {
            role: Role::Assistant,
            content,
            tool_calls: None,
            tool_call_id: None,
            reasoning,
            reasoning_details: (!reasoning_details.is_empty()).then_some(reasoning_details),
        }
    }

    pub fn assistant_with_tool_calls_and_reasoning(
        content: Option<String>,
        tool_calls: Vec<ToolCall>,
        reasoning: Option<String>,
        reasoning_details: Vec<serde_json::Value>,
    ) -> Self {
        Self {
            role: Role::Assistant,
            content,
            tool_calls: Some(tool_calls),
            tool_call_id: None,
            reasoning,
            reasoning_details: (!reasoning_details.is_empty()).then_some(reasoning_details),
        }
    }

    pub fn tool_result(content: impl Into<String>, tool_call_id: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
            reasoning: None,
            reasoning_details: None,
        }
    }

    fn text(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
            reasoning: None,
            reasoning_details: None,
        }
    }
}
