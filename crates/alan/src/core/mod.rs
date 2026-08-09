//! UI-agnostic application core. No ratatui/crossterm dependencies here.

pub mod action;
pub mod controller;

pub use action::Action;
pub use controller::{Controller, Entry, Poll};
