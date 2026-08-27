//! UI-agnostic application core. No ratatui/crossterm dependencies here.

pub mod action;
pub mod chat;
pub mod command;
pub mod completion;
pub mod controller;
pub mod login;

pub use action::{Action, Command, ImageAttachment};
pub use chat::Entry;
pub use command::SlashCommand;
#[cfg(test)]
pub use completion::DirEntry;
pub use completion::{CompletionController, CompletionState, CompletionStatus};
pub use controller::{Controller, Overlay, Poll};
pub use login::LoginState;
