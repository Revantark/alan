# Alan

A minimal coding agent in your terminal.

> Alan is currently an early prototype. Some features are still being developed, and behavior may change between releases.

## Table of Contents

- [Installation](#installation)
- [Providers](#providers)
- [Usage](#usage)
- [Local models](#local-models)
- [Requirements](#requirements)

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

Run the generated PowerShell installer from a PowerShell prompt:

```powershell
irm https://github.com/Revantark/alan/releases/latest/download/alan-installer.ps1 | iex
```

### Updating

Run the same installer command again to install the latest release. To see the installed version:

```bash
alan --version
```

## Providers

- **OpenRouter**
- **Google** (beta)
- **Zai** (beta)

Use the `/login` command to login to any of the available providers.
For openrouter, per model custom provider can be set via /model-provider "paste name without quotes"

Eg:
- /model-provider deepseek
- /model-provider xiaomi/fp8

## Usage

- Slash commands:
  - `/models` — pick a model from the provider catalog
  - `/plan` — toggle plan mode (also `Shift+Tab`)
  - `/review` — toggle review mode (also `Shift+Tab`)
  - `/normal` — turn off plan and review mode
  - `/login` — sign in to a provider interactively
  - `/new` — start a fresh session
  - `/summarize-new [focus]` — summarize and restart; optional quoted focus hint
    - Eg: `/summarize-new` just take the XYZ details from the context and strip everything else
  - `/effort` — set reasoning effort (`none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`)
  - `/fork` — fork session from a checkpoint
  - `/local` — manage local models (add, remove, edit)
  - `/help` — list available commands

## Local models

Supports openai compatible models only for now.

```bash
/local add
```

Opens an overlay with fields: Model ID, URL, API, API Key. Use `/local remove` and `/local edit` to manage. No API key or `/login` needed for local.

## Requirements

- Rust 1.88 or newer (to build from source)
- An API key for your chosen provider (not needed for local)
