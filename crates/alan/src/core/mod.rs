//! UI-agnostic application core. No ratatui/crossterm dependencies here.

pub mod action;
pub mod chat;
pub mod command;
pub mod completion;
pub mod controller;

pub use action::{Command, ImageAttachment};
pub use chat::Entry;
pub use command::SlashCommand;
pub use completion::{
    CommandCompleterBackend, CommandsContext, Completer, CompletionRequest, CompletionStatus,
    PathCompleterBackend, PathsContext,
};
pub use controller::{Activity, CommandOutcome, Controller, Poll};
