# TUI reference

Key bindings, slash commands, and the finer points of the picocode TUI.
For setup, permissions and configuration, see the [README](../README.md).

## Keys

| Key | Action |
|---|---|
| `Enter` | Send |
| `\` + `Enter` (or `Alt+Enter` / `Ctrl+J`) | Insert a newline (pasting multi-line text works too) |
| `Tab` | Command completion (popup appears on `/`; repeat to cycle) |
| `Shift+Tab` | Cycle the permission mode (cycles the completion popup backwards while it is open) |
| `↑` / `↓` | Select a completion candidate; move between lines in a multi-line input |
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
| `/config` (or `/settings`) | Settings dialog: permission mode, reasoning display, max turns per prompt and bash timeout (`←`/`→` change, apply immediately, session-only), plus the model picker on `Enter` |
| `/status` (or `/usage`) | Overview: model, endpoint, mode, token usage, session, config |
| `/compact` | Compact the conversation into a summary |
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
- **Diffs**: `edit_file` shows a line diff (and `write_file` its added lines)
  both in the approval dialog and in the transcript, so changes are visible
  even in modes that skip the confirmation. Additions/removals are marked by
  the background color (delta-style) while the text keeps its syntax
  highlighting, picked from the file extension.
- **Reasoning**: model reasoning is collapsed to a one-liner by default;
  `Ctrl+T` expands it.
- **Scrolling**: while scrolled up, the view is anchored so streaming output
  doesn't drag it along; scrolling past the bottom resumes following.

## Input details

- **Multi-line input**: the input box grows with the text (up to 8 lines);
  `↑`/`↓` move between lines, plain `Enter` sends.
- **Paste handling**: pasting multi-line text inserts it as one block — via
  bracketed paste, or, on terminals without it, by treating a burst of
  simultaneous keystrokes around an Enter as a paste instead of a
  submission. Long pastes (6+ lines or 500+ chars) collapse into a
  `[Pasted text #1 +N lines]` placeholder — deleted as one unit, shown
  collapsed in the transcript, and expanded to the full text for the model
  on send.
- **Direct shell**: the input box turns yellow while typing a `!` command.
- **Long-running commands**: a bash command (model-invoked or `!`) still
  running after the configured timeout becomes a background job instead of
  being killed; the status bar shows `⏳ N bg` while jobs are running. When
  a job finishes, a notice with its output appears and the model is
  prompted with the result automatically so it responds to it.
- **User questions**: the model can present concrete choices (`ask_user`); a
  dialog opens — pick with `↑`/`↓` and `Enter`, or `Esc` to dismiss (the
  model is told and proceeds on its own).
