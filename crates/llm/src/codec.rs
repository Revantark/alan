use crate::{LlmError, LlmEvent, LlmRequest, Usage};

/// One decoded SSE data frame from a provider stream.
#[derive(Debug)]
pub struct CodecChunk {
    pub model: Option<String>,
    pub finish_reason: Option<String>,
    pub usage: Option<Usage>,
    /// Deltas carried by this frame, ready to emit in order.
    pub events: Vec<LlmEvent>,
}

/// Translates between canonical [`LlmRequest`]s / [`LlmEvent`]s and a
/// provider's wire format.
///
/// An API's default codec speaks the canonical protocol shape. Providers with
/// extras implement this trait and attach their codec to the API (see
/// [`ChatCompletionsApi::with_codec`](crate::ChatCompletionsApi::with_codec));
/// provider-specific request options arrive inside
/// [`LlmRequest::extensions`](crate::LlmRequest::extensions).
///
/// Wire request/response structs stay private to each codec: the API only
/// needs a serialized body and decoded chunks.
pub trait LlmApiCodec: Send + Sync {
    /// Serialize the wire body for `request`.
    fn request(&self, request: &LlmRequest<'_>) -> Result<String, LlmError>;

    /// Decode one SSE `data:` frame from the provider stream.
    fn response(&self, data: &str) -> Result<CodecChunk, LlmError>;
}
