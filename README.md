# picocode

A minimal TUI coding agent written in Rust — a pocket-sized take on opencode.

Built on [rig](https://github.com/0xPlaygrounds/rig)'s provider abstractions and
[ratatui](https://ratatui.rs). Defaults to a local LLM via Ollama, and can talk
to Anthropic, OpenAI, or any OpenAI-compatible server (vLLM, etc.).

## Features

- **TUI chat** with streaming output, markdown rendering, syntax-highlighted
  code blocks and line diffs for file edits
- **8 built-in tools**: `read_file` / `list_files` / `grep` / `edit_file`
  (replace a string, or create/overwrite a whole file) / `bash` /
  `web_search` / `web_fetch` / `submit_plan` — file tools are confined to
  the project directory, and the web tools can be switched off entirely
  with `disable_tools`
- **Approval flow** for anything that changes state or talks to the network:
  `y` / `n`, or `a` (always) to whitelist similar calls for the session
- **Permission modes**: read-only / edit / plan / bypass (`Shift+Tab` cycles)
- **Model switching** (`/model`, listing configured and provider-served
  models), **context compaction** (`/compact`), **session autosave and
  resume** (`/resume`), **settings dialog** (`/config`)
- **Direct shell** (`!<command>`), **instruction files** (`AGENTS.md`),
  **pluggable web search** (DuckDuckGo / SearXNG / Brave)

Key bindings, slash commands and display details: [docs/tui.md](docs/tui.md).

## Setup (local LLM)

```sh
brew install ollama          # other platforms: https://ollama.com/download
brew services start ollama
ollama pull qwen3:4b
cargo run
```

## Usage

```sh
picocode                                   # last-used model, else the first model Ollama serves
picocode --model qwen3:8b                  # different model
picocode --provider anthropic              # uses ANTHROPIC_API_KEY
picocode --provider openai --model gpt-4o  # uses OPENAI_API_KEY
picocode --base-url http://host:8000/v1 --provider openai --model qwen3:4b
                                           # OpenAI-compatible server (vLLM etc.)
picocode --bypass                          # start in bypass mode (isolated envs)
```

`--provider` / `--model` select an ad-hoc model and take precedence over the
config file's `[[models]]` entries and the saved state. `--base-url` is not a
selection — it only overrides the endpoint of whatever model is active. Base
URL precedence: `--base-url` > config file > environment variables
(`OLLAMA_API_BASE_URL` / `OPENAI_BASE_URL` / `ANTHROPIC_BASE_URL`) > provider
default.

The model a run starts with (or is switched to) is remembered per project in
`$XDG_DATA_HOME/picocode/state/<project>.json` and restored on the next start;
CLI flags always win. With no flags, no saved state and no `[[models]]`
entries, picocode asks the local Ollama server for its model list and uses
the first one — if Ollama is unreachable or empty, it exits with instructions
for setting up a provider instead.

## Permissions

Tools fall into two classes. **Local reads** (`read_file`, `list_files`,
`grep`) and the dialog tools always run. Everything that changes state or
talks to the network (`bash`, `edit_file`, `web_search`, `web_fetch`) is
**destructive** and asks for y/n confirmation by default.
File tools only ever touch the project directory: absolute paths and `..`
escaping the root are rejected (the model is pointed at `bash`, which asks).

One precedence, in every mode: **deny rules > mode (plan/bypass) > allow
rules > ask**. `/permissions` prints the effective rules at any time.

Allow rules come from the config file or from the approval dialog's `a`
(always) answer, which adds one at runtime — the tool's name to
`allow_tools`, or for bash the command's program (+ subcommand) prefix to
`allow_bash` (`cargo build --release` adds `cargo build`; `ls -la` adds
`ls`). Runtime additions last until picocode exits; copy them into
`picocode.toml`'s `[approval]` section to make them permanent.

`Shift+Tab` cycles the permission mode, shown in the status bar; `/read-only`,
`/edit`, `/plan` and `/bypass` switch to a specific mode directly:

| Mode | Behavior |
|---|---|
| `read-only` (default) | Destructive calls ask, unless allow-listed |
| `edit` | Like read-only, plus `edit_file` runs without asking |
| `plan` | `bash` and file writes are **denied** (even if allow-listed): the model investigates, then submits its plan via `submit_plan`, which opens an approval dialog. Approving switches to `edit` mode and the model executes the plan in the same turn. Web tools stay available under the usual ask/allow rules |
| `bypass` | **Everything runs without confirmation** (deny rules still apply). Meant for isolated environments such as containers — the `--bypass` flag starts in it. Not in the `Shift+Tab` cycle — only `/bypass` or `--bypass` enter it, with a warning; `Shift+Tab` leaves it for `read-only` |

A mode switch takes effect immediately, including for later tool calls of a
turn already running.

## Sessions

Every conversation is saved automatically after each completed turn to
`$XDG_DATA_HOME/picocode/sessions/<project>/<id>.json` (default
`~/.local/share/…`; on Windows the home is `%USERPROFILE%`), including both
the model history and the rendered transcript. `/resume` picks one from a
dialog listing this project's sessions newest-first; it is restored into the
current model and keeps writing to the same log. Empty conversations are
never written.

## Configuration

picocode reads `picocode.toml` from the project root — the nearest ancestor
of the current directory containing one, so starting from a subdirectory
finds the same config, sessions and saved state — merged over the global
`~/.config/picocode/config.toml`. Project values win; approval lists are
concatenated; `[[models]]` and `default_model` travel together (a project
that defines its own `[[models]]` starts from a clean slate).

```toml
default_model = "local"    # [[models]] entry used at startup (default: first)

# Instruction files loaded into the system prompt (default: ["AGENTS.md"])
instructions = ["AGENTS.md"]

# Seconds before a bash command is moved to the background (default: 120;
# also adjustable at runtime in /config)
bash_timeout = 120

# read_file output limits (defaults shown; also adjustable in /config)
read_max_lines = 2000        # max lines per call
read_max_line_bytes = 500    # bytes per line before truncation

# Auto-compact the conversation when the context usage crosses this percent
# of the window, checked after each turn (default: 85; 0 disables; also
# adjustable in /config)
auto_compact = 85

# Leave tools unregistered entirely — their schemas are never sent to the
# model, saving context on models that don't need them. Only the web tools
# can be listed; default: [] (everything on). Needs a restart to change.
disable_tools = ["web_search", "web_fetch"]

# Optional: replace the built-in base system prompt entirely. `{root}` expands
# to the working directory; instruction files are still appended after it.
system_prompt = """
You are a careful coding assistant working in {root}.
Prefer small, verifiable changes.
"""

[[models]]
name = "local"
provider = "ollama"
model = "qwen3:4b"
context_window = 32768       # tokens; drives the status-bar usage gauge
                             # (default 32768 when omitted)

[[models]]
name = "vllm"
provider = "openai"          # any OpenAI-compatible server
model = "qwen3:8b"
base_url = "http://host:8000/v1"
# If OPENAI_API_KEY is unset, a placeholder key is sent — fine for local
# servers that don't check it.

[[models]]
name = "opus"
provider = "anthropic"
model = "claude-opus-4-8"

[approval]
allow_tools = ["web_search"]      # destructive tools that never ask (any mode)
deny_tools  = ["web_fetch"]       # tools that are always denied (any mode)
allow_bash  = ["cargo", "git status", "ls"]
deny_bash   = ["sudo", "rm -rf"]  # always denied, even in bypass mode

[search]
provider = "duckduckgo"           # default; no API key needed
max_results = 5                   # provider and max_results are also
                                  # switchable at runtime in /config
# provider = "searxng"            # self-hosted metasearch
# base_url = "http://localhost:8888"   # required; enable `format: json` server-side
# provider = "brave"              # Brave Search API; needs BRAVE_API_KEY
```

The `bash` tool runs commands via `sh -c` (`cmd /C` on Windows). A command
still running after `bash_timeout` seconds is not killed but moved to a
**background job**: the model is told right away and the status bar counts
the running jobs; when a job finishes, its output is shown and the model is
prompted with it automatically so it reacts to the result. `Esc` stops a
command that is still in the foreground (the process is killed).

Bash rules
split the command at `&&` `||` `;` `|` `&` and newlines, then match
each segment by **word-boundary prefix** (`cargo` matches `cargo build` but not
`cargofoo`; a trailing `*` as in `cargo *` is accepted and ignored):

- `deny_bash`: if any segment matches, the call is auto-denied — in **every**
  mode, bypass included (`!` commands you type yourself are exempt)
- `allow_bash`: the call auto-runs only if **every** segment matches; commands
  containing substitution (`` ` `` or `$(`) or output redirection (`>`) never
  auto-run, and an environment-variable prefix (`FOO=1 cargo …`) doesn't
  prefix-match, so it asks
- anything else falls back to the normal y/n approval prompt
- tool names in `allow_tools` / `deny_tools` are validated at startup, so a
  typo is an error instead of a silently dead rule

## Layout

A two-crate workspace: everything UI-independent lives in `picocode-core`,
and the TUI is one front end on top of it (a GUI could be another). The two
sides talk exclusively through the `AgentEvent` / `WorkerCmd` channels and
the plain data types in `transcript` — nothing in the core depends on a
rendering library.

```
crates/
  picocode-core/src/     — the agent engine (library)
    config/      — CLI args and config file (mod), permission modes and
                   approval rules (rules), web-search settings (search)
    agent.rs     — rig agent construction and the streaming worker
    approval.rs  — approval gate for destructive tools (rig AgentHook)
    event.rs     — AgentEvent / WorkerCmd: the core ⇄ front-end protocol
    models.rs    — provider model-list queries backing /model
    session.rs   — session autosave/load backing /resume
    state.rs     — per-project persisted state (last-used model)
    tools/       — built-in tool implementations
    transcript.rs — renderer-agnostic transcript entries (Entry/EntryKind)
  picocode-tui/src/      — the ratatui front end (binary `picocode`)
    main.rs      — entry point (+ --smoke headless debug mode)
    app.rs       — application state and event loop
    ui.rs        — ratatui rendering (transcript / input / status bar / dialogs)
    input.rs     — input thread, paste detection, input-box cursor math
    history.rs   — shell-style ↑/↓ input history
    highlight.rs — syntax highlighting (syntect) and line diffs (similar)
    markdown.rs  — markdown renderer for assistant replies (pulldown-cmark)
```
