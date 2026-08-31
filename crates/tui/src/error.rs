//! Framework error type.
//!
//! Component callbacks are infallible in the current API. Component panics
//! unwind normally and are handled by terminal cleanup. Runtime errors come
//! from terminal operations and background tasks.

use std::fmt;

/// No component callback currently returns an error; panics unwind normally
/// and are handled by the terminal panic cleanup. Runtime errors come from
/// terminal operations and background tasks.
#[derive(Debug)]
pub enum RuntimeError {
    /// Terminal setup, draw, or restore failure.
    Terminal(std::io::Error),
    /// An error raised by a background task or command executor.
    Task(Box<dyn std::error::Error + Send + Sync>),
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RuntimeError::Terminal(error) => write!(f, "terminal error: {error}"),
            RuntimeError::Task(error) => write!(f, "task error: {error}"),
        }
    }
}

impl std::error::Error for RuntimeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RuntimeError::Terminal(error) => Some(error),
            RuntimeError::Task(error) => Some(error.as_ref()),
        }
    }
}

impl From<std::io::Error> for RuntimeError {
    fn from(error: std::io::Error) -> Self {
        RuntimeError::Terminal(error)
    }
}
