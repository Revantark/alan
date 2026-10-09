# Alan

A minimal coding agent in your terminal.

> Alan is currently an early prototype. Some features are still being developed, and behavior may change between releases.

 ![Alan running in a terminal](assets/alan.png)

## Table of Contents

- [Features](#features)
- [Installation](#installation)
- [Providers](#providers)
- [Usage](#usage)
- [Permissions](#permissions)
- [Local models](#local-models)
- [Skills](#skills)
- [Configuration and data locations](#configuration-and-data-locations)
- [Building from source](#building-from-source)
- [Requirements](#requirements)

## Features

- **First-class OpenRouter support** — full model catalog, per-model custom provider pinning (`/model-providers`), and reasoning effort control (`/effort`).
- **Paste images** — drop a screenshot from your clipboard straight into the conversation and ask about it.
- **Compact with focus** — `/summarize-new "focus hint"` summarizes the context and restarts with just what matters.
- **Steering** — queue a message mid-run and the agent picks it up without waiting for the response to finish.
- **Plan and review modes** — cycle through them with `Shift+Tab` when you want the agent to think before it acts.
- **Local models** — point Alan at any OpenAI-compatible endpoint, no API key required.
- **Skills** — type `#name` in the prompt to attach a skill's full instructions to that message.

## Installation

Prebuilt binaries are published through GitHub Releases. The installer detects your operating system and CPU architecture, downloads the matching binary, verifies it, and installs `alan` into Cargo's binary directory.

### macOS and Linux

```bash
curl -fsSL https://github.com/Revantark/alan/releases/latest/download/alan-installer.sh | bash
```

Restart your shell or ensure `~/.cargo/bin` is on your `PATH`, then run:

```bash
alan
```

### Windows

Not supported yet.

### Updating

The installer also installs `alan-update` next to `alan` (in Cargo's binary directory, `~/.cargo/bin`). Run it to check for a newer release and install it:

```bash
alan-update
```

It takes no arguments and needs no confirmation. To see the installed version:

```bash
alan --version
```

`alan-update` only exists for installations that ran the installer after it started shipping the updater. If you installed earlier, or you built from source, run the installer command once to get it. Alan also shows an update notice on startup when a newer release is published.

## Providers

Alan currently supports:

- **OpenRouter**
- **DeepSeek**
- **Zai** (beta)

Use the `/login` command to sign in to any of the available providers.

For OpenRouter, you can pin a custom provider for the selected model with `/model-providers`, followed by the provider name (no quotes):

- `/model-providers deepseek`
- `/model-providers xiaomi/fp8`

## Usage

| Command | Description |
| --- | --- |
| `/models` | Pick a model from the provider catalog |
| `/profile` | Open the profile picker to switch profiles (`Ctrl+P`, or `Cmd+Option+P` where the terminal passes it through) |
| `/profile save <name>` | Save current model/provider, reasoning, and web settings as a new profile |
| `/profile delete` | Pick a saved profile to delete |
| `/plan` | Toggle plan mode (also `Shift+Tab`) |
| `/review` | Toggle review mode (also `Shift+Tab`) |
| `/normal` | Turn off plan and review mode |
| `/login` | Sign in to a provider interactively |
| `/new` | Start a fresh session |
| `/summarize-new [focus]` | Summarize the context and restart; optionally pass a quoted focus hint, e.g. `/summarize-new "just take the XYZ details"` |
| `/effort` | Set reasoning effort (`none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`) |
| `/fork` | Fork the session from a checkpoint |
| `/rename <name>` | Rename the current session |
| `/tool-free` | Allow all tool calls without asking |
| `/tool-slip` | Approve a command family once, allow its siblings |
| `/tool-strict` | Allow only exact commands already approved |
| `/local` | Manage local models (add, remove, edit) |
| `/model-providers <name>` | Pin a custom provider for the current OpenRouter model |
| `/help` | List available commands |
| `/quit` | Exit Alan |

## Permissions

Alan provides three policies to handle tool call permissions.

- **◌ Free** — allow every tool call without asking.
- **⟐ Slip** — once a command family (e.g. `bun`) is approved, siblings run without asking.
- **# Strict** — only exact previously-approved commands are allowed; approving an edit tool once unlocks all edit tools (file edits only, not commands).

A single tool call may chain several commands (`cargo test && cargo run`, or with `||`, `;`, `|`). Each chained command is granted and checked separately, and the call runs only if every one of them is allowed, so approving `cargo test && cargo run` stores two grants and the next call runs without prompting. Splitting is deliberately shallow: subshells, redirections and `$(...)` are not parsed, so such a command stays one opaque grant rather than being partly authorized.

Grants persist per-project at `<data dir>/projects/<pwd-hash>/permissions.json`. The current policy is shown as a glyph in the status line.

Alan starts in strict policy and remembers the policy you pick across runs. Switch with:
  - `/tool-slip`
  - `/tool-free`
  - `/tool-strict`

## Local models

Alan supports OpenAI-compatible local models.

```bash
/local add
```

This opens an overlay with fields for URL, API, API Key, and Model ID. Alan queries the server's `/models` endpoint and offers the detected models in a searchable picker. Use `/local remove` and `/local edit` to manage your local models. No API key or `/login` is needed for local models.

## Configuration and data locations

Alan stores everything under `~/.alan/` by default:

- `settings.json` — your current settings and active-profile marker.
- `profiles.json` — saved named model/reasoning/web settings profiles.
- `auth.json` — provider credentials saved by `/login`.
- `local_models.json` — local models added with `/local`.
- `projects/` — per-project permission grants.
- `sessions/` — conversation history (append-only JSONL, one file per session).
- `skills/` — personal skills, available in every project.
- `logs/` — daily rotating logs.

All of these live in the data directory (`~/.alan` by default).

### Environment variables

Settings variables override `settings.json` for that run without changing it. Start Alan with `--save` to write them into `settings.json`.

| Variable | Effect |
| --- | --- |
| `ALAN_HOME` | Parent of the data directory: `ALAN_HOME=/tmp/x` puts everything in `/tmp/x/.alan`. |
| `ALAN_PROVIDER` | Provider id: `openrouter`, `deepseek`, `zai`, or `local`. |
| `ALAN_MODEL` | Model id for that provider, e.g. `ALAN_MODEL=openai/gpt-4o-mini alan`. |
| `ALAN_REASONING_EFFORT` | `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, or `max`. |
| `ALAN_OPENROUTER_WEB_FETCH` | Turn OpenRouter's web fetch tool on or off: `true`/`false`, `1`/`0`, `yes`/`no`, `on`/`off`. |
| `ALAN_OPENROUTER_WEB_SEARCH` | Turn OpenRouter's web search tool on or off, same values. |
| `ALAN_OR_MODEL_PROVIDER` | Comma-separated OpenRouter provider order for the selected model, e.g. `deepseek,xiaomi/fp8`. |
| `ALAN_SESSION` | Resume a saved session by id. Alan prints the exact command when it exits. |
| `ALAN_LOG` | Log filter in `tracing` `EnvFilter` syntax. Falls back to `RUST_LOG`. |
| `ALAN_LOG_DIR` | Log directory, default `~/.alan/logs`. |
| `OPENROUTER_API_KEY`, `DEEPSEEK_API_KEY`, `ZAI_API_KEY` | API key for that provider, used when `/login` has not saved one. |

### Flags

- `--save` — write the settings environment variables above into `settings.json`.
- `--blank` — start without Alan's default system prompt.
- `--version` — print the installed version.

## Skills

A skill is a folder of instructions for a specific kind of task. Type `#name`
in the prompt to attach one to that message; Alan completes the name as you
type and highlights the token.

Skills live in three places, scanned in this order:

- `<project>/.alan/skills/<name>/SKILL.md` — project skills. Commit these;
  everyone who clones the repo gets them, like `AGENTS.md`.
- `~/.alan/skills/<name>/SKILL.md` — personal skills, shared across projects.
- `~/.agents/skills/<name>/SKILL.md` — skills shared with other agent tools
  (Claude Code, Zed, Codex, and others), so one you write for them is not
  locked to Alan.

An earlier entry shadows a later one with the same name. The folder name is
what `#name` resolves against.

Each skill is a `SKILL.md` with YAML frontmatter and a body:

```markdown
---
description: Use when shipping a release. Covers the checklist and tag format.
---
Run `scripts/check.sh`, then open a release PR. Never push to `main`.
```

`description` is the only frontmatter key Alan reads, and it is required —
it is the text you pick the skill from, so write it as "use when…". The body
is appended in full to the message you attached it to, so keep skills focused.
A body over 25,000 characters is skipped at load — the skill does not appear
in the `#` popup, and the skip is logged.

Skills are read once at startup, so editing a `SKILL.md` takes effect on the
next launch; a `#name` that matches no skill is left as ordinary text.

## Building from source

You'll need Rust 1.88 or newer (see [Requirements](#requirements)).

```bash
git clone https://github.com/Revantark/alan
cd alan
cargo run -p alan
```

## Requirements

- Rust 1.88 or newer (only for building from source)
- An API key for your chosen provider (not needed for local models)
