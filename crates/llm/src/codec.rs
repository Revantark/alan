use crate::{LlmError, LlmEvent, LlmRequest, Usage};

/// One decoded Chat Completions chunk, before stream metadata is accumulated.
#[derive(Debug)]
pub struct CodecChunk {
    pub model: Option<String>,
    pub finish_reason: Option<String>,
    pub usage: Option<Usage>,
    /// Deltas carried by this frame, ready to emit in order.
    pub events: Vec<LlmEvent>,
}

/// Translates requests and events to and from an API's wire format.
///
/// Codecs are shared across requests; each stream gets its own decoder.
/// Provider-specific options arrive in [`LlmRequest::extensions`].
pub trait LlmApiCodec: Send + Sync {
    /// Serialize the wire body for `request`.
    fn request(&self, request: &LlmRequest<'_>) -> Result<String, LlmError>;

    /// Create a response decoder for one SSE stream.
    fn decoder(&self) -> Box<dyn LlmStreamDecoder>;
}

/// Decodes SSE `data:` frames; owns protocol state and terminal-event handling.
pub trait LlmStreamDecoder: Send {
    /// Whether the terminal frame has been seen. No further frames are read
    /// after this returns `true`.
    fn done(&self) -> bool;

    /// Translate one `data:` frame into events, in emission order.
    fn frame(&mut self, data: &str) -> Result<Vec<LlmEvent>, LlmError>;

    /// Settle the stream at end of input, or report a missing terminal frame.
    fn finish(&mut self) -> Result<Vec<LlmEvent>, LlmError>;
}
