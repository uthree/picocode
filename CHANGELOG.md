# Changelog

## Unreleased

### The context window is a setting now

It was readable in `/status` but settable only as `context_window` on a
`[[models]]` entry — and on Ollama that is the `num_ctx` picocode sends
with every request, which overrides the Modelfile, `/set parameter
num_ctx` and `OLLAMA_CONTEXT_LENGTH`. So a window set on the Ollama side
never applied, with nowhere in the app to correct it.

`/config` has a "context window" row in both front ends. A change there
is remembered per project next to the model it was set on, which also
gives an ad-hoc `--model` selection somewhere to keep one — that case
used to be pinned at 32768 whatever the model could do. Switching models
adopts the new model's own figure.

The row also asks the provider what the model actually takes, and stops
the stepper there instead of at an arbitrary constant. Ollama answers
from `/api/show`, Anthropic with `max_input_tokens`, vLLM and llama.cpp
on their model listings; api.openai.com says nothing and the stepper
keeps its flat 1M ceiling. Only Anthropic's figure is adopted as the
value: Ollama's is the model's built-in maximum, and the KV cache for it
comes out of your machine, so there it is a ceiling and not a suggestion.

### `/config`

- The rows are grouped into **Model**, **Tools** and **Interface**.
- Large counts are abbreviated: `32k` rather than `32768`, `1M` rather
  than `1048576`. Covers the context window, the reply cap and the two
  read_file limits.
- **The TUI remembers them.** Both front ends now write the same
  `$XDG_DATA_HOME/picocode/settings.json`, so a bash timeout set in one is
  the timeout in the other; the TUI used to forget on exit. The GUI's
  `gui-settings.json` is read once and carried over. The permission mode
  still resets each run.
- The TUI's dialog is localized (Japanese, following the system language),
  and both front ends take the row labels from one catalog, so they name
  the settings identically.

### Internal

- The rows are one table in `picocode-core`, keyed by a `SettingId`
  rather than by position. Adding a setting was an edit to a row array, a
  `match` on hard-coded indices and a row count in each front end, with
  nothing checking that a label and its arm still lined up.

## 0.8.0

An adversarial review of the whole workspace, and the fixes it turned up.
Most of this is security and data-safety work in `picocode-core`, so it
applies to the TUI and the GUI alike.

### Upgrading

- **A project `picocode.toml` now has to be trusted before it can run
  commands.** `after_edit`, `[[mcp_servers]]`, `[approval] allow_tools` /
  `allow_bash`, `[sandbox]` and any `base_url` are held back on first use,
  with a notice naming what was skipped. Read the file, then `/trust` to
  allow them from the next start (`/trust revoke` takes it back). Everything
  else in the file applies as before, and the global
  `~/.config/picocode/config.toml` is never gated. If a project of yours
  stops running its `after_edit` hook after upgrading, this is why.
- **`web_fetch` no longer reaches link-local or private addresses.**
  Loopback still works, so local dev servers are unaffected; a fetch aimed
  at `169.254.169.254` or an internal `10.x` service is refused.
- **In auto mode, an `allow_bash` rule no longer covers the always-ask
  commands.** `allow_bash = ["git"]` still auto-runs `git status`
  everywhere, but `git push` reaches you when nobody else is answering.
- Session stores are keyed by path *and* a digest of it. An existing store
  is renamed into place the first time you open the project — nothing to do.

### Fixed — escaping the project

- File tools confined paths by collapsing `..` and comparing prefixes, and
  never looked at symlinks. A directory symlink inside the project — the
  kind a cloned repository can carry — let `edit_file` write and
  `read_file` / `grep` / `list_files` read anywhere on disk, with no
  approval prompt, since `edit_file` is auto-approved in edit mode.
- An MCP server could register a tool named after a built-in. The approval
  hook classifies by name, so a server tool called `read_file` or `grep`
  replaced the built-in *and* ran with no prompt in any mode. Those names
  are refused at connect time now.
- The always-ask list that bounds auto mode was entirely POSIX, but the bash
  tool runs `cmd /C` on Windows: `del /s /q C:\…`, `format`, `runas`,
  `reg delete`, `Remove-Item -Recurse` and `iwr | iex` all reach the user
  now. Allow/deny rules also match case-insensitively there, since cmd.exe
  does.

### Fixed — losing data

- `/undo` used the local filesystem directly, so on a remote workspace it
  reverted nothing and reported success anyway. It goes through the
  workspace backend now.
- `/undo` also wrote the pre-turn contents back unconditionally, discarding
  anything you had edited by hand since. Those files are left alone and
  reported as skipped.
- `edit_file` truncated the target before refilling it, so a crash or a
  dropped connection mid-write left an empty or half-written file. Both
  backends write a temp file and rename it over the target, carrying the
  original's permissions.
- Two projects whose paths differ only in punctuation (`~/work/a-b` and
  `~/work/a/b`) shared one session store and saw each other's
  conversations.

### Fixed — everything else

- `web_fetch` buffered whole responses before checking the size cap, and
  followed redirects without re-checking where they led.
- Switching search provider in `/config` kept the previous provider's
  `base_url`, sending `BRAVE_API_KEY` to a searxng host.
- Fetched pages, search results and tool output are marked as data rather
  than instructions, and the system prompt says so once for all of them.
- GUI: dialogs are modal — clicking the dimmed backdrop no longer reaches
  the sidebar behind it. A slow model-list reply from a provider you
  switched away from no longer refills the menu. Deleting a session right
  after a turn no longer loses the race with its own autosave.
- TUI: `/remote` connects off the event loop, so the UI stays responsive
  while ssh works (and gives up on an unreachable host after 10s). The
  add-model form no longer panics when a second fetch returns a shorter
  list.
- `list_files` emitted mixed path separators on Windows.
- Attachments are read with a size cap instead of being loaded whole, and
  the clipboard scratch directory and ssh control socket use random,
  owner-only temp paths that are cleaned up on exit.

### Internal

- CI ran 21 of 168 tests: bare `cargo test` follows `default-members`,
  which is the TUI alone. It now covers the workspace on Linux and Windows,
  with the GUI on macOS. 23 regression tests came with the fixes above.

## 0.7.1 and earlier

See the [release history](https://github.com/uthree/picocode/releases).
