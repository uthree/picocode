# TUI reference

Key bindings, slash commands, and the finer points of the picocode TUI.
For setup see the [README](../README.md); for models, permissions and the
config file see [configuration.md](configuration.md).

## Remote workspaces

`picocode --remote host:/path` (or `--remote <name>` for a `[[remotes]]`
entry) opens a project on another machine over SSH. All the tools then
operate on the host: `read_file`, `edit_file`, `list_files` and `grep`
run over the connection, `bash` runs on the host. Authentication is
entirely your `ssh` setup (`~/.ssh/config`, keys, agent, ProxyJump) —
picocode shells out to `ssh` through a ControlMaster socket and never
handles credentials. The input box shows `host:path`. The conversation
log is saved locally, keyed by host+path.

The host's own `picocode.toml` and instruction files (AGENTS.md) apply on
top of your local config, so a remote project keeps its approval rules,
`after_edit` hook, system prompt and limits. What the host cannot change
is anything that would run or connect from *your* machine: `[[models]]`,
`[[mcp_servers]]`, `[sandbox]` and `[search]` are always taken from the
local config. The `[sandbox]` guard does not apply to a remote host
either (its bash runs with the host's own permissions). Unix hosts only.

`/remote` opens the workspace dialog — the local project, the configured
`[[remotes]]`, and a "+ add a remote…" row for a host that isn't
configured yet. The add-remote form takes an ssh destination and a path
on it (the `Host` aliases from `~/.ssh/config` are listed to pick from),
connects to check both, and prints a ready-to-paste `[[remotes]]` snippet
for picocode.toml. `/remote <name|host:/path>` switches directly and
`/remote local` comes back. Switching reconnects and starts a fresh
conversation in the new workspace (the old session log stays on disk).

## Windows

Windows support is experimental: the `bash` tool (and the `!` escape and
`after_edit` hook) runs `cmd.exe` instead of `sh`, and the system prompt
tells the model so. Development and testing happen primarily on
macOS/Linux — please report anything broken.

## Headless mode

`picocode -p "<prompt>"` runs one prompt without the TUI: the reply
streams to stdout (clean text, pipeable) while tool activity, notices and
errors go to stderr. Tool calls that would need confirmation are denied —
add `--bypass` to allow everything (isolated environments only). `--attach
<path>` (repeatable) attaches files to the prompt.

```sh
picocode -p "Summarize what this project does" > summary.txt
```

## Keys

| Key | Action |
|---|---|
| `Enter` | Send (configurable — see below) |
| `\` + `Enter` (or `Alt+Enter` / `Ctrl+J`) | Insert a newline (pasting multi-line text works too) |
| `Tab` | Completion (popup appears on `/`; repeat to cycle). Works for arguments too: `/model` completes model names, `/resume` session ids, `/attach` file paths |
| `Shift+Tab` | Cycle the permission mode (cycles the completion popup backwards while it is open) |
| `↑` / `↓` | Select a completion candidate; move between lines in a multi-line input; at the top/bottom line, recall previously submitted messages (shell-style input history) |
| `y` / `n` | Approve / deny a tool call |
| `a` | Approve and don't ask again for similar calls this session (the dialog shows the allow rule it adds) |
| `Esc` | Stop the generation in progress, or a running `!` shell command (the process is killed) |
| `PgUp` / `PgDn` / mouse wheel | Scroll (follow resumes at the bottom) |
| `Ctrl+V` | Paste from the system clipboard: copied files (Finder/Explorer) and images (screenshots) are staged as attachments — images are saved to a temp PNG first — plain text pastes normally. The terminal's own paste shortcut keeps working for text |
| `Ctrl+T` | Expand / collapse model reasoning |
| `Ctrl+C` / `Ctrl+D` | Quit |

Mouse capture is enabled for wheel scrolling, so terminal-native text selection
needs the usual bypass modifier held (`Shift` on most terminals, `Option`/`Fn`
on macOS ones).

### The send key

Which key sends is a setting: `submit_key` in picocode.toml (`"enter"`,
`"shift-enter"`, `"ctrl-enter"` or `"cmd-enter"`), or the "send key" row of
`/config` for the current session. Whichever key sends, the other Enter
combinations insert a newline, and `Alt+Enter` and `\` + `Enter` always do.

Reporting `Shift+Enter` and `Cmd+Enter` at all requires a terminal that
implements the kitty keyboard protocol (kitty, Ghostty, WezTerm, foot, and
iTerm2 with the option enabled); picocode asks for it at startup when the
terminal advertises support. Elsewhere those two combinations never arrive,
so `Enter` keeps sending and a startup warning says so. `Ctrl+Enter` works
everywhere: terminals without the protocol send it as `Ctrl+J`, which counts
as the same key.

## Commands

| Command | Action |
|---|---|
| `!<command>` | Run a shell command directly (no approval — you typed it; output joins the context) |
| `/model` | Model-selection dialog (configured + provider-served models); `/model <name>` switches directly (history carries over). The last row, "+ add a provider / model…", opens a form: pick a provider (←→), optionally a base URL, then type a model or fetch the endpoint's list with Tab and pick one — switching this way is an ad-hoc selection (remembered per project) and prints a ready-to-paste `[[models]]` snippet for picocode.toml |
| `/read-only` / `/edit` / `/plan` / `/auto` / `/bypass` | Switch to that permission mode directly. `/auto` hands the approval prompts to a reviewer model (warned about on entry — see [configuration.md](configuration.md#auto-mode-letting-a-model-answer-the-prompts)) |
| `/goal <condition>` | Keep working until the condition is met: after each turn a reviewer model judges it and, while it is not reached, picocode starts another turn on its own (up to `goal_max_rounds`, default 10). `/goal` alone shows the current goal, `/goal off` clears it. The status bar shows `goal n/m` while one is set. `Esc` stops the run in progress; the goal stays set (so the next message resumes towards it) until it is reached or cleared |
| `/permissions` | Show the current mode and the effective allow/deny rules |
| `/config` (or `/settings`) | Settings dialog: permission mode, send key, reasoning display, bash timeout, read_file limits (lines / bytes per line), web search provider / result count, the reply-length cap ("max tokens" — stepping below 1024 turns it off, leaving the limit to the provider) and the auto-compact threshold (`←`/`→` change, apply immediately, session-only), plus the model picker on `Enter`. Search providers with unmet requirements (searxng without `base_url`, brave without `BRAVE_API_KEY`) are skipped |
| `/status` (or `/usage`) | Overview: model, endpoint, mode, token usage, session, config — plus a color-coded context breakdown (segmented bar + legend: system prompt, instructions, user/assistant messages, tool activity, attachments, overhead, free) estimated from the real conversation history |
| `/compact` | Compact the conversation into a summary — the last 2 user turns survive verbatim (the current task's context), only older messages are summarized. Also runs automatically after a turn once context usage reaches the `auto_compact` threshold (default 85% of the window; 0 or the `/config` "off" setting disables). As a softer stage, at 2/3 of that threshold old tool outputs are replaced with placeholders first (a notice reports how many) |
| `/undo` | Revert the file edits of the most recent turn that made any — modified files are restored, created files deleted — and tell the model so. Repeat to walk further back (up to 20 turns). Only `edit_file` changes are covered: side effects of `bash` (or `!`) commands are not tracked |
| `/jobs` | List running background jobs (id, elapsed, command); `/jobs kill <id>` stops one — the kill is reported as the job's result, so the model knows too. Tab completes the ids |
| `/attach <path>` | Stage a file to send with the next prompt: images as multimodal content (audio/PDF on providers that take them — Ollama is images-only), anything that reads as text (markdown, source code, …) inlined as text. `/attach` lists what's staged, `/attach clear` unstages all. `Ctrl+V` stages copied files and clipboard images the same way |
| `/prompt` | Edit the system prompt in the input box (loaded with the current one; Enter applies for this session — the worker restarts with the conversation carried over — and a ready-to-paste `system_prompt` snippet for picocode.toml is shown; Esc cancels). `/prompt <name>` switches to a `[[prompts]]` preset (Tab completes; unique substrings resolve), `/prompt reset` restores the built-in default |
| `/remote` | Workspace dialog: the local project, the configured `[[remotes]]`, and "+ add a remote…" — a form (name / ssh destination / path, with `~/.ssh/config` aliases listed to pick) that connects and prints a `[[remotes]]` snippet for picocode.toml. `/remote <name\|host:/path>` switches directly (Tab completes the names), `/remote local` comes back. The new workspace reconnects, applies its own picocode.toml and instruction files, and starts a fresh conversation |
| `/resume` | Pick a saved session (↑↓ + Enter, Esc cancels); `/resume <id>` resumes directly |
| `/clear` | Clear conversation history (a new session log starts) |
| `/quit` (`Ctrl+C`) | Quit |

## Display details

- **Status bar**: token usage on the right — a flat tqdm-style context-window
  gauge (green/yellow/red by pressure) plus, while tokens stream, a live
  `↓` output counter with the generation speed (`tok/s`, over a rolling
  window). The activity indicator distinguishes *waiting* (request sent,
  no tokens yet) from *running* (tokens streaming). The permission mode
  is shown on the left.
- **Markdown rendering**: replies are rendered — headings, bold/italic,
  inline code, lists, quotes, links, and tables (box-drawn, column-aligned).
  Fenced code blocks are syntax-highlighted (via syntect, language taken
  from the ```` ```lang ```` tag).
- **Diffs**: `edit_file` shows a line diff (all additions when it creates a
  file) both in the approval dialog and in the transcript, so changes are
  visible even in modes that skip the confirmation. Additions/removals are
  marked by the background color (delta-style) while the text keeps its
  syntax highlighting, picked from the file extension.
- **Reasoning**: model reasoning is collapsed to a one-liner by default;
  `Ctrl+T` expands it.
- **Scrolling**: while scrolled up, the view is anchored so streaming output
  doesn't drag it along; scrolling past the bottom resumes following.

## Input details

- **Multi-line input**: the input box grows with the text (up to 8 lines);
  `↑`/`↓` move between lines, plain `Enter` sends.
- **Input history**: `↑` at the input's first line recalls previously
  submitted messages (newest first), `↓` at the last line walks forward
  again — past the newest entry, the unsubmitted text you were typing is
  restored. Editing a recalled message makes it the current input, like in
  a shell. The history is per run and consecutive duplicates collapse.
- **Paste handling**: pasting multi-line text inserts it as one block — via
  bracketed paste, or, on terminals without it, by treating a burst of
  simultaneous keystrokes around an Enter as a paste instead of a
  submission. Long pastes (6+ lines or 500+ chars) collapse into a
  `[Pasted text #1 +N lines]` placeholder — deleted as one unit, shown
  collapsed in the transcript, and expanded to the full text for the model
  on send.
- **Direct shell**: the input box turns yellow while typing a `!` command,
  and cyan while typing a `/` command.
- **Workdir title**: the input box's top border shows the project directory
  (`~`-shortened) and the git branch, refreshed after each turn.
- **Mid-turn messages**: a message sent while the model is still working is
  steered into the running turn — delivered at the next tool-call boundary
  (or as an immediate follow-up prompt), so the model adjusts course without
  waiting for the turn to end. Messages with staged attachments wait for
  the turn to finish instead.
- **Long-running commands**: a bash command (model-invoked or `!`) still
  running after the configured timeout becomes a background job instead of
  being killed; the status bar shows `⏳ N bg` while jobs are running. When
  a job finishes, a notice with its output appears and the model is
  prompted with the result automatically so it responds to it.
- **Plan approval**: in plan mode the model submits its plan (`submit_plan`)
  through a dialog — pick with `↑`/`↓` and `Enter`, or `Esc` to dismiss (the
  model is told and proceeds on its own).
