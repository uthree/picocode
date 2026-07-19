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
  newline and the input box grows with the text; pasted newlines are kept
  (bracketed paste). `↑`/`↓` move between lines, plain `Enter` sends
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
  `submit_plan`
- **Approval flow**: destructive operations (bash, file writes) ask for y/n
  confirmation; reads run automatically; configurable allow/deny rules
- **Permission modes**: `Shift+Tab` cycles read-only (default — every write
  asks), edit (file writes run freely; commands follow the config rules), and
  plan (writes blocked — the model explores and proposes a plan first)
- **Multi-turn**: keeps conversation history and tool results across turns
- **Context compaction**: `/compact` replaces the history with an LLM-written
  summary to free context
- **Model switching**: define a model roster in the config file and switch at
  runtime with `/model <name>` — the conversation carries over. `/model` also
  asks the provider which models it actually serves (Ollama `/api/tags`,
  OpenAI-compatible `/v1/models`, Anthropic `/v1/models`) and any of those can
  be switched to directly, no config entry needed
- **Direct shell**: prefix the input with `!` to run a shell command yourself;
  the output is shown and recorded into the model's context
- **Instruction files**: `AGENTS.md` (configurable) is loaded into the system
  prompt automatically
- **Web search**: pluggable providers — DuckDuckGo (default, no key), a
  self-hosted SearXNG instance, or the Brave Search API
- **User questions**: the model can present concrete choices (`ask_user`); a
  dialog opens — pick with `↑`/`↓` and `Enter`, or `Esc` to dismiss (the model
  is told and proceeds on its own)

## Setup (local LLM)

```sh
brew install ollama
brew services start ollama
ollama pull qwen3:4b
cargo run
```

## Usage

```sh
picocode                                   # ollama/qwen3:4b (default)
picocode --model qwen3:8b                  # different model
picocode --provider anthropic              # uses ANTHROPIC_API_KEY
picocode --provider openai --model gpt-4o  # uses OPENAI_API_KEY
picocode --base-url http://host:8000/v1 --provider openai --model qwen3:4b
                                           # OpenAI-compatible server (vLLM etc.)
picocode --yolo                            # skip all approval prompts (dangerous)
```

CLI flags select an ad-hoc model and take precedence over the config file's
`[[models]]` entries. Base URL precedence: `--base-url` > config file >
environment variables (`OLLAMA_API_BASE_URL` / `OPENAI_BASE_URL` /
`ANTHROPIC_BASE_URL`) > provider default.

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
| `/model` | List configured models and the ones the provider serves; `/model <name>` switches (history carries over) |
| `/read-only` / `/edit` / `/plan` / `/bypass` | Switch to that permission mode directly (see below) |
| `/compact` | Compact the conversation into a summary |
| `/resume` | Pick a saved session (↑↓ + Enter, Esc cancels); `/resume <id>` resumes directly |
| `/clear` | Clear conversation history (a new session log starts) |
| `/quit` (`Ctrl+C`) | Quit |

Mouse capture is enabled for wheel scrolling, so terminal-native text selection
needs the usual bypass modifier held (`Shift` on most terminals, `Option`/`Fn`
on macOS ones).

## Permission modes

`Shift+Tab` cycles the permission mode, shown in the status bar; `/read-only`,
`/edit`, `/plan` and `/bypass` switch to a specific mode directly:

| Mode | Behavior |
|---|---|
| `read-only` (default) | Read tools run freely; **every** write or command asks for confirmation, even if allow-listed |
| `edit` | `write_file` / `edit_file` run without asking; `bash` and other tools follow the `[approval]` config rules |
| `plan` | Writes and commands are **auto-denied**: the model investigates with the read tools, then submits its plan via `submit_plan`, which opens an approval dialog. Approving switches to `edit` mode and the model executes the plan in the same turn; declining sends it back to planning |
| `bypass` | **Everything runs without confirmation** (deny rules still apply). Meant for isolated environments such as containers. Not in the `Shift+Tab` cycle — only the explicit `/bypass` command enters it, with a warning; `Shift+Tab` leaves it for `read-only` |

Deny rules and `--yolo` take precedence over the mode. Full precedence:
deny rules > `--yolo` > mode > allow rules > ask. A switch takes effect
immediately, including for later tool calls of a turn already running.

## Sessions

Every conversation is saved automatically after each completed turn to
`$XDG_DATA_HOME/picocode/sessions/<project>/<id>.json` (default
`~/.local/share/…`), including both the model history and the rendered
transcript. `/resume` opens a dialog listing this project's sessions
newest-first — pick one with `↑`/`↓` and `Enter`. The session is restored into
the current model and keeps writing to the same log. Empty conversations are
never written.

## Configuration

picocode reads `picocode.toml` from the working directory, merged over the
global `~/.config/picocode/config.toml` (project values win; approval lists are
concatenated).

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
allow_tools = ["write_file"]      # tools that run without a prompt
deny_tools  = ["web_fetch"]       # tools that are always denied (wins over --yolo)
allow_bash  = ["cargo", "git status", "ls"]
deny_bash   = ["sudo", "rm -rf"]

[search]
provider = "duckduckgo"           # default; no API key needed
max_results = 5
# provider = "searxng"            # self-hosted metasearch
# base_url = "http://localhost:8888"   # required; enable `format: json` server-side
# provider = "brave"              # Brave Search API; needs BRAVE_API_KEY
```

Bash rules split the command at `&&` `||` `;` `|` `&` and newlines, then match
each segment by **word-boundary prefix** (`cargo` matches `cargo build` but not
`cargofoo`; a trailing `*` as in `cargo *` is accepted and ignored):

- `deny_bash`: if any segment matches, the call is auto-denied — **even with
  `--yolo`**
- `allow_bash`: the call auto-runs only if **every** segment matches; commands
  containing command substitution (`` ` `` or `$(`) never auto-run
- anything else falls back to the normal y/n approval prompt

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
  tools/       — built-in tool implementations
```
