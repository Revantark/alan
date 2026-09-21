# Alan

Alan is a minimal coding agent written in Rust that runs in your terminal. It can use an LLM to answer questions, inspect files, edit files, and run shell commands.

> Alan is currently an early prototype. Some features are still being developed, and behavior may change between releases.

## Install

Prebuilt binaries are published through GitHub Releases. The installer detects your operating system and CPU architecture, downloads the matching binary, verifies it, and installs `alan` into Cargo's binary directory.

### macOS and Linux

```bash
curl https://github.com/Revantark/alan/releases/latest/download/alan-installer.sh | sh
```

Restart your shell or ensure `~/.cargo/bin` is on your `PATH`, then run:

```bash
alan
```

### Windows

Run the generated PowerShell installer from a PowerShell prompt:

```powershell
irm https://github.com/Revantark/alan/releases/latest/download/alan-installer.ps1 | iex
```

### Updating

Run the same installer command again to install the latest release. To see the installed version:

```bash
alan --version
```

## Features

- Interactive terminal UI built with Ratatui
- OpenRouter provider support
- File read, file write, file edit, and shell tools
- Optional OpenRouter web search and web fetch tools
- Tool-call execution with configurable round limits
- Plan mode toggled with `/plan` or `Shift+Tab`; review mode with `/review`.
  `Shift+Tab` cycles Normal → Plan → Review; `/normal` turns both off.
- Session history is stored under `$ALAN_HOME/.alan/sessions` (or
  `$HOME/.alan/sessions` when `ALAN_HOME` is unset). Each working directory
  gets its own hashed subdirectory, containing append-only JSONL session files.
  Sessions are created on the first non-empty prompt.
- To resume a session, set `ALAN_SESSION` to the session ID (the filename
  without `.jsonl`) and start Alan from the same working directory. For
  example: `ALAN_SESSION=018f... cargo run -p alan`. The configured model and
  provider must match the stored session.

## Requirements

- Rust 1.88 or newer
- OpenRouter API key

## Run

```bash
OPENROUTER_API_KEY=... cargo run -p alan
```

Default model is `openai/gpt-4o-mini`. Override it with:

```bash
OPENROUTER_API_KEY=... \
ALAN_MODEL=openai/gpt-4o-mini \
cargo run -p alan
```

Alan stores credentials at `~/.alan/auth.json` by default. Set `ALAN_HOME` to change its home directory. You can also run `/login` inside Alan to authenticate interactively.

## Usage

- Type a prompt and press `Enter` to run it. Use `Shift+Enter`, `Alt+Enter`,
  `Ctrl+J`, or `Ctrl+M` for a newline.
- Type `@` in the prompt to complete file and folder paths (`@popup.rs`
  matches anywhere; `@src/co` matches by directory).
- Slash commands:
  - `/models` — pick a model from the provider catalog
  - `/login` — sign in to a provider interactively
  - `/new` — start a fresh session
  - `/summarize-new [focus]` — summarize this session and restart into a new one
    seeded with the summary; the optional quoted text is an extra focus hint
  - `/plan` — toggle plan mode (also `Shift+Tab`)
  - `/review` — toggle review mode (also `Shift+Tab`)
  - `/normal` — turn off plan and review mode
  - `/help` — list available commands
- Key bindings: `Esc` clears input/selection, `Ctrl+C` interrupts the agent,
  `Ctrl+U` deletes to line start, `Ctrl+Z` undoes an edit,
  `Up`/`Down`/`PageUp`/`PageDown` scroll the transcript or navigate the
  completion popup. Mouse scrolling and bracketed paste are supported.

## Configuration

All variables are optional:

| Variable | Purpose | Default |
|---|---|---|
| `ALAN_MODEL` | Model id | `openai/gpt-4o-mini` |
| `ALAN_HOME` | Alan home directory (Alan uses `$ALAN_HOME/.alan/`) | `$HOME` |
| `ALAN_SESSION` | Resume this session ID from the current working directory | unset |
| `ALAN_OR_MODEL_PROVIDER` | Comma-separated ordered provider list forwarded to the API | unset |
| `ALAN_REASONING_EFFORT` | `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max` | unset |
| `ALAN_OPENROUTER_WEB_SEARCH` | Enable web search tool (`1`, `true`, `yes`, `on`) | off |
| `ALAN_OPENROUTER_WEB_FETCH` | Enable web fetch tool (`1`, `true`, `yes`, `on`) | off |
| `ALAN_LOG` | Log filter (falls back to `RUST_LOG`) | unset |
| `ALAN_LOG_DIR` | Where daily log files go | `$ALAN_HOME/.alan/logs` |

Files live under `$ALAN_HOME/.alan/`: `auth.json` (credentials),
`sessions/` (conversation history), and `logs/`.

## Run locally

For development from a checkout:

```bash
OPENROUTER_API_KEY=... cargo run -p alan
```

To build and run a release binary locally:

```bash
cargo build -p alan --release
./target/release/alan
```

## Release process

Releases are built for multiple platforms with [`cargo-dist`](https://github.com/axodotdev/cargo-dist) and published as GitHub Release artifacts. The generated workflow is stored at `.github/workflows/release.yml`.

To create a release:

1. Update the version in `crates/alan/Cargo.toml`.
2. Review and test the changes.
3. Create and push a version tag from the repository root:

   ```bash
   git tag v0.1.0
   git push origin v0.1.0
   ```

4. GitHub Actions builds the supported targets and publishes the installers and archives to the GitHub Release.

To preview the distribution locally before tagging:

```bash
cargo dist plan
cargo dist build
```

The release workflow requires the repository's GitHub Actions permissions to be allowed to create releases. Do not push a release tag until the version and generated artifacts have been reviewed.

## Development

Run commands from repository root:

```bash
cargo fmt --all
cargo check --workspace
cargo test --workspace
```

Run Alan locally:

```bash
cargo run -p alan
```

## Workspace crates

- `crates/llm` — provider-independent LLM protocol types and API clients
- `crates/providers` — provider bindings, model catalog, and authentication
- `crates/tools` — file and shell tool implementations
- `crates/agent` — conversation state, skills, tools, and agent loop
- `crates/alan` — interactive Ratatui frontend
