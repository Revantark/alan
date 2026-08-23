# Alan

Alan is a minimal coding agent written in Rust. It runs in your terminal and uses an LLM to answer questions, inspect files, edit files, and run shell commands.

**Note:** Alan is currently in an early prototype stage. Some basic features are still missing and will be added soon.
 
## Features

- Interactive terminal UI built with Ratatui
- OpenRouter provider support
- File read, file write, file edit, and shell tools
- Optional OpenRouter web search and web fetch tools
- Tool-call execution with configurable round limits
- Plan mode toggled with `/plan` or `Shift+Tab`
- Interactive provider login overlay
- File/folder path completion in the prompt (`@`)
- Configurable reasoning effort
- Session history persisted under `~/.alan/sessions`

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
  - `/login` — sign in to a provider interactively
  - `/plan` — toggle plan mode (also `Shift+Tab`)
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
| `ALAN_HOME` | Alan home directory | `$HOME` |
| `ALAN_REASONING_EFFORT` | `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max` | unset |
| `ALAN_OPENROUTER_WEB_SEARCH` | Enable web search tool (`1`, `true`, `yes`, `on`) | off |
| `ALAN_OPENROUTER_WEB_FETCH` | Enable web fetch tool (`1`, `true`, `yes`, `on`) | off |
| `ALAN_LOG` | Log filter (falls back to `RUST_LOG`) | unset |
| `ALAN_LOG_DIR` | Where daily log files go | `$ALAN_HOME/.alan/logs` |

Files live under `$ALAN_HOME/.alan/`: `auth.json` (credentials),
`sessions/` (conversation history), and `logs/`.

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
