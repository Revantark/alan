use super::store::StoreError;
use std::path::PathBuf;
use thiserror::Error;

/// Errors from session persistence on top of the raw [`super::store`].
#[derive(Debug, Error)]
pub enum SessionError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("invalid session header in {path}: {reason}")]
    InvalidHeader { path: PathBuf, reason: String },
    #[error("unsupported session schema version {version} in {path} (supported: {supported})")]
    UnsupportedVersion {
        version: u16,
        supported: u16,
        path: PathBuf,
    },
    #[error("malformed session record in {path}: {reason}")]
    MalformedRecord { path: PathBuf, reason: String },
    #[error("failed to serialize session record for {path}: {source}")]
    Serialize {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}
