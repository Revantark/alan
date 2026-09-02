# tui

A small, production-oriented UI runtime for [Ratatui](https://ratatui.rs)
applications. The framework owns UI mechanics; the application owns meaning.

## The framework owns

- terminal setup/cleanup (panic-safe guard for raw mode + alternate screen)
- the event loop (`EventStream` + tick + task deliveries via `tokio::select!`)
- raw input mapping (`KeyMapper` → semantic `Action`)
- action dispatch and propagation (topmost overlay → focused entity → root)
- component composition (`Entity<T>` handles, parent routing, child rendering)
- focus management (scopes, next/prev, save/restore, modal boundaries)
- popups, modals, and overlays (runtime-owned overlay stack)
- redraw scheduling (dirty flag + tick)
- background tasks (`cx.spawn`, results delivered as messages by entity id)
- error handling (`RuntimeError::{Terminal, Task}`; component panics unwind
  normally and are cleaned up by the panic hook)

## The application owns

- state, domain actions, and messages (`Component<A, M>`)
- business rules and layout decisions
- disk/web work, expressed as spawned tasks — never inside an action or
  message handler or `render`

## Usage

```rust,ignore
let runtime = tui::Runtime::builder(Root::new())
    .key_mapper(MyKeyMapper)
    .executor(TokioExecutor) // default
    .tick_rate(Duration::from_millis(16))
    .build();

// requires a tokio runtime; drive with your own `block_on`
runtime.run().await?;
```

See `examples/counter.rs` for the full flow: key press → `KeyMapper` →
`Action` → focused component → deferred `cx.emit` → root interprets the
message → deferred `cx.send` commands the counter and status components →
redraw.

## Design rules

- `cx.emit(message)` queues a deferred message for the root; `cx.send(entity,
  message)` queues one for a specific entity. Both preserve FIFO ordering and
  are delivered only after the current callback returns.
- Components own their state transitions. Parents should normally send a
  command to a child instead of updating its private fields with `cx.update`.
- Actions route to the top overlay, or through the focused entity's parent path
  to the root; an unhandled action is not broadcast to siblings.
- Entity handles are non-owning. Closing an overlay removes its entity,
  bindings, and focus scope; later task or message delivery is a safe no-op.
- Initialization drains queued descendants before the initial dirty frame is
  rendered.
- Rendering is pure and immediate-mode: read state, recalculate layout, no I/O.
- Task results are routed by entity id captured at spawn; a removed component
  is never kept alive, and delivery to it is a safe no-op.

## Stream subscriptions

`Context::subscribe` attaches a repeated asynchronous stream to the component
that calls it. Stream items and normal completion are delivered on the runtime
side, not from the worker task, so the callback can safely mutate the target
component and use normal `Context` capabilities such as `notify`, `send`, and
`emit`.

Keep the returned [`Subscription`](crate::Subscription) handle alive while the
subscription should run. Dropping or cancelling it stops future delivery;
removing the target entity cancels its subscriptions as well. Deliveries that
arrive after cancellation or entity removal are safe no-ops.

Subscriptions are distinct from `notify` (redraw invalidation), `send` (a
deferred targeted message), and `emit` (a deferred root message). Stream
errors are application data and should be represented by the stream item type,
for example `Stream<Item = Result<Value, Error>>`.

Async workers retain only an id and cancellation state; they do not retain a
component or a strong entity handle.

```rust,ignore
use futures_util::stream;

struct Loader {
    values: Vec<String>,
    subscription: Option<tui::Subscription>,
}

impl Component<Action, Message> for Loader {
    fn init(&mut self, cx: &mut Context<'_, Self, Action, Message>) {
        self.subscription = Some(cx.subscribe(
            stream::iter(["one".to_owned(), "two".to_owned()]),
            |event, loader, cx| {
                if let tui::SubscriptionEvent::Item(value) = event {
                    loader.values.push(value);
                    cx.notify();
                }
            },
        ));
    }

    // render omitted
}
```
