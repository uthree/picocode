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

### The context window

The active model's window sizes the status-bar usage gauge and the
auto-compact threshold — and on Ollama it is also what the server
allocates, because picocode sends it as `num_ctx` on every request.

That last part is worth knowing if you run Ollama: **a `num_ctx` set on
the Ollama side does not apply.** A per-request option wins over the
Modelfile, so `PARAMETER num_ctx`, `/set parameter num_ctx` and
`OLLAMA_CONTEXT_LENGTH` are all overridden. This is deliberate — Ollama's
own default is 4096, which cuts long replies off mid-sentence — but it
means the window has to be set here.

Two places to set it:

- `context_window` on a `[[models]]` entry, for the durable answer.
- The `/config` "context window" row, which doubles and halves it live.
  A change there is remembered next to the model in the project state, so
  it survives a restart — including for an ad-hoc `--model` selection,
  which has no entry to declare one. Switching models drops it and adopts
  the new model's own figure, since a window that fits one model is wrong
  for the next.

With neither, the window is 32768 tokens. `/config` abbreviates its
counts — `32k`, `128k`, `1M` — dividing by 1024 where that comes out
even, since that is how the models themselves are named. The bash
timeout is a clock rather than a count: `30s`, `1m30s`, `30m`.

#### What the provider says

picocode asks the provider how large a context the active model takes, and
the `/config` stepper stops there rather than at an arbitrary constant.

Not every provider answers. Ollama does, from `/api/show`; Anthropic does,
as `max_input_tokens`; among OpenAI-compatible servers, vLLM and llama.cpp
do, while api.openai.com's model listing carries only ids. When nothing
comes back the stepper keeps its own flat ceiling of 1M.

The figure is **adopted as the value only on Anthropic**, where the
provider owns the memory behind the window as well as the number. Ollama's
is the model's built-in maximum, and the KV cache for it comes out of your
own machine — a 9B with a 262144 window will happily try to allocate tens
of gigabytes. There it bounds the setting rather than choosing it. Either
way a window you set yourself, in `picocode.toml` or in `/config`, is left
alone.

## Permissions

Tools fall into two classes. **Local reads** (`read_file`, `list_files`,
`grep`) and the dialog tools always run. Everything that changes state or
talks to the network (`bash`, `edit_file`, `web_search`, `web_fetch`) is
**destructive** and asks for y/n confirmation by default.
File tools only ever touch the project directory: absolute paths and `..`
escaping the root are rejected, and so are paths that stay inside the root
only until a symlink is followed — the kind a cloned repository can carry
(the model is pointed at `bash`, which asks). A remote workspace keeps the
lexical check alone: its paths live on the other host.

`web_fetch` will not reach link-local or private addresses — cloud metadata
endpoints (169.254.169.254) and internal services — whatever the hostname
resolves to, and every redirect hop is checked the same way. Loopback stays
reachable, so a local dev server still works. Responses stop downloading at
2 MB rather than being buffered first.

One precedence, in every mode: **deny rules > mode (plan/bypass) > allow
rules > ask**, with one exception in auto mode, where the always-ask
commands come before the allow rules (see
[auto mode](#auto-mode-letting-a-model-answer-the-prompts)).
`/permissions` prints the effective rules at any time.

Allow rules come from the config file or from the approval dialog's `a`
(always) answer, which adds one at runtime — the tool's name to
`allow_tools`, or for bash the command's program (+ subcommand) prefix to
`allow_bash` (`cargo build --release` adds `cargo build`; `ls -la` adds
`ls`). Runtime additions last until picocode exits; copy them into
`picocode.toml`'s `[approval]` section to make them permanent.

`Shift+Tab` cycles the permission mode, shown in the status bar; `/read-only`,
`/edit`, `/plan`, `/auto` and `/bypass` switch to a specific mode directly:

| Mode | Behavior |
|---|---|
| `read-only` (default) | Destructive calls ask, unless allow-listed |
| `edit` | Like read-only, plus `edit_file` runs without asking |
| `plan` | `bash` and file writes are **denied** (even if allow-listed): the model investigates, then submits its plan via `submit_plan`, which opens an approval dialog. Approving switches to `edit` mode and the model executes the plan in the same turn. Web tools stay available under the usual ask/allow rules |
| `auto` | Like `edit`, except the confirmations are answered by a **reviewer model** instead of you — see below. Not in the `Shift+Tab` cycle: only `/auto` or `--auto` enter it, with a warning |
| `bypass` | **Everything runs without confirmation** (deny rules still apply). Meant for isolated environments such as containers — the `--bypass` flag starts in it. Not in the `Shift+Tab` cycle — only `/bypass` or `--bypass` enter it, with a warning; `Shift+Tab` leaves it for `read-only` |

A mode switch takes effect immediately, including for later tool calls of a
turn already running.

### Auto mode: letting a model answer the prompts

In `auto` mode every call that would have asked you is put to a separate,
tool-less *reviewer* agent running on the same model: it sees the tool name,
the arguments, the working directory and what you asked for this turn, and
answers `ALLOW` or `DENY` with a reason. Both are printed in the transcript
as they happen, so nothing runs unattended without a record.

The delegation is bounded on every side:

- deny rules and plan-mode blocks are decided before the reviewer is asked;
- the always-ask commands below reach you even when an `allow_bash` rule
  covers them: those rules bound what *you* have to confirm, and in auto
  mode you are not the one answering (in every other mode an allow rule
  still allows, since you chose it and you are there);
- destructive or outward-facing commands always ask **you**, whatever the
  reviewer would say: `sudo`/`su`/`doas`, `rm` reaching outside the project
  (an absolute path or `~`), `git push`, `git reset --hard`, `mkfs`, `dd`,
  `shutdown`/`reboot`, `chown`, `chmod 777`, `npm publish`, `cargo publish`,
  `kubectl`, `terraform apply`, `docker system prune`, and anything piping a
  download into a shell. The cmd.exe equivalents count too, since that is
  what the bash tool runs on Windows: `runas`, `format`, `diskpart`,
  `bcdedit`, `vssadmin`, `takeown`, `icacls`, `cipher`, `reg delete`,
  `net user`/`net localgroup`, `sc delete`, `del`/`rd`/`rmdir` reaching
  outside the project or recursing, `Remove-Item -Recurse`/`-Force`, and
  `iwr … | iex`;
- an error, a timeout (90s) or a reply naming neither verdict falls back to
  your confirmation — never to "allow";
- a refusal is reported to the model as the tool result, so it adapts
  instead of failing.

A small local model is a mediocre security reviewer, and everything it reads
from the project (file contents, command output) is untrusted input that may
try to talk it into approving something. Use auto mode where a wrong
approval would be recoverable, and keep `deny_bash`/`deny_tools` as the
hard boundary. `--auto` starts in it.

## Sessions

Every conversation is saved automatically after each completed turn to
`$XDG_DATA_HOME/picocode/sessions/<project>/<id>.json` (default
`~/.local/share/…`; on Windows the home is `%USERPROFILE%`), including both
the model history and the rendered transcript. `/resume` picks one from a
dialog listing this project's sessions newest-first; it is restored into the
current model and keeps writing to the same log. Empty conversations are
never written. The TUI and the GUI share the same session store, so either
front end can resume the other's conversations.

`<project>` is the project path with non-alphanumeric characters replaced by
`-`, plus a short digest of the full path — the readable part alone is not
unique (`~/work/a-b` and `~/work/a/b` flatten to the same thing), and two
projects sharing one store would show each other's conversations. A store
written by an earlier version, under the name without the digest, is moved
across the first time the project is opened.

## The config file

picocode reads `picocode.toml` from the project root — the nearest ancestor
of the current directory containing one, so starting from a subdirectory
finds the same config, sessions and saved state — merged over the global
`~/.config/picocode/config.toml`. Project values win; approval lists are
concatenated; `[[models]]` and `default_model` travel together (a project
that defines its own `[[models]]` starts from a clean slate).

Settings marked below as adjustable in `/config` can also be changed while
picocode runs. Those changes are saved to
`$XDG_DATA_HOME/picocode/settings.json` and re-applied on the next start,
by either front end — the TUI and the GUI share the file, since they share
the dialog. It is a sparse overlay: only what you actually changed is
stored, so everything else keeps following `picocode.toml`, and a stored
value wins over a later edit to the config file (it was chosen more
recently). Delete the file to go back to following `picocode.toml`
entirely. Two settings are deliberately not in it: the permission mode
resets each run, and the context window is remembered per project next to
the model it was set on.

### Trusting a project config

A project config is not only preferences: `after_edit` runs a shell command
after every write, `[[mcp_servers]]` launches a child process at startup,
`[approval] allow_tools` / `allow_bash` pre-authorise tool calls, `[sandbox]`
can switch the OS guard off, and a `base_url` decides which endpoint your
provider API key is sent to. Since the file is found by walking *up* from the
working directory, it may not even belong to the project you opened — a
`picocode.toml` in `~/Downloads` covers everything below it.

So those settings wait for you. On the first start in a project, everything
else in `picocode.toml` applies as usual and the list above is held back with
a notice naming what was skipped. Read the file, then:

```
/trust           # allow this picocode.toml, from the next start
/trust revoke    # take it back
```

Trust is recorded as the file's SHA-256 in
`$XDG_DATA_HOME/picocode/trusted.json`, so *any* later edit — the agent
writing to `picocode.toml` included — puts the gate back up. Deny rules
(`deny_tools`, `deny_bash`) are never gated: a project can only tighten with
those. The global config is never gated either; you put it there yourself.

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

# Which key sends the message in the input box: "enter" (default),
# "shift-enter", "ctrl-enter" or "cmd-enter" (Super+Enter off macOS).
# Whichever is chosen, the other Enter combinations insert a newline. Also
# switchable at runtime in /config. A remote workspace's config never
# overrides it: it belongs to the machine you type on.
# In the TUI, Shift+Enter and Cmd+Enter need a terminal implementing the
# kitty keyboard protocol (kitty, Ghostty, WezTerm, foot); elsewhere Enter
# keeps sending and picocode says so at startup. Ctrl+Enter works
# everywhere — terminals without the protocol report it as Ctrl+J.
submit_key = "enter"

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
# (`/remote local` comes back). `/remote` with no argument opens a picker
# whose "+ add a remote…" form tries a destination live and prints the
# entry to paste here.
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

# Cap on the tokens one reply may generate. 0 means picocode sets no cap
# and leaves the limit to the provider. Adjustable at runtime in the
# `/config` "max tokens" row (the worker rebuilds its agents; the
# conversation is kept).
#
# For Ollama this also decides what actually reaches the server: picocode
# sends it as `num_predict`, next to the `num_ctx` from the active model's
# context window. Both matter — a reply is cut off mid-sentence when
# prompt + reply fill the window, no matter what max_tokens says. See
# "The context window" above.
max_tokens = 8192

# How many follow-up turns a `/goal` may run before it stops and hands
# back to you (1-100). The goal loop asks a reviewer model after each
# turn whether the goal is reached and keeps working until it is — this
# caps how much it may spend unattended.
goal_max_rounds = 10

# Optional, opt-in: MCP (Model Context Protocol) servers. With none
# configured no MCP code runs and nothing changes for the model — extra
# tools confuse small local models, so this is deliberately off by
# default. Servers connect once at startup (a failure is shown and
# skipped); their tools join the agent's tool set and, being unknown to
# the approval rules, ask for confirmation like the destructive
# built-ins (allow-list them via [approval] allow_tools to auto-run).
# "Unknown to the approval rules" is a check by name, so a server tool
# named after a built-in (read_file, grep, …) is refused with a notice
# rather than registered: it would otherwise replace that built-in and
# inherit its approval class — read_file and grep are not destructive,
# so it would run with no prompt at all. Names are unique across servers
# for the same reason.
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
context_window = 32768       # tokens (default 32768); also adjustable in
                             # /config. On Ollama this is the `num_ctx`
                             # picocode sends, overriding the server's own
                             # setting — see "The context window" above.

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

`base_url` belongs to the provider it is written next to. Switching provider
in `/config` does not carry it across — brave requests send `BRAVE_API_KEY`
in a header, and a searxng endpoint following the switch would have handed
the key to that host.

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
- matching is case-sensitive on unix, where the case is part of the command's
  identity, and case-insensitive on Windows, where cmd.exe treats `DEL` and
  `del` as one command — a rule that only caught one spelling there would
  not be a rule
- tool names in `allow_tools` / `deny_tools` are validated at startup, so a
  typo is an error instead of a silently dead rule
