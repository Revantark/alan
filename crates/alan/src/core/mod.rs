//! UI-agnostic application core. No ratatui/crossterm dependencies here.

pub mod action;
pub mod chat;
pub mod controller;
pub mod login;

pub use action::{Action, Command};
pub use chat::Entry;
pub use controller::{Controller, Overlay, Poll};
pub use login::LoginState;
