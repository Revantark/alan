//! Runtime and builder.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;

use crate::component::Component;
use crate::context::RuntimeState;
use crate::entity::{EntityId, EntityStore};
use crate::error::RuntimeError;
use crate::event_loop::event_loop;
use crate::keymap::{KeyMapper, NoopMapper};
use crate::subscription::RuntimeDelivery;
use crate::task::{TaskExecutor, TokioExecutor};
use crate::terminal::{TerminalGuard, TerminalOptions, install_panic_hook};

/// Default tick interval driving periodic redraws.
const DEFAULT_TICK_RATE: Duration = Duration::from_millis(16);

/// Builder configuring runtime services before the loop starts.
///
/// Frame-level values (areas, layouts) do not belong here; the builder owns
/// services such as the key mapper, task executor, and tick rate.
pub struct RuntimeBuilder<C, A, M = ()> {
    root: C,
    key_mapper: Arc<dyn KeyMapper<A>>,
    executor: Arc<dyn TaskExecutor<M>>,
    tick_rate: Duration,
    terminal_options: TerminalOptions,
}

impl<C, A, M> RuntimeBuilder<C, A, M>
where
    C: Component<A, M>,
    A: Send + 'static,
    M: Send + 'static,
{
    /// Create a builder with the root component.
    pub fn new(root: C) -> Self {
        Self {
            root,
            key_mapper: Arc::new(NoopMapper),
            executor: Arc::new(TokioExecutor),
            tick_rate: DEFAULT_TICK_RATE,
            terminal_options: TerminalOptions::default(),
        }
    }

    /// Set the key mapper converting terminal events into actions.
    pub fn key_mapper(mut self, key_mapper: impl KeyMapper<A> + 'static) -> Self {
        self.key_mapper = Arc::new(key_mapper);
        self
    }

    /// Set the executor running background tasks.
    pub fn executor(mut self, executor: impl TaskExecutor<M> + 'static) -> Self {
        self.executor = Arc::new(executor);
        self
    }

    /// Set the interval between periodic ticks.
    ///
    /// The duration must be non-zero; `build` panics with a clear message for
    /// an invalid duration because Tokio intervals reject zero durations.
    pub fn tick_rate(mut self, tick_rate: Duration) -> Self {
        self.tick_rate = tick_rate;
        self
    }

    /// Set the terminal features configured for the runtime.
    pub fn terminal_options(mut self, terminal_options: TerminalOptions) -> Self {
        self.terminal_options = terminal_options;
        self
    }

    pub fn build(self) -> Runtime<C, A, M> {
        if self.tick_rate.is_zero() {
            panic!("tick rate must be greater than zero");
        }
        Runtime {
            root: self.root,
            key_mapper: self.key_mapper,
            executor: self.executor,
            tick_rate: self.tick_rate,
            terminal_options: self.terminal_options,
        }
    }
}

/// UI runtime owning the terminal, event loop, and framework services.
///
/// The runtime owns mechanics; the application root component owns meaning.
///
/// ```no_run
/// # use std::time::Duration;
/// # use ratatui::Frame;
/// # use ratatui::layout::Rect;
/// # use tui::{ActionStatus, Component, RenderContext, Runtime, RuntimeError};
/// # struct Root;
/// # impl Component<&'static str> for Root {
/// #     fn render(&self, _frame: &mut Frame, _area: Rect, _cx: &RenderContext<'_, &'static str>) {}
/// # }
/// let runtime = Runtime::builder(Root)
///     .tick_rate(Duration::from_millis(16))
///     .build();
/// # async fn example(runtime: Runtime<Root, &'static str>) -> Result<(), RuntimeError> {
/// runtime.run().await?;
/// # Ok(())
/// # }
/// ```
pub struct Runtime<C, A, M = ()> {
    root: C,
    key_mapper: Arc<dyn KeyMapper<A>>,
    executor: Arc<dyn TaskExecutor<M>>,
    tick_rate: Duration,
    terminal_options: TerminalOptions,
}

impl<C, A, M> Runtime<C, A, M>
where
    C: Component<A, M>,
    A: Send + 'static,
    M: Send + 'static,
{
    /// Start a builder with the given root component.
    pub fn builder(root: C) -> RuntimeBuilder<C, A, M> {
        RuntimeBuilder::new(root)
    }

    /// Run the event loop until the application quits.
    ///
    /// Sets up the terminal, runs the loop, and always restores terminal
    /// state, including on errors and panics.
    pub async fn run(self) -> Result<(), RuntimeError> {
        let guard = TerminalGuard::with_options(self.terminal_options)?;
        let _panic_hook = install_panic_hook();

        let entity_store = Arc::new(Mutex::new(EntityStore::new()));
        let root: EntityId = {
            let mut write = entity_store.lock().expect("entity store poisoned");
            write.insert(self.root).id()
        };

        let (sender, receiver) = mpsc::unbounded_channel::<RuntimeDelivery<M>>();
        let mut state = RuntimeState::new(sender, self.executor.clone());
        // Run the root's init (insert children, focus, etc.) before the
        // first frame renders.
        state.pending_inits.push_back(root);

        event_loop(
            guard,
            root,
            entity_store,
            state,
            self.key_mapper.clone(),
            receiver,
            self.tick_rate,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Frame;
    use ratatui::layout::Rect;

    struct TestRoot;

    impl Component<(), ()> for TestRoot {
        fn render(
            &self,
            _frame: &mut Frame,
            _area: Rect,
            _cx: &crate::component::RenderContext<'_, (), ()>,
        ) {
        }
    }

    #[test]
    #[should_panic(expected = "tick rate must be greater than zero")]
    fn zero_tick_rate_is_rejected() {
        Runtime::builder(TestRoot).tick_rate(Duration::ZERO).build();
    }
}
