//! A small UI runtime for Ratatui applications.
//!
//! The framework owns UI mechanics: terminal lifecycle, the event loop, raw
//! input mapping, action dispatch and action propagation, focus management, popups
//! and overlays, redraw scheduling, background tasks, and error handling.
//! Components receive application-defined actions and messages. Actions are
//! semantic input intents routed synchronously through the active overlay,
//! focused component, parent components, and root. Messages are deferred
//! internal communication delivered to the root or an explicitly targeted
//! entity after the current callback returns.
//!
//! ```no_run
//! # use std::time::Duration;
//! # use ratatui::Frame;
//! # use ratatui::layout::Rect;
//! # use tui::{ActionStatus, Component, RenderContext, Runtime, RuntimeError};
//! # use tui::context::Context;
//! # struct Root;
//! # impl Component<&'static str> for Root {
//! #     fn render(&self, _frame: &mut Frame, _area: Rect, _cx: &RenderContext<'_, &'static str>) {}
//! # }
//! let runtime = Runtime::builder(Root).tick_rate(Duration::from_millis(16)).build();
//! # async fn example(runtime: Runtime<Root, &'static str>) -> Result<(), RuntimeError> {
//! runtime.run().await?;
//! # Ok(())
//! # }
//! ```

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
pub use focus::{FocusHandle, FocusScope};
pub use keymap::{InputContext, KeyMapper, NoopMapper, PassthroughMapper};
pub use subscription::{Subscription, SubscriptionEvent};
pub use task::{TaskExecutor, TokioExecutor};
