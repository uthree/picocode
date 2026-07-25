# Configuration reference

Model selection, permissions, sessions, and the `picocode.toml` config file.
For key bindings and display details see [tui.md](tui.md) / [gui.md](gui.md).

## Model selection

```sh
picocode                                   # last-used model, else the first model Ollama serves
picocode --model qwen3:8b                  # different model
picocode --provider anthropic              # uses ANTHROPIC_API_KEY
picocode --provider openai --model gpt-4o  # uses OPENAI_API_KEY
picocode --base-url http://host:8000/v1 --provider openai --model qwen3:4b
                                           # OpenAI-compatible server (vLLM etc.)
```

On the Anthropic provider, requests use the API's automatic prompt caching
(the repeated prefix an agent loop resends — system prompt, tools, history —
is cached server-side, cutting cost and latency; no configuration needed).

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
never written. The TUI and the GUI share the same session store, so either
front end can resume the other's conversations.

## The config file

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

# Shell command run after every successful edit_file write; its verdict is
# appended to the tool result the model sees ("passed", or the failure
# output), so breakage surfaces immediately without relying on the model
# remembering to verify. Runs in the project root, 120 s cap, no approval
# (you configured it). Default: unset.
after_edit = "cargo check"

# Optional: replace the built-in base system prompt entirely. `{root}` expands
# to the working directory; instruction files are still appended after it.
# The /prompt command edits this at runtime (session-only) and prints a
# ready-to-paste snippet for this file.
system_prompt = """
You are a careful coding assistant working in {root}.
Prefer small, verifiable changes.
"""

# Optional: named system-prompt presets, switched at runtime with
# `/prompt <name>` (Tab completes the names). Each is a full replacement
# for the base prompt, like `system_prompt`; a project's list replaces
# the global one.
[[prompts]]
name = "strict"
prompt = """
You are a careful coding assistant in {root}. Make minimal, verifiable
changes and run the tests after every edit.
"""

# Optional: named remote workspaces for `--remote <name>`. Each opens a
# project on another host over SSH — every tool then operates on the
# remote (read/edit/list/grep over the connection, bash on the host).
# Authentication is entirely your ssh setup (~/.ssh/config, keys, agent);
# picocode never handles credentials. The conversation log stays local.
# You can also pass `--remote host:/path` directly without an entry here,
# or switch workspaces at runtime with `/remote <name|host:/path>`
# (`/remote local` comes back).
# The remote root's own picocode.toml and instruction files (AGENTS.md)
# are merged over this one, so a host project keeps its approval rules,
# after_edit hook, system prompt/presets and limits. A host cannot change
# what would run or connect locally: [[models]], [[mcp_servers]],
# [sandbox] and [search] always come from the local config (such sections
# in a remote file are ignored). Note: the [sandbox] guard is local-only —
# remote bash runs with the host's permissions. Unix hosts only.
[[remotes]]
name = "prod"
host = "user@prod.example.com"    # or an ssh alias from ~/.ssh/config
path = "/srv/app"

# Optional, opt-in: OS-level sandbox for model-initiated bash commands
# (`!` commands and after_edit are user-authored and stay unsandboxed).
# Writes are confined to the project root, the temp directories and
# allow_write; network can be blocked. macOS uses sandbox-exec
# (Seatbelt), Linux uses Landlock (kernel 5.13+ required — commands fail
# instead of silently running unconfined; the TCP block needs 6.7+ and
# stays off quietly on older kernels). Windows is not supported: bash
# fails with a clear error while a sandbox is requested. For truly
# untrusted work a container is still the stronger isolation.
[sandbox]
mode = "bypass"          # "off" (default) | "bypass" | "always"
allow_network = false    # block network from sandboxed commands
allow_write = ["~/.cargo"]  # extra write-allowed paths (~ expands)

# Optional, opt-in: MCP (Model Context Protocol) servers. With none
# configured no MCP code runs and nothing changes for the model — extra
# tools confuse small local models, so this is deliberately off by
# default. Servers connect once at startup (a failure is shown and
# skipped); their tools join the agent's tool set and, being unknown to
# the approval rules, ask for confirmation like the destructive
# built-ins (allow-list them via [approval] allow_tools to auto-run).
# Exactly one of `command` (stdio child process) or `url`
# (streamable HTTP) per server.
[[mcp_servers]]
name = "time"
command = "uvx"
args = ["mcp-server-time"]
# env = { SOME_TOKEN = "…" }        # extra env for the child process

#[[mcp_servers]]
#name = "remote"
#url = "http://localhost:8000/mcp"

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

## The bash tool

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
