use llm::{AssistantMessage, Message, ToolCall};

#[derive(Debug, Clone, PartialEq)]
pub enum AgentMessage {
    User(String),
    Assistant(AssistantMessage),
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
                    Message::assistant(text)
                } else {
                    Message::assistant_with_tool_calls((!text.is_empty()).then_some(text), calls)
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
        let assistant = AssistantMessage {
            content: vec![ContentBlock::Text("hello".into())],
            stop_reason: StopReason::Stop,
            usage: None,
            model: None,
        };

        let message = AgentMessage::Assistant(assistant).to_llm();

        assert_eq!(message, Message::assistant("hello"));
        assert!(message.tool_calls.is_none());
    }
}
