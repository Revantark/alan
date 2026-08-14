# Alan Development Guide

## Project

Alan is minimal coding-agent written in rust.

Workspace crates:

- `crates/llm/` — provider-independent LLM protocol types and API clients.
- `crates/providers/` — provider/model binding and authentication.
- `crates/agent/` — conversation state, tool calls, system prompt, agent loop.
- `crates/alan/` — interactive ratatui REPL frontend.

Workspace manifest: `Cargo.toml`.

## Commands

Run from repository root:

```bash
cargo fmt --all
cargo check --workspace
cargo test --workspace
cargo run -p alan
```

Focused checks:

```bash
cargo check -p alan
cargo test -p agent
cargo test -p llm
cargo test -p providers
```

Run formatting and checks after Rust code changes. Do not run release or destructive Git commands unless requested.

## Runtime

Alan currently uses OpenRouter:

```bash
OPENROUTER_API_KEY=... cargo run -p alan
```

Optional model override:

```bash
ALAN_MODEL=openai/gpt-4o-mini cargo run -p alan
```

Current default model is `openai/gpt-4o-mini`.

## Architecture

Keep dependency direction one-way:

```text
llm <- providers <- agent <- alan
```

`llm` must not depend on providers, agent, or UI.

`agent` must not depend on ratatui or crossterm.

`alan` owns terminal setup, key mapping, and ratatui rendering.

### Alan frontend boundary

```text
crates/alan/src/main.rs
  terminal lifecycle and crossterm event polling

crates/alan/src/core/
  UI-independent actions, controller, transcript, agent execution

crates/alan/src/views/
  ratatui adapter, UI state, layout, rendering
```

`core` must stay frontend-independent. Future GPUI frontend should reuse `Controller`, `Entry`, and `Action`, then provide its own event mapping and renderer.

Do not add ratatui/crossterm types to `crates/alan/src/core/`.

## Current Alan UI

`crates/alan/src/views/mod.rs` provides minimal borderless ratatui UI:

- Header.
- Scrollable chat history.
- User messages with padded background.
- Assistant messages without background, aligned with user content.
- Bottom editor area with background.
- Status line above editor.
- Input cursor placement.

`UiState` owns frontend interaction state:

- input text
- transcript scroll offset
- auto-follow output state

`Action` is UI-independent input intent:

- `Quit`
- `Submit`
- `ClearInput`
- `Backspace`
- `Insert(char)`
- `ScrollUp`
- `ScrollDown`

Keep UI simple. Avoid borders, unnecessary widgets, and premature abstraction.

## Current Agent Behavior

`crates/agent/src/agent.rs` currently:

- Stores conversation history in memory.
- Sends prompts through bound `providers::Model`.
- Supports system prompt and skills.
- Supports tools through `AgentTool`.
- Executes tool calls in rounds.
- Limits tool rounds with `max_tool_rounds`.
- Does not yet provide streaming events.
- Does not yet provide abort, session persistence, or queued prompts.

`Agent::builder(model).build()` creates agent with no system prompt, skills, or tools.

When converting assistant messages to LLM messages, omit `tool_calls` when list empty. OpenAI-compatible APIs reject `"tool_calls": []`.

## Code Style

- Rust edition 2024.
- Prefer small functions with one responsibility.
- Keep UI rendering separate from state mutation.
- Keep business logic out of `main.rs`.
- Avoid traits unless they solve current substitution or testing need.
- Prefer explicit types and straightforward control flow.
- Reuse existing workspace dependencies.
- Add dependencies only when necessary.
- Add regression tests for protocol and agent bugs.
- Preserve error context. Do not silently discard provider or tool errors.
- Avoid broad rewrites when targeted edits solve issue.

## UI Rules

- No core dependency on terminal framework.
- Frontend converts native events into `Action`.
- Renderer reads state; it should not perform network calls.
- Controller owns agent interaction and transcript state.
- Keep visual constants centralized in view module.
- Use terminal display width for cursor/layout calculations, not byte count.
- Keep auto-scroll behavior explicit.

Implement in this order unless user requests different scope. Keep each step usable.
