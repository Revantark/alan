use crate::{LlmError, StopReason, Usage};
use futures_util::stream::BoxStream;

pub type LlmStream = BoxStream<'static, Result<LlmEvent, LlmError>>;

#[derive(Debug, Clone, PartialEq)]
pub enum LlmEvent {
    TextDelta {
        text: String,
    },
    ToolCallDelta {
        index: usize,
        id: Option<String>,
        name: Option<String>,
        arguments: String,
    },
    Done {
        stop_reason: StopReason,
        usage: Option<Usage>,
        model: Option<String>,
    },
}
