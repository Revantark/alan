use thiserror::Error;

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("configuration error: {0}")]
    Configuration(String),
    #[error("transport error: {0}")]
    Transport(#[source] reqwest::Error),
    #[error("HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error("serialization error: {0}")]
    Serialization(#[source] serde_json::Error),
    #[error("invalid response: {0}")]
    InvalidResponse(String),
}
