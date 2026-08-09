use serde::{Deserialize, Serialize};

use crate::{Message, ToolCall};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContentBlock {
    Text(String),
    ToolCall(ToolCall),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopReason {
    Stop,
    ToolUse,
    Length,
    ContentFilter,
    Error,
    Aborted,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub content: Vec<ContentBlock>,
    pub stop_reason: StopReason,
    pub usage: Option<Usage>,
    pub model: Option<String>,
}

impl AssistantMessage {
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.as_str()),
                ContentBlock::ToolCall(_) => None,
            })
            .collect()
    }

    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.content.iter().filter_map(|block| match block {
            ContentBlock::ToolCall(call) => Some(call),
            ContentBlock::Text(_) => None,
        })
    }

    pub fn from_message(message: Message) -> Self {
        let mut content = Vec::new();
        if let Some(text) = message.content.filter(|text| !text.is_empty()) {
            content.push(ContentBlock::Text(text));
        }
        if let Some(calls) = message.tool_calls {
            content.extend(calls.into_iter().map(ContentBlock::ToolCall));
        }
        let stop_reason = if content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolCall(_)))
        {
            StopReason::ToolUse
        } else {
            StopReason::Stop
        };
        Self {
            content,
            stop_reason,
            usage: None,
            model: None,
        }
    }
}
