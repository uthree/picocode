# GUI reference

An experimental native front end built on [gpui](https://gpui.rs) and
[gpui-component](https://github.com/longbridge/gpui-component), running the
same engine (`picocode-core`) as the TUI.

```sh
cargo run -p picocode-gui        # accepts the same CLI flags as the TUI
```

## Features

- **Chat**: streaming replies rendered as markdown with syntax-highlighted
  code blocks; reasoning shown dimmed (collapsible via `/config`); tool
  calls with per-tool icons and accent colors, outputs as attached blocks;
  colored `edit_file` line diffs in the transcript and the approval dialog.
- **TeX math**: display equations (`$$…$$`, `\[…\]`) are typeset by
  [RaTeX](https://github.com/erweixin/RaTeX) as images in the theme color;
  inline math falls back to Unicode (`$x^2$` → `x²`, via unicodeit).
- **Scrolling**: the transcript is a virtualized list — only the entries
  in (or near) the viewport are rendered each frame, so long conversations
  scroll and stream as fast as short ones. The view follows streaming
  output until you scroll up, and resumes following at the bottom.
- **Approvals**: y/n/always dialog for destructive tools (bash commands
  shown bare, edits as diffs) — answer by button or key: `y` approve,
  `n`/`Esc` deny, `a` always. The plan-approval dialog for `submit_plan`
  dismisses with `Esc`; focus returns to the input afterwards.
- **Status bar**: clickable mode chip (left) opening the mode menu;
  context-usage gauge colored by pressure and a clickable model chip
  (right) opening the model menu — configured `[[models]]` entries plus
  whatever the provider reports serving, with the conversation carried
  over on switch.
- **Workdir line**: the project directory and git branch (with an icon)
  sit above the input box, refreshed after each turn. Clicking the
  directory opens a native folder picker and moves the project root
  there — the new directory's config, instructions and saved state are
  loaded, a fresh worker and conversation start (the current model is
  kept unless the new project selects its own), and the permission mode
  and persisted settings carry over.
- **Input**: auto-growing multi-line field (1–8 rows); Enter sends,
  Shift+Enter inserts a newline; IME composition works. Typing `/` opens a
  slash-command completion popup — Tab fills and cycles, click fills.
- **Copying**: assistant text is selectable (Cmd+C copies the selection),
  every code block has a copy button in its top-right corner, and
  right-clicking any transcript entry opens a "Copy text" menu.
- **Commands**: `/clear`, `/compact`, `/model`, `/resume`, the mode
  commands, `/config` (settings dialog with the same rows as the TUI),
  `/status`, `/permissions`, `/quit`.
- **Sessions**: autosaved after each turn to the same per-project store as
  the TUI, so either front end can resume the other's conversations.
- **Theme**: follows the system light/dark appearance live by default; the
  `/config` theme row forces light or dark.
- **Persistence**: `/config` changes are saved to
  `$XDG_DATA_HOME/picocode/gui-settings.json` and re-applied on the next
  start, as a sparse overlay — untouched values keep following
  `picocode.toml`, a saved value wins over later config-file edits. The
  permission mode is deliberately not persisted; the model is already
  remembered per project.
- **i18n**: UI strings localize to the system language (English and
  Japanese so far — `crates/picocode-gui/locales/*.yml`, via rust-i18n).

## Build notes

- gpui is built with its `runtime_shaders` feature, so a full Xcode
  install (the `metal` CLI) is not required on macOS.
- `--smoke <prompt>` auto-sends the prompt once the window opens — a debug
  aid that exercises the whole worker ⇄ view bridge on launch.
- The icons gpui-component references are embedded in
  `crates/picocode-gui/src/assets.rs` (Lucide, ISC license); add new
  entries there when using more icon-bearing components.
