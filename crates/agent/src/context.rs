use llm::{LlmResponse, Message, ToolCall};

#[derive(Debug, Clone, PartialEq)]
pub enum AgentMessage {
    User(String),
    Assistant(LlmResponse),
    ToolResult {
        tool_call_id: String,
        content: String,
    },
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
