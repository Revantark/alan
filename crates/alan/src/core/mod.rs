//! UI-agnostic application core. No ratatui/crossterm dependencies here.

pub mod action;
pub mod chat;
pub mod command;
pub mod completion;
pub mod paths;
pub mod permissions;
pub mod permissions_store;
pub mod profile;
pub mod server_tools;
pub mod settings;
pub mod skills;
pub mod store;
pub mod update;

pub use action::ImageAttachment;
pub use chat::{Activity, ChatController, Entry};
pub use command::SlashCommand;
pub use completion::{
    CommandCompleterBackend, CommandsContext, Completer, CompletionRequest, CompletionStatus,
    PathCompleterBackend, PathsContext, SkillCompleterBackend, SkillsContext,
};
