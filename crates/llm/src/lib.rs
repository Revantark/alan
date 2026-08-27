//! Provider-independent LLM protocol types and API implementations.

mod api;
mod error;
mod event;
mod message;
mod request;
mod response;
mod tool;
mod transport;

pub mod apis;

pub use api::LlmApi;
pub use apis::ChatCompletionsApi;
pub use error::LlmError;
pub use event::{LlmEvent, LlmStream};
pub use message::{ContentPart, ImageUrl, Message, Role};
pub use request::{
    CompletionInput, Credential, LlmRequest, PromptCacheControl, PromptCacheControlType,
    PromptCacheTtl, ReasoningEffort, RequestOptions,
};
pub use response::{ContentBlock, LlmResponse, LlmResponseBuilder, StopReason, Usage};
pub use tool::{ServerTool, ToolCall, ToolDefinition, ToolSpec};
pub use transport::HttpClient;
