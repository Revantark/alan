use crate::{LlmError, LlmEvent, Message, ToolCall};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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
    pub total_tokens: Option<u64>,
    pub cost: Option<f64>,
    pub upstream_inference_cost: Option<f64>,
    pub cached_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub audio_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmResponse {
    pub content: Vec<ContentBlock>,
    pub stop_reason: StopReason,
    pub usage: Option<Usage>,
    pub model: Option<String>,
    pub reasoning: Option<String>,
    #[serde(default)]
    pub reasoning_details: Vec<serde_json::Value>,
}

fn add<T: std::ops::Add<Output = T> + Copy>(a: Option<T>, b: Option<T>) -> Option<T> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x + y),
        (a, None) => a,
        (None, b) => b,
    }
}

impl Usage {
    pub fn accumulate(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;

        self.total_tokens = add(self.total_tokens, other.total_tokens);
        self.cost = add(self.cost, other.cost);
        self.upstream_inference_cost =
            add(self.upstream_inference_cost, other.upstream_inference_cost);
        self.cached_tokens = add(self.cached_tokens, other.cached_tokens);
        self.cache_write_tokens = add(self.cache_write_tokens, other.cache_write_tokens);
        self.reasoning_tokens = add(self.reasoning_tokens, other.reasoning_tokens);
        self.audio_tokens = add(self.audio_tokens, other.audio_tokens);
    }
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
            reasoning: message.reasoning,
            reasoning_details: message.reasoning_details.unwrap_or_default(),
        }
    }
}

#[derive(Debug, Default)]
pub struct LlmResponseBuilder {
    text: String,
    reasoning: String,
    reasoning_details: Vec<serde_json::Value>,
    tool_calls: BTreeMap<usize, PartialToolCall>,
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
            LlmEvent::ReasoningDelta { reasoning, details } => {
                self.reasoning.push_str(reasoning);
                self.reasoning_details.extend(details.iter().cloned());
            }
            LlmEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments,
            } => {
                let call = self.tool_calls.entry(*index).or_default();
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
        for (_, call) in self.tool_calls {
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
            reasoning: (!self.reasoning.is_empty()).then_some(self.reasoning),
            reasoning_details: self.reasoning_details,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn done() -> LlmEvent {
        LlmEvent::Done {
            stop_reason: StopReason::ToolUse,
            usage: None,
            model: None,
        }
    }

    #[test]
    fn accepts_sparse_provider_tool_indexes() {
        let mut builder = LlmResponseBuilder::new();
        builder
            .apply(&LlmEvent::ToolCallDelta {
                index: 1,
                id: Some("call-1".into()),
                name: Some("bash".into()),
                arguments: "{}".into(),
            })
            .unwrap();
        builder.apply(&done()).unwrap();

        let response = builder.finish().unwrap();
        let calls: Vec<_> = response.tool_calls().collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call-1");
    }

    #[test]
    fn emits_sparse_tool_indexes_in_provider_order() {
        let mut builder = LlmResponseBuilder::new();
        for (index, id) in [(3, "call-3"), (1, "call-1")] {
            builder
                .apply(&LlmEvent::ToolCallDelta {
                    index,
                    id: Some(id.into()),
                    name: Some("bash".into()),
                    arguments: "{}".into(),
                })
                .unwrap();
        }
        builder.apply(&done()).unwrap();

        let response = builder.finish().unwrap();
        let ids: Vec<_> = response.tool_calls().map(|call| call.id.as_str()).collect();
        assert_eq!(ids, ["call-1", "call-3"]);
    }
}
