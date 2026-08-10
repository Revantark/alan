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
pub use message::{Message, Role, ToolCall};
pub use request::{CompletionInput, Credential, LlmRequest, RequestOptions};
pub use response::{ContentBlock, LlmResponse, LlmResponseBuilder, StopReason, Usage};
pub use tool::ToolDefinition;
pub use transport::HttpClient;
