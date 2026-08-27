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
pub use completion::{CompletionController, CompletionItem, CompletionStatus};
pub use controller::{Controller, Overlay, Poll};
pub use login::LoginState;
