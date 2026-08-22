//! UI-agnostic application core. No ratatui/crossterm dependencies here.

pub mod action;
pub mod chat;
pub mod completion;
pub mod controller;
pub mod login;

pub use action::{Action, Command};
pub use chat::Entry;
#[cfg(test)]
pub use completion::DirEntry;
pub use completion::{CompletionController, CompletionState, CompletionStatus};
pub use controller::{Controller, Overlay, Poll};
pub use login::LoginState;
