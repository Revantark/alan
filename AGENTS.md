# Alan Development Guide

## Project

Alan is a minimal coding agent written in Rust.

Workspace crates:

- `crates/llm/` — provider-independent LLM protocol types and API clients.
- `crates/providers/` — provider/model binding and authentication.
- `crates/tools/` — built-in file system and shell tools.
- `crates/agent/` — conversation state, tool calls, system prompt, agent loop.
- `crates/tui/` — package `alan-tui`, a standalone Ratatui runtime: components,
  event loop, terminal lifecycle. Published to crates.io.
- `crates/alan/` — interactive ratatui REPL frontend.

Workspace manifest: `Cargo.toml`.

## Commands

Run from repository root:

```bash
cargo fmt --all
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p alan
```

CI runs `cargo fmt --all -- --check`, the `clippy` command above, and
`cargo test --workspace` on every pull request. Use `cargo check` for the fast
inner loop; run `clippy` before pushing.

Focused checks:

```bash
cargo check -p alan
cargo test -p agent
cargo test -p llm
cargo test -p providers
cargo test -p tools
cargo test -p alan-tui
```

Run formatting and checks after Rust code changes. Do not run release or destructive Git commands unless requested.

### Model selection

`/models` opens a picker over the model catalogs of every registered provider.
Selecting a model switches the active conversation model and updates the
session header.

Profiles save the current provider/model, reasoning effort, and web-fetch/search
settings. Use `/profile save <name>` to create one, `/profile` to switch, and
`/profile delete` to remove one. `Ctrl+P` opens the picker while idle. Applying a profile persists its
settings for future launches; manual settings changes clear its active-profile
marker. Profiles are stored separately in `<data dir>/profiles.json`. See
`core::paths` for how the data directory is resolved.

`build_providers` in `crates/alan/src/main.rs` lists the registered providers.
The `providers` crate also has a Google provider that is not registered. Sign in
with `/login`, or set the provider's API key variable:

```bash
OPENROUTER_API_KEY=... cargo run -p alan
```

Optional model override:

```bash
ALAN_MODEL=openai/gpt-4o-mini cargo run -p alan
```

With no saved settings, Alan uses the `openrouter` provider and
`DEFAULT_MODEL` from `crates/alan/src/core/settings.rs`.

## Architecture

Keep dependency direction one-way:

```text
llm <- providers <- agent <- alan
llm <- tools     <- agent
alan-tui               <- alan
```

`llm` must not depend on providers, agent, or UI.

`agent` must not depend on ratatui or crossterm.

`alan-tui` must not depend on other workspace crates.

`alan-tui` owns terminal setup and the event loop. `alan` owns key mapping and
ratatui rendering.

### Alan frontend boundary

```text
crates/alan/src/main.rs
  provider setup, model selection, runtime launch

crates/alan/src/keymap.rs
  crossterm events to `AlanAction`

crates/alan/src/root.rs
  root component: routes `AlanAction` to children, lays out the screen

crates/alan/src/core/
  UI-independent chat controller, transcript, commands, settings,
  permissions, skills

crates/alan/src/views/
  ratatui components and theme
```

`core` must stay frontend-independent. A future GPUI frontend should reuse `ChatController` and `Entry`, then provide its own event mapping and renderer.

Do not add ratatui/crossterm types to `crates/alan/src/core/`.

## Changelog

`CHANGELOG.md` at the repo root follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
format. `cargo dist` reads it to generate GitHub Release titles and bodies, and
auto-includes it in release archives.

- Update the `## [Unreleased]` section before tagging a release.
- When cutting a release, rename `## [Unreleased]` to `## <version> - <YYYY-MM-DD>`
  and add a fresh empty `## [Unreleased]` section on top.
- Use `### Added` / `### Changed` / `### Fixed` / `### Removed` subsections.

## Current Alan UI

`crates/alan/src/views/components/` provides minimal borderless ratatui UI:

- Header.
- Scrollable chat history.
- User messages with padded background.
- Assistant messages without background, aligned with user content.
- Bottom editor area with background.
- Status line above editor.
- Input cursor placement.

`ChatView` (`views/components/chat_view/`) owns the chat session: the
`ChatController` and the agent stream. `PromptEditor` owns input. Overlays
such as login and local models live beside `root.rs`.

Keep UI simple. Avoid borders, unnecessary widgets, and premature abstraction.

## Current Agent Behavior

`crates/agent/src/agent/` currently:

- Stores conversation history in memory.
- Sends prompts through bound `providers::Model`.
- Supports system prompt and skills.
- Supports tools through `AgentTool`.
- Executes tool calls in rounds.
- Limits tool rounds with `max_tool_rounds`.
- Streams events, supports abort, and persists sessions under
  `<data dir>/sessions` (append-only JSONL), where the data dir is
  `$ALAN_HOME/.alan` or `$HOME/.alan`.
- `/new` resets the in-memory conversation and starts a new session file; the
  old file stays on disk and remains resumable by its id. `/summarize-new
  [focus]` runs one tool-less summarization round and restarts into a fresh
  session seeded with the summary (plus an optional quoted focus hint).

`Agent::builder(model).build()` creates agent with no system prompt, skills, or tools.

When converting assistant messages to LLM messages, omit `tool_calls` when list empty. OpenAI-compatible APIs reject `"tool_calls": []`.

## Skills

Skills load from `<project>/.alan/skills/<name>/SKILL.md`,
`<data dir>/skills/<name>/SKILL.md`, and
`<home>/.agents/skills/<name>/SKILL.md`, in that order, so an earlier
root shadows a later one of the same name. Typing `#name` in the prompt
attaches that skill's body to the message.

Discovery lives in `core::skills`; the `agent` crate only formats skills
it is given and never reads from disk. `core::completion::SkillCompleterBackend`
offers names on `#`, and `PromptEditor` highlights matching tokens. The
catalog loads once at startup.

`core::skills::description_of` parses frontmatter by hand rather than with
a YAML dependency; only `description` is interpreted, and a skill with none
is rejected. A body over `MAX_INSTRUCTIONS` (25,000 chars) is rejected
whole — partial instructions would be followed as if complete — and the
skip is `tracing::warn!`-logged.

## Code Style

- Rust edition 2024.
- Use one blank line between items (functions, trait signatures, impls, structs/enums, top-level functions). `cargo fmt` allows zero, so do it by hand.
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
- Frontend converts native events into `AlanAction`.
- Renderer reads state; it should not perform network calls.
- `ChatController` owns agent interaction and transcript state.
- Keep visual constants centralized in `views/theme.rs`.
- Use terminal display width for cursor/layout calculations, not byte count.
- Keep auto-scroll behavior explicit.
