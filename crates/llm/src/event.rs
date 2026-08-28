use crate::{LlmError, StopReason, Usage};
use futures_util::stream::BoxStream;

pub type LlmStream = BoxStream<'static, Result<LlmEvent, LlmError>>;

#[derive(Debug, Clone, PartialEq)]
pub enum LlmEvent {
    TextDelta {
        text: String,
    },
    ReasoningDelta {
        reasoning: String,
        details: Vec<serde_json::Value>,
    },
    ToolCallDelta {
        index: usize,
        id: Option<String>,
        name: Option<String>,
        arguments: String,
    },
    /// Emitted as soon as a chunk carries token usage, before the stream
    /// completes. Useful for live cost/token display without waiting for
    /// [`Done`](LlmEvent::Done).
    Usage {
        usage: Usage,
    },
    Done {
        stop_reason: StopReason,
        usage: Option<Usage>,
        model: Option<String>,
    },
}
