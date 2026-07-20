# Architecture

A workspace: everything UI-independent lives in `picocode-core`, and each
front end is a crate on top of it. Front ends talk to the core exclusively
through the `AgentEvent` / `WorkerCmd` channels and the plain data types in
`transcript` — nothing in the core depends on a rendering library.

```
crates/
  picocode-core/src/     — the agent engine (library)
    config/      — CLI args and config file (mod), permission modes and
                   approval rules (rules), web-search settings (search)
    agent.rs     — rig agent construction and the streaming worker
    approval.rs  — approval gate for destructive tools (rig AgentHook)
    event.rs     — AgentEvent / WorkerCmd: the core ⇄ front-end protocol
    models.rs    — provider model-list queries backing /model
    session.rs   — session autosave/load backing /resume
    state.rs     — per-project persisted state (last-used model)
    tools/       — built-in tool implementations
    transcript.rs — renderer-agnostic transcript entries (Entry/EntryKind)
  picocode-tui/src/      — the ratatui front end (binary `picocode`)
    main.rs      — entry point (+ --smoke headless debug mode)
    app.rs       — application state and event loop
    ui.rs        — ratatui rendering (transcript / input / status bar / dialogs)
    input.rs     — input thread, paste detection, input-box cursor math
    history.rs   — shell-style ↑/↓ input history
    highlight.rs — syntax highlighting (syntect) and line diffs (similar)
    markdown.rs  — markdown renderer for assistant replies (pulldown-cmark)
  picocode-gui/src/      — experimental gpui front end (binary `picocode-gui`)
    main.rs      — window bootstrap, tokio ⇄ gpui bridge (+ --smoke auto-prompt)
    chat.rs      — chat view: transcript, input, status bar, dialogs, menus
    math.rs      — TeX span extraction and Unicode fallback (unicodeit)
    tex.rs       — display-math typesetting via RaTeX (cached PNGs)
    assets.rs    — embedded icon SVGs served to gpui-component
```

The agent worker runs on tokio in both front ends; the TUI drives it from
its own tokio main loop, while the GUI creates a runtime beside gpui's
executor and pumps the event channel from a gpui task.
