//! Runtime and builder.

use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

use crate::component::Component;
use crate::context::RuntimeState;
use crate::entity::EntityStore;
use crate::error::RuntimeError;
use crate::event_loop::event_loop;
use crate::keymap::{KeyMapper, NoopMapper};
use crate::subscription::RuntimeDelivery;
use crate::task::{TaskExecutor, TokioExecutor};
use crate::terminal::{TerminalGuard, TerminalOptions, install_panic_hook};

const DEFAULT_TICK_RATE: Duration = Duration::from_millis(16);

/// Builder configuring runtime services before the loop starts.
pub struct RuntimeBuilder<C, A> {
    root: C,
    key_mapper: Arc<dyn KeyMapper<A>>,
    executor: Arc<dyn TaskExecutor>,
    tick_rate: Duration,
    terminal_options: TerminalOptions,
}
impl<C, A> RuntimeBuilder<C, A>
where
    C: Component<A>,
    A: Send + 'static,
{
    pub fn new(root: C) -> Self {
        Self {
            root,
            key_mapper: Arc::new(NoopMapper),
            executor: Arc::new(TokioExecutor),
            tick_rate: DEFAULT_TICK_RATE,
            terminal_options: TerminalOptions::default(),
        }
    }
    pub fn key_mapper(mut self, key_mapper: impl KeyMapper<A> + 'static) -> Self {
        self.key_mapper = Arc::new(key_mapper);
        self
    }
    pub fn executor(mut self, executor: impl TaskExecutor + 'static) -> Self {
        self.executor = Arc::new(executor);
        self
    }
    pub fn tick_rate(mut self, tick_rate: Duration) -> Self {
        self.tick_rate = tick_rate;
        self
    }
    pub fn terminal_options(mut self, terminal_options: TerminalOptions) -> Self {
        self.terminal_options = terminal_options;
        self
    }
    pub fn build(self) -> Runtime<C, A> {
        assert!(
            !self.tick_rate.is_zero(),
            "tick rate must be greater than zero"
        );
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
pub struct Runtime<C, A> {
    root: C,
    key_mapper: Arc<dyn KeyMapper<A>>,
    executor: Arc<dyn TaskExecutor>,
    tick_rate: Duration,
    terminal_options: TerminalOptions,
}
impl<C, A> Runtime<C, A>
where
    C: Component<A>,
    A: Send + 'static,
{
    pub fn builder(root: C) -> RuntimeBuilder<C, A> {
        RuntimeBuilder::new(root)
    }
    pub async fn run(self) -> Result<(), RuntimeError> {
        let guard = TerminalGuard::with_options(self.terminal_options)?;
        let _panic_hook = install_panic_hook();
        let entity_store = Arc::new(Mutex::new(EntityStore::new()));
        let root = {
            let mut store = entity_store.lock().expect("entity store poisoned");
            store.insert(self.root).id()
        };
        let (sender, receiver) = mpsc::unbounded_channel::<RuntimeDelivery>();
        let mut state = RuntimeState::new(sender, self.executor.clone());
        state.pending_inits.push_back(root);
        event_loop(
            guard,
            root,
            entity_store,
            state,
            self.key_mapper,
            receiver,
            self.tick_rate,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Frame, layout::Rect};
    struct TestRoot;
    impl Component<()> for TestRoot {
        fn render(&self, _: &mut Frame, _: Rect, _: &crate::component::RenderContext<'_, ()>) {}
    }
    #[test]
    #[should_panic(expected = "tick rate must be greater than zero")]
    fn zero_tick_rate_is_rejected() {
        Runtime::builder(TestRoot).tick_rate(Duration::ZERO).build();
    }
}
