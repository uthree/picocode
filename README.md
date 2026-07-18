# picocode

A minimal TUI coding agent written in Rust — a pocket-sized take on opencode.

Built on [rig](https://github.com/0xPlaygrounds/rig)'s provider abstractions and
[ratatui](https://ratatui.rs). Defaults to a local LLM via Ollama, and can talk
to Anthropic, OpenAI, or any OpenAI-compatible server (vLLM, etc.).

## Features

- **TUI chat**: streaming output, scrolling that stays put while the model is
  generating, token usage in the status bar. Model reasoning is collapsed by
  default (`Ctrl+T` to expand)
- **7 built-in tools**: `read_file` / `list_files` / `grep` / `write_file` /
  `edit_file` / `bash` / `web_fetch`
- **Approval flow**: destructive operations (bash, file writes) ask for y/n
  confirmation; reads run automatically; configurable allow/deny rules
- **Multi-turn**: keeps conversation history and tool results across turns
- **Context compaction**: `/compact` replaces the history with an LLM-written
  summary to free context
- **Model switching**: define a model roster in the config file and switch at
  runtime with `/model <name>` — the conversation carries over
- **Direct shell**: prefix the input with `!` to run a shell command yourself;
  the output is shown and recorded into the model's context
- **Instruction files**: `AGENTS.md` (configurable) is loaded into the system
  prompt automatically

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
| `Tab` / `Shift+Tab` | Command completion (popup appears on `/`; repeat to cycle) |
| `↑` / `↓` | Select a completion candidate |
| `y` / `n` | Approve / deny a tool call |
| `PgUp` / `PgDn` | Scroll (follow resumes at the bottom) |
| `Ctrl+T` | Expand / collapse model reasoning |
| `!<command>` | Run a shell command directly (no approval — you typed it; output joins the context) |
| `/model` | List models; `/model <name>` switches (history carries over) |
| `/compact` | Compact the conversation into a summary |
| `/resume` | List saved sessions; `/resume <n>` (or an id) restores one |
| `/clear` | Clear conversation history (a new session log starts) |
| `/quit` (`Ctrl+C`) | Quit |

## Sessions

Every conversation is saved automatically after each completed turn to
`$XDG_DATA_HOME/picocode/sessions/<project>/<id>.json` (default
`~/.local/share/…`), including both the model history and the rendered
transcript. `/resume` lists this project's sessions newest-first; `/resume <n>`
restores the selected one into the current model and continues writing to the
same log. Empty conversations are never written.

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
  tools/       — built-in tool implementations
```
