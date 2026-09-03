# tui

A small Ratatui runtime with explicit, typed communication. Components are
`Component<A>`: they receive semantic actions, own their state, and render it
without I/O or mutation.

## Communication API

| Need | API |
|---|---|
| Semantic user/feature command | `Action` + `dispatch` |
| Direct targeted state change | `update` |
| Current entity state changed | `notify` |
| React to state invalidation | `observe` |
| Emit typed occurrence | `emit(event)` |
| Subscribe to typed entity event | `subscribe(entity, callback)` |
| Consume external async stream | `subscribe_stream(stream, callback)` |
| Consume one-shot async work | `spawn(future, callback)` |
| Request redraw | `notify` |

The mechanisms have deliberately different meanings:

```text
notify + observe:    “My state may have changed; read me again.”
emit + subscribe:    “This specific event happened, with this payload.”
subscribe_stream:    “An external asynchronous source produced an item.”
spawn:               “The one-shot work started by this entity completed.”
```

Actions route synchronously to the top overlay, focused entity, and parent
path. `dispatch` is a synchronous targeted action operation and `update` is a
synchronous targeted state operation. Deferred callbacks from events,
observations, streams, and tasks are never re-entrant. Entity handles are
non-owning, and missing or removed entities are safe no-ops.

Siblings should normally communicate through their parent:

```text
child A -> typed event or notify -> parent -> update/dispatch -> child B
```

A direct typed event subscription is useful when a component reacts to a
specific occurrence from a specific source, such as a reusable confirmation
overlay returning a typed result. See `examples/counter.rs` for that flow and
`examples/stream.rs` for `subscribe_stream`.
