//! Provider-independent LLM protocol types and API implementations.

mod api;
mod error;
mod message;
mod request;
mod response;
mod tool;
mod transport;

pub mod apis;

pub use api::LlmApi;
pub use apis::ChatCompletionsApi;
pub use error::LlmError;
pub use message::{Message, Role, ToolCall};
pub use request::{CompletionInput, Credential, LlmRequest, RequestOptions};
pub use response::{AssistantMessage, ContentBlock, StopReason, Usage};
pub use tool::ToolDefinition;
pub use transport::HttpClient;
