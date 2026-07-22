# TUI reference

Key bindings, slash commands, and the finer points of the picocode TUI.
For setup see the [README](../README.md); for models, permissions and the
config file see [configuration.md](configuration.md).

## Keys

| Key | Action |
|---|---|
| `Enter` | Send |
| `\` + `Enter` (or `Alt+Enter` / `Ctrl+J`) | Insert a newline (pasting multi-line text works too) |
| `Tab` | Command completion (popup appears on `/`; repeat to cycle) |
| `Shift+Tab` | Cycle the permission mode (cycles the completion popup backwards while it is open) |
| `↑` / `↓` | Select a completion candidate; move between lines in a multi-line input; at the top/bottom line, recall previously submitted messages (shell-style input history) |
| `y` / `n` | Approve / deny a tool call |
| `a` | Approve and don't ask again for similar calls this session (the dialog shows the allow rule it adds) |
| `Esc` | Stop the generation in progress, or a running `!` shell command (the process is killed) |
| `PgUp` / `PgDn` / mouse wheel | Scroll (follow resumes at the bottom) |
| `Ctrl+T` | Expand / collapse model reasoning |
| `Ctrl+C` / `Ctrl+D` | Quit |

Mouse capture is enabled for wheel scrolling, so terminal-native text selection
needs the usual bypass modifier held (`Shift` on most terminals, `Option`/`Fn`
on macOS ones).

## Commands

| Command | Action |
|---|---|
| `!<command>` | Run a shell command directly (no approval — you typed it; output joins the context) |
| `/model` | Model-selection dialog (configured + provider-served models); `/model <name>` switches directly (history carries over) |
| `/read-only` / `/edit` / `/plan` / `/bypass` | Switch to that permission mode directly |
| `/permissions` | Show the current mode and the effective allow/deny rules |
| `/config` (or `/settings`) | Settings dialog: permission mode, reasoning display, bash timeout, read_file limits (lines / bytes per line), web search provider / result count and the auto-compact threshold (`←`/`→` change, apply immediately, session-only), plus the model picker on `Enter`. Search providers with unmet requirements (searxng without `base_url`, brave without `BRAVE_API_KEY`) are skipped |
| `/status` (or `/usage`) | Overview: model, endpoint, mode, token usage, session, config |
| `/compact` | Compact the conversation into a summary. Also runs automatically after a turn once context usage reaches the `auto_compact` threshold (default 85% of the window; 0 or the `/config` "off" setting disables) |
| `/undo` | Revert the file edits of the most recent turn that made any — modified files are restored, created files deleted — and tell the model so. Repeat to walk further back (up to 20 turns). Only `edit_file` changes are covered: side effects of `bash` (or `!`) commands are not tracked |
| `/attach <path>` | Stage a file to send with the next prompt: images as multimodal content (audio/PDF on providers that take them — Ollama is images-only), anything that reads as text (markdown, source code, …) inlined as text. `/attach` lists what's staged, `/attach clear` unstages all |
| `/resume` | Pick a saved session (↑↓ + Enter, Esc cancels); `/resume <id>` resumes directly |
| `/clear` | Clear conversation history (a new session log starts) |
| `/quit` (`Ctrl+C`) | Quit |

## Display details

- **Status bar**: token usage on the right — a flat tqdm-style context-window
  gauge (green/yellow/red by pressure) plus a live `↑ prefill ↓ decode`
  counter while generating. The activity indicator distinguishes *waiting*
  (request sent, no tokens yet) from *running* (tokens streaming). The
  permission mode is shown on the left.
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
- **Long-running commands**: a bash command (model-invoked or `!`) still
  running after the configured timeout becomes a background job instead of
  being killed; the status bar shows `⏳ N bg` while jobs are running. When
  a job finishes, a notice with its output appears and the model is
  prompted with the result automatically so it responds to it.
- **Plan approval**: in plan mode the model submits its plan (`submit_plan`)
  through a dialog — pick with `↑`/`↓` and `Enter`, or `Esc` to dismiss (the
  model is told and proceeds on its own).
