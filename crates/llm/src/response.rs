use serde::{Deserialize, Serialize};

use crate::{LlmError, LlmEvent, Message, ToolCall};

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
pub struct LlmResponse {
    pub content: Vec<ContentBlock>,
    pub stop_reason: StopReason,
    pub usage: Option<Usage>,
    pub model: Option<String>,
}

impl LlmResponse {
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

#[derive(Debug, Default)]
pub struct LlmResponseBuilder {
    text: String,
    tool_calls: Vec<Option<PartialToolCall>>,
    stop_reason: Option<StopReason>,
    usage: Option<Usage>,
    model: Option<String>,
}

#[derive(Debug, Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

impl LlmResponseBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply(&mut self, event: &LlmEvent) -> Result<(), LlmError> {
        if self.stop_reason.is_some() {
            return Err(LlmError::InvalidResponse(
                "stream emitted event after completion".into(),
            ));
        }

        match event {
            LlmEvent::TextDelta { text } => self.text.push_str(text),
            LlmEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments,
            } => {
                if self.tool_calls.len() <= *index {
                    self.tool_calls.resize_with(index + 1, || None);
                }
                let call = self.tool_calls[*index].get_or_insert_with(PartialToolCall::default);
                if let Some(id) = id {
                    call.id.push_str(id);
                }
                if let Some(name) = name {
                    call.name.push_str(name);
                }
                call.arguments.push_str(arguments);
            }
            LlmEvent::Done {
                stop_reason,
                usage,
                model,
            } => {
                self.stop_reason = Some(*stop_reason);
                self.usage = usage.clone();
                self.model = model.clone();
            }
        }
        Ok(())
    }

    pub fn finish(self) -> Result<LlmResponse, LlmError> {
        let stop_reason = self.stop_reason.ok_or_else(|| {
            LlmError::InvalidResponse("stream ended without completion event".into())
        })?;

        let mut content = Vec::new();
        if !self.text.is_empty() {
            content.push(ContentBlock::Text(self.text));
        }
        for call in self.tool_calls {
            let call = call.ok_or_else(|| {
                LlmError::InvalidResponse("tool call indexes are not contiguous".into())
            })?;
            if call.id.is_empty() || call.name.is_empty() {
                return Err(LlmError::InvalidResponse(
                    "tool call missing id or name".into(),
                ));
            }
            content.push(ContentBlock::ToolCall(ToolCall {
                id: call.id,
                name: call.name,
                arguments: call.arguments,
            }));
        }

        Ok(LlmResponse {
            content,
            stop_reason,
            usage: self.usage,
            model: self.model,
        })
    }
}
