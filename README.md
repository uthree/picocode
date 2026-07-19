# picocode

A minimal TUI coding agent written in Rust — a pocket-sized take on opencode.

Built on [rig](https://github.com/0xPlaygrounds/rig)'s provider abstractions and
[ratatui](https://ratatui.rs). Defaults to a local LLM via Ollama, and can talk
to Anthropic, OpenAI, or any OpenAI-compatible server (vLLM, etc.).

## Features

- **TUI chat**: streaming output, scrolling that stays put while the model is
  generating, token usage on the right of the status bar — a flat
  tqdm-style context-window gauge (green/yellow/red by pressure) plus a live
  `↑ prefill ↓ decode` counter while generating. The activity indicator
  distinguishes *waiting* (request sent, no tokens yet) from *running*
  (tokens streaming). Model reasoning is collapsed by default (`Ctrl+T` to
  expand)
- **Multi-line input**: `\` + `Enter` (also `Alt+Enter` or `Ctrl+J`) inserts a
  newline and the input box grows with the text; pasting multi-line text
  inserts it as one block — via bracketed paste, or, on terminals without
  it, by treating a burst of simultaneous keystrokes around an Enter as a
  paste instead of a submission. Long pastes (6+ lines or 500+ chars)
  collapse into a `[Pasted text #1 +N lines]` placeholder — deleted as one
  unit, shown collapsed in the transcript, and expanded to the full text
  for the model on send. `↑`/`↓` move between lines, plain `Enter` sends
- **Markdown rendering**: replies are rendered — headings, bold/italic,
  inline code, lists, quotes, links, and tables (box-drawn, column-aligned)
- **Syntax highlighting**: fenced code blocks in replies are highlighted
  (via syntect, language taken from the ```` ```lang ```` tag)
- **Diffs**: `edit_file` shows a line diff (and `write_file` its added lines)
  both in the approval dialog and in the transcript, so changes are visible
  even in modes that skip the confirmation. Additions/removals are marked by
  the background color (delta-style) while the text keeps its syntax
  highlighting, picked from the file extension
- **10 built-in tools**: `read_file` / `list_files` / `grep` / `write_file` /
  `edit_file` / `bash` / `web_search` / `web_fetch` / `ask_user` /
  `submit_plan`. File tools are confined to the project directory —
  absolute paths and `..` escapes are rejected
- **Approval flow**: anything that changes state or talks to the network
  (bash, file writes, web search/fetch) asks for y/n confirmation by
  default; local reads run automatically. Config rules are absolute in
  every mode: deny always denies, allow always allows. `/permissions`
  shows the effective rules
- **Permission modes**: `Shift+Tab` cycles read-only (default — destructive
  calls ask), edit (file writes run freely), and plan (bash and file writes
  denied — the model explores and proposes a plan first)
- **Multi-turn**: keeps conversation history and tool results across turns
- **Context compaction**: `/compact` replaces the history with an LLM-written
  summary to free context
- **Model switching**: `/model` opens a selection dialog listing the
  configured `[[models]]` entries plus the models the provider actually
  serves (Ollama `/api/tags`, OpenAI-compatible `/v1/models`, Anthropic
  `/v1/models`) — pick with `↑`/`↓` and `Enter`; no config entry needed for
  served models and the conversation carries over. `/model <name>` switches
  directly. The last-used model is remembered per project and restored on
  the next start
- **Direct shell**: prefix the input with `!` to run a shell command yourself;
  the input box turns yellow while typing one, and the output is shown and
  recorded into the model's context
- **Instruction files**: `AGENTS.md` (configurable) is loaded into the system
  prompt automatically
- **Web search**: pluggable providers — DuckDuckGo (default, no key), a
  self-hosted SearXNG instance, or the Brave Search API
- **User questions**: the model can present concrete choices (`ask_user`); a
  dialog opens — pick with `↑`/`↓` and `Enter`, or `Esc` to dismiss (the model
  is told and proceeds on its own)

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

Keys inside the TUI:

| Key | Action |
|---|---|
| `Enter` | Send |
| `\` + `Enter` (or `Alt+Enter` / `Ctrl+J`) | Insert a newline (pasting multi-line text works too) |
| `Tab` | Command completion (popup appears on `/`; repeat to cycle) |
| `Shift+Tab` | Cycle the permission mode (cycles the completion popup backwards while it is open) |
| `↑` / `↓` | Select a completion candidate; move between lines in a multi-line input |
| `y` / `n` | Approve / deny a tool call |
| `Esc` | Stop the generation in progress |
| `PgUp` / `PgDn` / mouse wheel | Scroll (follow resumes at the bottom) |
| `Ctrl+T` | Expand / collapse model reasoning |
| `!<command>` | Run a shell command directly (no approval — you typed it; output joins the context) |
| `/model` | Model-selection dialog (configured + provider-served models); `/model <name>` switches directly (history carries over) |
| `/read-only` / `/edit` / `/plan` / `/bypass` | Switch to that permission mode directly (see below) |
| `/permissions` | Show the current mode and the effective allow/deny rules |
| `/status` (or `/usage`) | Overview: model, endpoint, mode, token usage, session, config |
| `/compact` | Compact the conversation into a summary |
| `/resume` | Pick a saved session (↑↓ + Enter, Esc cancels); `/resume <id>` resumes directly |
| `/clear` | Clear conversation history (a new session log starts) |
| `/quit` (`Ctrl+C`) | Quit |

Mouse capture is enabled for wheel scrolling, so terminal-native text selection
needs the usual bypass modifier held (`Shift` on most terminals, `Option`/`Fn`
on macOS ones).

## Permissions

Tools fall into two classes. **Local reads** (`read_file`, `list_files`,
`grep`) and the dialog tools always run. Everything that changes state or
talks to the network (`bash`, `write_file`, `edit_file`, `web_search`,
`web_fetch`) is **destructive** and asks for y/n confirmation by default.
File tools only ever touch the project directory: absolute paths and `..`
escaping the root are rejected (the model is pointed at `bash`, which asks).

One precedence, in every mode: **deny rules > mode (plan/bypass) > allow
rules > ask**. `/permissions` prints the effective rules at any time.

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
the model history and the rendered transcript. `/resume` opens a dialog listing this project's sessions
newest-first — pick one with `↑`/`↓` and `Enter`. The session is restored into
the current model and keeps writing to the same log. Empty conversations are
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

The `bash` tool runs commands via `sh -c` (`cmd /C` on Windows). Bash rules
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
