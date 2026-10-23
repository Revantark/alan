# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- `LlmApiCodec` implementations now create a per-stream `LlmStreamDecoder` instead of decoding shared stateless chunks through `response`; existing provider codecs delegate to reusable base decoders.
- `OpenRouterOptions` (exposed as `providers::OpenRouterOptions`) carries its provider pin list as `only` instead of `provider_order`.
- `Provider::apis` returns API bindings in preference order; `bind_model` uses the first entry as the default, without a separate `default_api` override.
- OpenRouter now uses Responses API.

## 0.1.9 - 2026-10-21

### Added

- `/provider add`, `/provider edit` and `/provider remove` to use any Chat Completions server by name. Alan checks the server's `/models` before saving, lists all of its models in `/models`, and keeps the key in `auth.json` so `/login` can change it.

### Changed

- Update default system prompt to include output formatting.
- Assistant responses and info entries now wrap at word boundaries instead of splitting mid-word; long unbreakable words such as URLs are hard-split to fit the line.
- Markdown headings render in the cyan accent color, with weight separating the top levels, instead of body-colored text; H1 no longer paints a full-width background band.

## 0.1.8 - 2026-10-16

### Fixed

- Fixed text selection not being copied in the ssh environment.

## 0.1.7 - 2026-10-14

### Added

- `/fork-command` copies the whole conversation into a new session file and prints the command to open it in another terminal.

### Changed

- The terminal now attaches the working directory and the active mode's instruction as a machine-readable `<attached_context>` block on user messages, instead of plain-text trailers.

### Fixed

- Resumed sessions now show tool outputs clipped to the last 5 lines, matching the live stream instead of dumping the full stored output.
- Resumed and forked transcripts no longer render machine-appended text (plan/review mode instructions, working-directory trailer, skills) in user prompts.

## 0.1.6 - 2026-10-13

### Added

- Headless mode: `alan -p "prompt"` (or `-p=prompt`) runs one prompt without the TUI, with all tool permissions approved, and prints the final answer to stdout
- `ALAN_PROFILE=<name>` overrides alan settings with profile settings but not persisted.
- ChatGPT subscription provider

### Changed

- `/fork` now uses the shared searchable picker overlay
- SSE stream decoding now compacts its buffer once per chunk instead of per line, avoiding quadratic copies

### Fixed

- Fix local model binding on startup

## 0.1.5 - 2026-10-07

### Added

- DeepSeek provider (`https://api.deepseek.com`). 

### Changed

- `/local add` auto-detects models via `/models` and provides a searchable picker for model selection.
- Provider-specific request/response handling moved out of the generic chat completions codec into per-provider codecs.
- Remove google from active providers.

### Fixed

- Local model overlay background now opaque — no longer shows underlying content.
- Picker search (models, profiles, local models) now matches characters as an ordered subsequence.
- Popup v2 clears before rendering a message such that background text doesn't overlap with popup's message.
- Text selection in the chat history is precise now.

## 0.1.4 - 2026-10-04

### Added

- Shell installer now also installs `alan-update`, a standalone self-updater; the in-app update notice points at it instead of the installer command.
- Skills: type `#name` in the prompt to attach a skill's full instructions to that message. Skills load from `<project>/.alan/skills/<name>/SKILL.md`, `<data dir>/skills/<name>/SKILL.md`, and `~/.agents/skills/<name>/SKILL.md`, complete on `#`, and highlight when they name a loaded skill. A skill whose body exceeds 25,000 characters is skipped rather than truncated.

### Fixed

- Permission tests no longer delete the shared temp root on cleanup, fixing an intermittent `cargo test --workspace` failure where a concurrent test's directory was removed mid-run.
- Pressing `Esc` to cancel a queued steering prompt no longer crashes the app.

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
