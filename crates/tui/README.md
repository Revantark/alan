# tui

A small Ratatui runtime with explicit, typed communication. Components are
`Component<A>`: they receive semantic actions, own their state, and render it
without I/O or mutation.

## Install

```toml
[dependencies]
tui = "0.1"
```

## First program

Every app needs a root component, a `KeyMapper`, and a `Runtime`:

```rust
use tui::{Component, Runtime, KeyMapper, PassthroughMapper, RenderContext};
use tui::context::Context;
use tui::entity::Entity;
use ratatui::Frame;
use ratatui::layout::Rect;

// Your action type — the semantic intents your app understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action { Quit, Increment, Decrement }

// A component: implement `Component<Action>` and at minimum `render`.
struct Counter { value: u32 }

impl Component<Action> for Counter {
    fn handle_action(&mut self, action: &Action, cx: &mut Context<'_, Self, Action>) -> tui::ActionStatus {
        match action {
            Action::Increment => { self.value += 1; cx.notify(); }
            Action::Decrement => { self.value = self.value.saturating_sub(1); cx.notify(); }
            _ => {}
        }
        tui::ActionStatus::Handled
    }

    fn render(&self, frame: &mut Frame, area: Rect, cx: &RenderContext<'_, '_, Action>) {
        // ... render self.value into area ...
    }
}

struct Root;

impl Component<Action> for Root {
    fn init(&mut self, cx: &mut Context<'_, Self, Action>) {
        cx.insert(Counter { value: 0 });
    }

    fn render(&self, frame: &mut Frame, area: Rect, cx: &RenderContext<'_, '_, Action>) {
        // ... render children ...
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            Runtime::builder(Root)
                .key_mapper(PassthroughMapper)
                .tick_rate(std::time::Duration::from_millis(16))
                .build()
                .run()
                .await
        })?;
    Ok(())
}
```

Run an example to see it in action:

```bash
cargo run -p tui --example counter
cargo run -p tui --example greet
```

## The `Component<A>` trait

`A` is your app's action type. The trait contract:

| Method | Required? | When it's called |
|--------|-----------|------------------|
| `render` | **Yes** | Every frame. Must not perform I/O or mutate state. |
| `init` | No (default empty) | Once, before the first frame, after the component is inserted. |
| `handle_action` | No (default `Continue`) | When a semantic action reaches this entity. |
| `handle_mouse` | No (default `Continue`) | When a mouse event hits this entity's rendered area. |
| `cleanup` | No (default empty) | Before the entity is removed (overlay closed, parent removed). |

## Input

The framework receives native terminal events from crossterm. A user-provided
[`KeyMapper<A>`] converts them into semantic actions. Components must **not**
depend on raw crossterm events — crossterm types appear only at this boundary.

Actions route synchronously through a hierarchy:

```text
overlay → focused entity → parent chain → root
```

If a component returns [`ActionStatus::Handled`], propagation stops. If it
returns [`ActionStatus::Continue`], the action bubbles up.

## Communication API

| Need | API |
|------|-----|
| Semantic user/feature command | `Action` + `dispatch` |
| Direct targeted state change | `update` |
| Current entity state changed | `notify` |
| React to state invalidation | `observe` |
| Emit typed occurrence | `emit(event)` |
| Subscribe to typed entity event | `subscribe(entity, callback)` |
| Subscribe once (runtime-owned) | `subscribe_once(entity, callback)` |
| Consume external async stream | `subscribe_stream(stream, callback)` |
| Consume one-shot async work | `spawn(future, callback)` |
| Request redraw | `notify` |

The mechanisms have deliberately different meanings:

```text
notify + observe:    "My state may have changed; read me again."
emit + subscribe:    "This specific event happened, with this payload."
subscribe_stream:    "An external asynchronous source produced an item."
spawn:               "The one-shot work started by this entity completed."
```

Deferred callbacks from events, observations, streams, and tasks are never
re-entrant. Entity handles are non-owning, and missing or removed entities are
safe no-ops.

## Parent–child communication

Siblings should normally communicate through their parent:

```text
child A -> typed event or notify -> parent -> update/dispatch -> child B
```

A direct typed event subscription is useful when a component reacts to a
specific occurrence from a specific source, such as a reusable confirmation
overlay returning a typed result. See `examples/counter.rs` for that flow.

## Providing state to children

A parent can hand immutable slices of its state to children by reference — no
clones, no shared handles. Render with `cx.render_with_state(entity, frame, area, &state)`
and read it back during render with `cx.state::<T>()`. See `examples/provided_state.rs`.

## Key types

| Type | Purpose |
|------|---------|
| [`Runtime<C, A>`] | The UI runtime. Owns the terminal, event loop, and framework services. |
| [`RuntimeBuilder<C, A>`] | Configures services before the loop starts (key mapper, executor, tick rate). |
| [`Component<A>`] | A self-contained piece of UI. |
| [`Context<'_, T, A>`] | Capabilities available during component callbacks (actions, events, state). |
| [`RenderContext<'_, '_, A>`] | Read-only capabilities during render (focus, child rendering, state access). |
| [`Entity<T>`] | A non-owning typed handle to a component instance. |
| [`KeyMapper<A>`] | Converts crossterm events into semantic actions. |
| [`PassthroughMapper`] | Passes events through unchanged (useful for tests or event-as-action apps). |
| [`NoopMapper`] | Maps every event to no action (runtime default). |
| [`Subscription`] | A cancellation handle for stream, event, or observation subscriptions. |
| [`TaskHandle`] | A handle for one-shot async work started via `spawn`. |

## Examples

| Example | What it demonstrates |
|---------|----------------------|
| `counter` | Typed events + `observe` + confirmation overlay |
| `greet` | Minimal boot path, `PassthroughMapper`, editor integration |
| `stream` | `cx.subscribe_stream` for async data |
| `provided_state` | `render_with_state` / `state()` for parent→child data flow |
| `models` | `spawn` + external API calls (`reqwest`) |
| `selection_showcase` | `Selection` / `TextPosition` text selection in a terminal |
