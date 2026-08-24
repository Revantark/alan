use crate::{AgentTool, Skill};
use llm::{LlmResponse, Message, ToolCall, Usage};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub enum AgentMessage {
    User(String),
    Assistant(LlmResponse),
    ToolResult {
        tool_call_id: String,
        content: String,
    },
}

/// Serde representation of [`AgentMessage`].
///
/// Tagged explicitly with `kind` so the on-disk format does not depend on
/// Rust enum variant names:
///
/// ```json
/// {"kind":"user","content":"hello"}
/// {"kind":"assistant","response":{...}}
/// {"kind":"tool_result","tool_call_id":"call-1","content":"..."}
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SessionMessage {
    User {
        content: String,
    },
    Assistant {
        response: LlmResponse,
    },
    ToolResult {
        tool_call_id: String,
        content: String,
    },
}

impl From<&AgentMessage> for SessionMessage {
    fn from(message: &AgentMessage) -> Self {
        match message {
            AgentMessage::User(content) => Self::User {
                content: content.clone(),
            },
            AgentMessage::Assistant(response) => Self::Assistant {
                response: response.clone(),
            },
            AgentMessage::ToolResult {
                tool_call_id,
                content,
            } => Self::ToolResult {
                tool_call_id: tool_call_id.clone(),
                content: content.clone(),
            },
        }
    }
}

impl From<SessionMessage> for AgentMessage {
    fn from(message: SessionMessage) -> Self {
        match message {
            SessionMessage::User { content } => Self::User(content),
            SessionMessage::Assistant { response } => Self::Assistant(response),
            SessionMessage::ToolResult {
                tool_call_id,
                content,
            } => Self::ToolResult {
                tool_call_id,
                content,
            },
        }
    }
}

impl Serialize for AgentMessage {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        SessionMessage::from(self).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for AgentMessage {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        SessionMessage::deserialize(deserializer).map(Self::from)
    }
}

impl AgentMessage {
    pub fn user(content: impl Into<String>) -> Self {
        Self::User(content.into())
    }

    pub fn to_llm(&self) -> Message {
        match self {
            Self::User(content) => Message::user(content),
            Self::Assistant(message) => {
                let text = message.text();
                let calls: Vec<ToolCall> = message.tool_calls().cloned().collect();
                if calls.is_empty() {
                    Message::assistant_with_reasoning(
                        (!text.is_empty()).then_some(text),
                        message.reasoning.clone(),
                        message.reasoning_details.clone(),
                    )
                } else {
                    Message::assistant_with_tool_calls_and_reasoning(
                        (!text.is_empty()).then_some(text),
                        calls,
                        message.reasoning.clone(),
                        message.reasoning_details.clone(),
                    )
                }
            }
            Self::ToolResult {
                tool_call_id,
                content,
            } => Message::tool_result(content, tool_call_id),
        }
    }
}

pub struct AgentContext {
    pub system_prompt: Option<String>,
    pub skills: Vec<Skill>,
    pub messages: Vec<AgentMessage>,
    pub usage: Usage,
    pub tools: Vec<AgentTool>,
    pub tool_indexes: HashMap<String, usize>,
}

impl AgentContext {
    pub fn new(system_prompt: Option<String>, skills: Vec<Skill>, tools: Vec<AgentTool>) -> Self {
        let mut tool_indexes = HashMap::with_capacity(tools.len());
        for (index, tool) in tools.iter().enumerate() {
            tool_indexes
                .entry(tool.definition.name.clone())
                .or_insert(index);
        }

        Self {
            system_prompt,
            skills,
            messages: Vec::new(),
            usage: Usage::default(),
            tools,
            tool_indexes,
        }
    }

    /// Hydrate the persistent portions (`messages`, `usage`) of this
    /// runtime context from a session.
    ///
    /// Runtime-only state (tools, system prompt, skills, tool indexes)
    /// remains untouched; a resumed agent must rebuild it from the current
    /// application configuration.
    #[allow(dead_code)]
    pub fn hydrate(&mut self, messages: Vec<AgentMessage>, usage: Usage) {
        self.messages = messages;
        self.usage = usage;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm::{ContentBlock, StopReason};

    #[test]
    fn plain_assistant_message_omits_empty_tool_calls() {
        let assistant = LlmResponse {
            content: vec![ContentBlock::Text("hello".into())],
            stop_reason: StopReason::Stop,
            usage: None,
            model: None,
            reasoning: None,
            reasoning_details: Vec::new(),
        };

        let message = AgentMessage::Assistant(assistant).to_llm();

        assert_eq!(
            message,
            Message::assistant_with_reasoning(Some("hello".into()), None, Vec::new())
        );
        assert!(message.tool_calls.is_none());
    }

    #[test]
    fn plain_assistant_message_preserves_reasoning() {
        let assistant = LlmResponse {
            content: vec![ContentBlock::Text("hello".into())],
            stop_reason: StopReason::Stop,
            usage: None,
            model: None,
            reasoning: Some("thought process".into()),
            reasoning_details: vec![
                serde_json::json!({"type": "reasoning.text", "text": "thought process"}),
            ],
        };

        let message = AgentMessage::Assistant(assistant).to_llm();

        assert_eq!(message.reasoning.as_deref(), Some("thought process"));
        assert_eq!(message.reasoning_details.as_ref().map(|v| v.len()), Some(1));
        assert!(message.tool_calls.is_none());
    }
}
