# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- The prompt editor caches its row measurement, so rendering no longer deep-clones the editor widget (buffer, undo history, search state) on every frame just to measure its wrapped height.

## 0.1.3 - 2026-09-30

### Added

- `/profile` command family to save, apply, and delete named model profiles capturing provider/model, reasoning effort, and web fetch/search settings.
- `Ctrl+P` to open the profile picker while idle.
- Profiles persisted in `profiles.json`; the active profile is recorded in `settings.json` and cleared by manual settings changes.

### Fixed

- Compound bash commands are split on `&&`, `||`, `;` and `|`; each chained command is granted and checked separately and the call runs only if all are allowed, so approving a chained call stores a grant per command.
- Permission requests resolve asynchronously in the agent loop, so the chat view no longer blocks while waiting on an answer.
- Kill the whole process group when a bash tool command times out or is aborted, so descendants like `cargo run`'s child no longer leak as orphaned processes.

## 0.1.2 - 2026-09-26

### Added

- Add permission manager with tool policies, interactive prompt, per-project grant persistence, and policy glyph in the status line.
- Strict policy: approving an edit tool once allows all subsequent edit tool calls without asking (persisted across sessions with "always allow").

## 0.1.1 - 2026-09-23

### Added

- Add --version flag printing package version.
- Add Cmd+V on macs to allow pasting an image.

### Fixed

- Refresh ui once a new session has started.
- Terminals not receiving `Shift` key events.

## 0.1.0 - 2026-09-21

The first tagged release of Alan, a minimal coding agent written in Rust.

### Added

- Interactive ratatui REPL frontend with scrollable chat history and editor.
- Streaming responses from LLM providers.
- Agent loop with tool-call rounds and an in-memory conversation store.
- OpenRouter provider support with full model catalog picker (`/models`).
- `/login` command for interactive provider authentication.
- `/new` and `/summarize-new` session commands with append-only JSONL persistence.
- `/plan` and `/review` modes (cycled with `Shift+Tab`).
- `/fork` to branch a session from a checkpoint.
- `/local` management of OpenAI-compatible local models.
- `/model-provider` to pin a custom provider for the active OpenRouter model.
- `/effort` reasoning-effort control (`none`…`max`).
- Markdown rendering of assistant messages.
- Image paste from clipboard.
- File and path completions in the editor.

### Fixed

- Chat scroll behaviour and paste-new-line handling.
- Sparse streamed tool-call indexes from providers.
- Dead watch cancellation in event/observation subscriptions.
