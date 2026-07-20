# picocode

A minimal TUI coding agent written in Rust — a pocket-sized take on opencode.

Built on [rig](https://github.com/0xPlaygrounds/rig)'s provider abstractions and
[ratatui](https://ratatui.rs). Defaults to a local LLM via Ollama, and can talk
to Anthropic, OpenAI, or any OpenAI-compatible server (vLLM, etc.).

## Features

- **TUI chat** with streaming output, markdown rendering, syntax-highlighted
  code blocks and line diffs for file edits
- **10 built-in tools**: `read_file` / `list_files` / `grep` / `write_file` /
  `edit_file` / `bash` / `web_search` / `web_fetch` / `ask_user` /
  `submit_plan` — file tools are confined to the project directory
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
talks to the network (`bash`, `write_file`, `edit_file`, `web_search`,
`web_fetch`) is **destructive** and asks for y/n confirmation by default.
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
| `edit` | Like read-only, plus `write_file` / `edit_file` run without asking |
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
max_results = 5
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

```
src/
  main.rs      — entry point (+ --smoke headless debug mode)
  config.rs    — CLI args, config file, approval rules
  app.rs       — application state and event loop
  ui.rs        — ratatui rendering (transcript / input / status bar / approval modal)
  agent.rs     — rig agent construction and the streaming worker
  approval.rs  — approval gate for destructive tools (rig AgentHook)
  highlight.rs — syntax highlighting (syntect) and line diffs (similar)
  markdown.rs  — markdown renderer for assistant replies (pulldown-cmark)
  models.rs    — provider model-list queries backing /model
  state.rs     — per-project persisted state (last-used model)
  tools/       — built-in tool implementations
```
