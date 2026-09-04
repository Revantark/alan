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
    use crate::context::Context;
    use crate::entity::Entity;
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

    struct Probe {
        hits: usize,
    }
    impl Component<()> for Probe {
        fn render(&self, _: &mut Frame, _: Rect, _: &crate::component::RenderContext<'_, ()>) {}
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Ping;

    fn harness() -> (
        RuntimeState<()>,
        EntityStore<()>,
        Entity<Probe>,
        Entity<TestRoot>,
    ) {
        let (sender, _) = mpsc::unbounded_channel();
        let mut store = EntityStore::new();
        let target = store.insert(Probe { hits: 0 });
        let source = store.insert(TestRoot);
        let state = RuntimeState::new(sender, Arc::new(TokioExecutor));
        (state, store, target, source)
    }

    #[test]
    fn subscribe_once_fires_without_a_stored_handle() {
        let (mut state, store, target, source) = harness();
        {
            let mut cx: Context<'_, Probe, ()> = Context::new(&mut state, &store, target.id());
            cx.subscribe_once::<Ping, TestRoot, _>(source, |_, probe, _, _| {
                probe.hits += 1;
            });
        }
        // No Subscription handle retained; the runtime record owns liveness.
        let mut cx: Context<'_, TestRoot, ()> = Context::new(&mut state, &store, source.id());
        cx.emit(Ping);
        let batch = std::mem::take(&mut state.deliveries);
        for delivery in batch {
            if let crate::subscription::RuntimeDelivery::Event(delivery) = delivery {
                crate::event_loop::deliver_event_for_test(delivery, &mut state, &store);
            }
        }
        assert_eq!(store.typed_read(target.id(), |p: &Probe| p.hits), Some(1));
        assert!(state.subscriptions.is_empty());
    }

    #[test]
    fn subscribe_once_fires_at_most_once() {
        let (mut state, store, target, source) = harness();
        {
            let mut cx: Context<'_, Probe, ()> = Context::new(&mut state, &store, target.id());
            cx.subscribe_once::<Ping, TestRoot, _>(source, |_, probe, _, _| {
                probe.hits += 1;
            });
        }
        for _ in 0..2 {
            let mut cx: Context<'_, TestRoot, ()> = Context::new(&mut state, &store, source.id());
            cx.emit(Ping);
        }
        let batch = std::mem::take(&mut state.deliveries);
        for delivery in batch {
            if let crate::subscription::RuntimeDelivery::Event(delivery) = delivery {
                crate::event_loop::deliver_event_for_test(delivery, &mut state, &store);
            }
        }
        assert_eq!(store.typed_read(target.id(), |p: &Probe| p.hits), Some(1));
    }
}
