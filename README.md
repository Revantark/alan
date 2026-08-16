# Alan

Alan is a minimal coding agent written in Rust. It runs in your terminal and uses an LLM to answer questions, inspect files, edit files, and run shell commands.

**Note:** Alan is currently in an early prototype stage. Some basic features are still missing and will be added soon.
 
## Features

- Interactive terminal UI built with Ratatui
- OpenRouter provider support
- File read, file write, file edit, and shell tools
- Optional OpenRouter web search and web fetch tools
- Tool-call execution with configurable round limits

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

Alan stores credentials at `~/.alan/auth.json` by default. Set `ALAN_HOME` to change its home directory.

## Optional server tools

Enable OpenRouter web tools with boolean environment variables:

```bash
ALAN_OPENROUTER_WEB_SEARCH=true
ALAN_OPENROUTER_WEB_FETCH=true
```

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
