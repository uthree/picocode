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
    command.rs   — shared slash-command parser and command list (both
                   front ends execute the same parsed Command)
    attachment.rs — user file attachments (image/audio/PDF) sent as
                   multimodal message content, with per-provider support
                   checks
    clipboard.rs — system-clipboard reading for paste (files/images →
                   attachments), shared by both front ends
    event.rs     — AgentEvent / WorkerCmd: the core ⇄ front-end protocol
    history.rs   — context savings: old-tool-output pruning and the
                   keep-recent-turns boundary used by compaction
    models.rs    — provider model-list queries backing /model
    report.rs    — /status and /permissions text shared by both front ends
    session.rs   — session autosave/load backing /resume
    state.rs     — per-project persisted state (last-used model)
    steer.rs     — mid-turn steering queue + rig hook (injects user
                   messages at tool-call boundaries)
    tools/       — built-in tool implementations
    transcript.rs — renderer-agnostic transcript entries (Entry/EntryKind)
                   and diff-line parsing shared by both front ends
    undo.rs      — per-turn journal of pre-edit file states backing /undo
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
    chat/        — the chat view: state, events and commands (mod), entry
                   rendering (transcript), dialogs and popups (dialogs),
                   status bar and its menus (status)
    highlight.rs — tree-sitter syntax highlighting for diffs (cached)
    math.rs      — TeX span extraction and Unicode fallback (unicodeit)
    tex.rs       — display-math typesetting via RaTeX (cached PNGs)
    assets.rs    — embedded icon SVGs served to gpui-component
```

The agent worker runs on tokio in both front ends; the TUI drives it from
its own tokio main loop, while the GUI creates a runtime beside gpui's
executor and pumps the event channel from a gpui task.
