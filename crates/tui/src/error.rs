//! Framework error type. Expected task errors are delivered to task callbacks;
//! only terminal failures are runtime errors.
use std::fmt;
#[derive(Debug)]
pub enum RuntimeError {
    Terminal(std::io::Error),
}
impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Terminal(e) => write!(f, "terminal error: {e}"),
        }
    }
}
impl std::error::Error for RuntimeError {}
impl From<std::io::Error> for RuntimeError {
    fn from(error: std::io::Error) -> Self {
        Self::Terminal(error)
    }
}
