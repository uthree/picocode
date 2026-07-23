# picocode

A minimal coding agent written in Rust — a pocket-sized take on opencode,
with a terminal UI and an experimental native GUI.

Built on [rig](https://github.com/0xPlaygrounds/rig)'s provider abstractions,
[ratatui](https://ratatui.rs) and [gpui](https://gpui.rs). Defaults to a
local LLM via Ollama, and can talk to Anthropic, OpenAI, or any
OpenAI-compatible server (vLLM, etc.).

## Features

- **Chat** with streaming output, markdown rendering, syntax-highlighted
  code blocks and line diffs for file edits
- **8 built-in tools**: `read_file` / `list_files` / `grep` / `edit_file` /
  `bash` / `web_search` / `web_fetch` / `submit_plan` — file tools are
  confined to the project directory
- **Approval flow** for anything that changes state or talks to the
  network, with allow/deny rules and **permission modes**
  (read-only / edit / plan / bypass)
- **Model switching** (`/model`), **context compaction** (`/compact`),
  **session autosave and resume** (`/resume`), **settings** (`/config`),
  **direct shell** (`!<command>`), **instruction files** (`AGENTS.md`),
  **pluggable web search** (DuckDuckGo / SearXNG / Brave)
- **File attachments** — send images (and audio / PDF on providers that
  support them) with a prompt: drag & drop, 📎 or ⌘V in the GUI,
  `/attach` or Ctrl+V in the TUI — pasting clipboard files and
  screenshots stages them as attachments
- **Opt-in MCP** — connect Model Context Protocol servers (stdio or
  streamable HTTP) via `[[mcp_servers]]` in picocode.toml; their tools
  go through the same approval flow as the built-ins. Off by default:
  no configuration, no extra tools, nothing for a small model to
  get confused by
- **Opt-in sandbox** — `[sandbox]` in picocode.toml runs model-initiated
  bash commands inside the OS sandbox (macOS sandbox-exec, Linux
  Landlock): writes confined to the project root and temp, network
  optionally blocked — a guard rail for bypass mode

## Install

Prebuilt TUI binaries for macOS (Apple Silicon / Intel), Linux (x86_64)
and Windows (x86_64, experimental) are on the
[releases page](https://github.com/uthree/picocode/releases), or one line:

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/uthree/picocode/releases/latest/download/picocode-tui-installer.sh | sh
```

Windows (PowerShell):

```powershell
irm https://github.com/uthree/picocode/releases/latest/download/picocode-tui-installer.ps1 | iex
```

The native GUI ships as a prebuilt binary for macOS (installed the same
way — the curl download carries no quarantine attribute, so Gatekeeper
does not object; it is a bare binary, not a .app bundle):

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/uthree/picocode/releases/latest/download/picocode-gui-installer.sh | sh
```

Then launch it with `picocode-gui`. On other platforms the GUI is built
from source (see below).

## Quick start

```sh
brew install ollama          # other platforms: https://ollama.com/download
brew services start ollama
ollama pull qwen3:4b
cargo run                    # TUI
cargo run -p picocode-gui    # GUI (same flags)
```

Cloud providers and other endpoints:

```sh
picocode --provider anthropic              # uses ANTHROPIC_API_KEY
picocode --provider openai --model gpt-4o  # uses OPENAI_API_KEY
picocode --base-url http://host:8000/v1 --provider openai --model qwen3:4b
```

Headless (for scripts and pipes — the reply goes to stdout, tool logs to
stderr; confirmation-needing tool calls are denied unless `--bypass`):

```sh
picocode -p "Explain the build setup" --attach Cargo.toml
```

## Configuration

picocode reads `picocode.toml` from the project root, merged over the
global `~/.config/picocode/config.toml`:

```toml
[[models]]
name = "local"
provider = "ollama"
model = "qwen3:4b"

[approval]
allow_bash = ["cargo", "git status"]
deny_bash  = ["sudo", "rm -rf"]
```

Destructive tools ask for y/n confirmation by default; `a` (always)
whitelists similar calls for the session, and plan/bypass modes tighten or
lift the gate. Everything — model selection and precedence, all config
keys, permission modes and bash rules, sessions — is described in
[docs/configuration.md](docs/configuration.md).

## Documentation

- [docs/configuration.md](docs/configuration.md) — models, permissions,
  sessions, and every `picocode.toml` key
- [docs/tui.md](docs/tui.md) — TUI key bindings, slash commands, display
  details
- [docs/gui.md](docs/gui.md) — GUI features and build notes
- [docs/architecture.md](docs/architecture.md) — workspace layout: the
  `picocode-core` engine and the two front-end crates
