//! UI-agnostic application core. No ratatui/crossterm dependencies here.

pub mod action;
pub mod chat;
pub mod command;
pub mod completion;

pub use action::ImageAttachment;
pub use chat::{Activity, ChatController, Entry, Poll};
pub use command::SlashCommand;
pub use completion::{
    CommandCompleterBackend, CommandsContext, Completer, CompletionRequest, CompletionStatus,
    PathCompleterBackend, PathsContext,
};