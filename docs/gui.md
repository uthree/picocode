# GUI reference

An experimental native front end built on [gpui](https://gpui.rs) and
[gpui-component](https://github.com/longbridge/gpui-component), running the
same engine (`picocode-core`) as the TUI.

```sh
cargo run -p picocode-gui        # accepts the same CLI flags as the TUI
```

It accepts the same `--remote host:/path` flag as the TUI to open a
project on another machine over SSH, and the same `/remote` command to
switch workspaces at runtime (all tools operate on the host; see the
[TUI reference](tui.md#remote-workspaces) for the details).

On macOS a prebuilt binary is also on the
[releases page](https://github.com/uthree/picocode/releases) with its own
shell installer (`picocode-gui-installer.sh`); launch it as
`picocode-gui`. It is a bare binary rather than a .app bundle, so there
is no Dock icon or double-click launch — packaging (and
signing/notarization) may come later.

## Features

- **Chat**: streaming replies rendered as markdown with syntax-highlighted
  code blocks; model reasoning collapsed to a dimmed one-line preview —
  click it to expand or fold that entry; tool calls with per-tool icons
  and accent colors, outputs as attached blocks;
  `edit_file` line diffs in the transcript and the approval dialog, with
  additions/removals marked by the row background while the text keeps its
  syntax highlighting (picked from the file extension, theme-aware).
- **TeX math**: display equations (`$$…$$`, `\[…\]`) are typeset by
  [RaTeX](https://github.com/erweixin/RaTeX) as images in the theme color;
  inline math falls back to Unicode (`$x^2$` → `x²`, via unicodeit).
- **Raw transcript**: the `raw` chip in the status bar (or the "raw
  transcript" row of `/config`) drops all of the above and shows the log as
  plain monospaced text — no markdown, no math, no syntax highlighting, no
  diff colors. Each entry gets a dim `[user]` / `[assistant]` / `[tool]` …
  label and then its text exactly as it arrived. Text is not selectable in
  this view (the selectable element is the markdown one, and parsing is
  what is being avoided) — right-click an entry to copy it whole. The chip
  stays lit while raw is on, and the setting is remembered (the TUI reads
  the same one).
- **Scrolling**: the transcript is a virtualized list — only the entries
  in (or near) the viewport are rendered each frame, so long conversations
  scroll and stream as fast as short ones. The view follows streaming
  output until you scroll up, and resumes following at the bottom.
- **Approvals**: y/n/always dialog for destructive tools (bash commands
  shown bare, edits as diffs) — answer by button or key: `y` approve,
  `n`/`Esc` deny, `a` always. The plan-approval dialog for `submit_plan`
  dismisses with `Esc`; focus returns to the input afterwards.
- **Status bar**: clickable mode chip (left) opening the mode menu (the
  `auto` entry hands approvals to a reviewer model); a `goal n/m` marker
  while a `/goal` is set;
  context-usage gauge colored by pressure and a clickable model chip
  (right) opening the model menu — configured `[[models]]` entries plus
  whatever the provider reports serving, with the conversation carried
  over on switch. The menu's "+ add a provider / model…" row opens a
  dialog for switching to any provider, endpoint and model: cycle the
  provider, optionally set a base URL, then type a model name or fetch
  the endpoint's model list and click one. Ad-hoc switches are remembered
  per project, and a ready-to-paste `[[models]]` snippet for
  picocode.toml lands in the transcript. While the model generates, the
  counter is quiet during the API wait, then shows a `↓` output counter
  ticking live from an estimate of the streamed deltas (with the
  generation speed in `tok/s` over a rolling window) that snaps to the
  provider-reported count at each completion boundary; idle shows both
  totals.
- **Workdir line**: the open workspace and the git branch (with an icon)
  sit above the input box, refreshed after each turn — a remote workspace
  shows `host:path`. Clicking it opens the workspace menu: the local
  project, every configured `[[remotes]]` entry (click to connect over
  SSH), "add a remote…" — a dialog taking an ssh destination and a path,
  with the `~/.ssh/config` aliases listed to click, that connects and
  prints a `[[remotes]]` snippet for picocode.toml — and
  "choose a folder…" for a native directory picker. Whichever
  you pick, the new workspace's config, instructions and saved state are
  loaded, a fresh worker and conversation start (the current model is
  kept unless the new workspace selects its own), and the permission mode
  and persisted settings carry over. `/remote` does the same from the
  input box.
- **Input**: auto-growing multi-line field (1–8 rows); Enter sends and
  Shift+Enter inserts a newline — swap that around in `/config` ("send
  key": Enter, Shift+Enter, Ctrl+Enter or Cmd+Enter; whichever sends, the
  others insert a newline, and the placeholder names both). IME
  composition works. Typing `/` opens a
  slash-command completion popup — Tab fills and cycles, click fills —
  which also completes arguments: `/model` offers model names and
  `/resume` session ids.
  A leading `!` runs the rest as a shell command directly (no model, no
  approval — you typed it); the output joins the transcript and the model's
  history like in the TUI. The input border turns yellow while typing a
  `!` command and cyan for a `/` command. Messages sent while the agent
  is still generating are not rejected: text-only messages are steered
  into the running turn — delivered at the next tool-call boundary (or as
  an immediate follow-up), so the model adjusts course without waiting —
  while messages with attachments (and `!` commands) queue above the
  input box and run one per completed turn; pressing Stop returns the
  queued ones to the input box.
- **Attachments**: drop files from the Finder anywhere on the window,
  click the 📎 button next to the input for a file picker, or paste with
  ⌘V (Ctrl+V off macOS): copied files and clipboard images (screenshots
  — saved to a temp PNG first) are staged as attachments, while plain
  text pastes into the input as usual. Images (and,
  depending on the provider, audio and PDFs — Ollama takes images only)
  are staged as chips above the input — image chips show a thumbnail, ✕
  removes one — and go to the model with the next prompt as multimodal
  message content. Files that aren't a known media type but read as text
  (markdown, source code, …) are attached as text and inlined into the
  message. Unsupported binary files are refused with a notice instead of
  being silently dropped. Sent attachments stay visible in the user's
  transcript bubble; clicking an image thumbnail there opens it full
  size (click again to close).
- **Copying**: assistant text and your own sent messages are selectable
  (Cmd+C copies the selection), every code block has a copy button in its
  top-right corner, and right-clicking any transcript entry opens a
  "Copy text" menu.
- **Commands**: the same set as the TUI (one shared parser, so aliases
  and errors behave identically): `/clear`, `/compact`, `/undo` (revert
  the last turn's file edits; repeatable — `edit_file` changes only, bash
  side effects are not tracked, and a file you edited yourself since that
  turn is left alone), `/jobs` (background-jobs popup — also
  reachable from the status-bar chip — with a kill button per job) and
  `/jobs kill <id>`, `/attach <path>` (stages like the 📎 button;
  `/attach` lists, `/attach clear` unstages), `/prompt` (a dialog editing
  the system prompt — applies to this session with the conversation
  carried over, and prints a picocode.toml snippet to persist it;
  `/prompt <name>` switches to a `[[prompts]]` preset, `/prompt reset`
  restores the built-in), `/remote` (opens the workspace menu;
  `/remote <name>` or `/remote local` switches over SSH without
  restarting), `/model`,
  `/resume`, the mode
  commands (including `/auto`, which hands the approval prompts to a
  reviewer model after a warning — see
  [configuration.md](configuration.md#auto-mode-letting-a-model-answer-the-prompts)),
  `/goal <condition>` (keep working until a reviewer model judges the
  condition met — `goal n/m` appears in the status bar, `/goal off`
  clears it, Stop ends the run in progress), `/config` (settings dialog, grouped
  into Model / Tools / Interface — the same rows as the TUI, minus its
  reasoning-display row, since reasoning folds per entry in the
  transcript, plus the GUI-only theme rows),
  `/status` (with a color-coded context breakdown — a segmented bar plus
  legend showing how much of the window the system prompt, instructions,
  messages, tool activity and attachments take; the attachments row names
  its source, see [attachment tokens](tui.md#attachment-tokens)),
  `/permissions`,
  `/trust` (allow this project's picocode.toml to run commands and relax
  approvals — see
  [configuration.md](configuration.md#trusting-a-project-config)), `/quit`.
- **Sessions**: autosaved after each turn to the same per-project store as
  the TUI, so either front end can resume the other's conversations.
- **Session sidebar**: this project's saved sessions, newest first, in a
  panel down the left edge — each row is the conversation's first prompt
  over its age and message count, and the one you are in is highlighted
  (a conversation with nothing saved yet shows as "new session"). Click
  a row to load it: the same path `/resume` takes, so a running turn
  refuses with a notice instead of switching under the agent's feet.
  Right-clicking a row opens a menu: open the session, copy its id (what
  `/resume <id>` takes) or delete it — deleting asks first, and deleting
  the conversation you are in also starts a fresh one so the next
  autosave doesn't write the file straight back. The ✎ button in its
  header starts a new session (`/clear` — the old one stays in the
  list), and the panel button at the top of the conversation shows or
  hides the sidebar, remembered across restarts. Switching workspaces
  re-lists the new project's sessions.
- **Toolbar**: above the conversation, the sidebar toggle on the left and
  a settings button on the right — the same dialog as `/config`.
- **Theme**: follows the system light/dark appearance live by default; the
  `/config` theme row forces light or dark. A separate "color theme" row
  picks the palette family — Default, Ayu, Catppuccin, Everforest,
  Flexoki, GitHub, Gruvbox, One (Atom) or Solarized — each pairing a light and a dark variant,
  so the appearance setting keeps deciding which of the two is showing.
  The bundled themes (Zed format, via gpui-component's theme registry) are
  written to `$XDG_DATA_HOME/picocode/themes/` at startup; both choices
  persist across restarts. All accent colors — the mode chip, per-tool log
  colors, the context gauge and breakdown legend, diff backgrounds, code
  blocks and their syntax highlighting — come from the selected theme's
  palette.
- **Persistence**: `/config` changes are saved to
  `$XDG_DATA_HOME/picocode/settings.json` and re-applied on the next
  start, as a sparse overlay — untouched values keep following
  `picocode.toml`, a saved value wins over later config-file edits. The
  file is shared with the TUI, so a bash timeout set in one is the timeout
  in the other; the GUI's own preferences (theme, color theme, sidebar)
  sit under its `ui` key. The permission mode is deliberately not
  persisted, and the model — with the context window set alongside it — is
  remembered per project instead. An older `gui-settings.json` is read
  once and carried over.
- **i18n**: UI strings localize to the system language (English and
  Japanese so far — `crates/picocode-gui/locales/*.yml`, via rust-i18n).
  The `/config` row labels come from `crates/picocode-core/locales/`, so
  both front ends name the settings the same way. `/config`'s **language**
  row overrides the system choice and is saved with the rest; it takes
  effect immediately, though text already in the transcript keeps the
  wording it was written with.

## Build notes

- gpui is built with its `runtime_shaders` feature, so a full Xcode
  install (the `metal` CLI) is not required on macOS.
- `--smoke <prompt>` auto-sends the prompt once the window opens — a debug
  aid that exercises the whole worker ⇄ view bridge on launch.
- The icons gpui-component references are embedded in
  `crates/picocode-gui/src/assets.rs` (Lucide, ISC license); add new
  entries there when using more icon-bearing components.
