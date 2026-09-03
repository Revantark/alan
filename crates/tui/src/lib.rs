//! A small UI runtime for Ratatui applications.
//!
//! Communication is explicit: semantic [`ActionStatus`] actions route
//! synchronously; `Context::dispatch` and `Context::update` target known
//! entities; `notify`/`observe` communicates state invalidation;
//! `emit`/`subscribe` communicates typed entity events;
//! `subscribe_stream` consumes external streams; and `spawn` delivers a
//! one-shot typed result. Deferred callbacks are delivered non-reentrantly.

pub mod app;
pub mod component;
pub mod context;
pub mod entity;
pub mod error;
pub mod event_loop;
pub mod focus;
pub mod keymap;
pub mod overlay;
pub mod render;
pub mod subscription;
pub mod task;
pub mod terminal;

pub use app::{Runtime, RuntimeBuilder};
pub use component::{ActionStatus, Component, RenderContext};
pub use error::RuntimeError;
pub use keymap::{InputContext, KeyMapper, NoopMapper, PassthroughMapper};
pub use subscription::{Subscription, SubscriptionEvent};
pub use task::{TaskError, TaskExecutor, TaskHandle, TokioExecutor};
