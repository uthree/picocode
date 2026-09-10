# Architecture

A workspace: everything UI-independent lives in `picocode-core`, and each
front end is a crate on top of it. Front ends talk to the core exclusively
through the `AgentEvent` / `WorkerCmd` channels and the plain data types in
`transcript` — nothing in the core depends on a rendering library.

```
crates/
  picocode-core/src/     — the agent engine (library)
    config/      — CLI args and config file (mod), permission modes and
                   approval rules (rules), web-search settings (search),
                   the project-config trust gate (trust), the `/config`
                   row table both front ends render (settings) and the
                   overlay of those rows they persist (saved)
    agent.rs     — rig agent construction and the streaming worker, plus
                   the `/goal` loop (a tool-less judge decides after each
                   turn whether to run another one)
    approval.rs  — approval gate for destructive tools (rig AgentHook);
                   in auto mode a tool-less reviewer agent answers the
                   prompts instead of the user, with the always-ask
                   commands and a fall back to asking as guard rails
    command.rs   — shared slash-command parser and command list (both
                   front ends execute the same parsed Command)
    attachment.rs — user file attachments (image/audio/PDF) sent as
                   multimodal message content, with per-provider support
                   checks
    backend/     — file+shell backend the tools run through: local, or a
                   remote host over SSH (--remote); ssh.rs drives the
                   system ssh binary through a ControlMaster socket
    clipboard.rs — system-clipboard reading for paste (files/images →
                   attachments), shared by both front ends
    context.rs   — estimated context-window composition (per-kind token
                   breakdown) backing the colored /status detail
    event.rs     — AgentEvent / WorkerCmd: the core ⇄ front-end protocol
    history.rs   — context savings: old-tool-output pruning and the
                   keep-recent-turns boundary used by compaction
    keys.rs      — the configurable send key (submit_key): which Enter
                   combination sends and which ones insert a newline
    mcp.rs       — opt-in MCP client: [[mcp_servers]] connect once at
                   startup, their tools join every spawned agent
    media.rs     — what the attached media costs in tokens: measured by
                   the provider where one will answer, computed from the
                   image's pixels where none will
    models.rs    — provider model-list queries and the /model switch
                   resolution shared by both front ends
    report.rs    — /status and /permissions text shared by both front ends
    sandbox.rs   — opt-in OS sandbox for model-initiated bash (macOS
                   sandbox-exec / Linux Landlock)
    session.rs   — session autosave/load backing /resume
    state.rs     — per-project persisted state (last-used model)
    steer.rs     — mid-turn steering queue + rig hook (injects user
                   messages at tool-call boundaries)
    tools/       — built-in tool implementations
                   (delegate.rs starts children and collects their reports;
                   delegate/tasks.rs owns their per-turn task registry)
    transcript.rs — renderer-agnostic transcript entries (Entry/EntryKind)
                   and diff-line parsing shared by both front ends
    undo.rs      — per-turn journal of pre-edit file states backing /undo
    workspace.rs — opening/switching the workspace (config + backend):
                   startup connect and the /remote switch share it
  picocode-tui/src/      — the ratatui front end (binary `picocode`)
    main.rs      — entry point (+ --smoke headless debug mode)
    app/         — application state and event loop (mod), split by concern:
                   agent events (events), input editing / completion /
                   attachments (editor), dialog state + /config + /status
                   (dialogs), model switching (models), system prompt
                   (prompt), /resume + autosave (sessions), /remote
                   (workspace)
    ui.rs        — ratatui rendering (transcript / input / status bar / dialogs)
    input.rs     — input thread, paste detection, input-box cursor math
    history.rs   — shell-style ↑/↓ input history
    highlight.rs — syntax highlighting (syntect) and line diffs (similar)
    markdown.rs  — markdown renderer for assistant replies (pulldown-cmark)
  picocode-gui/src/      — experimental gpui front end (binary `picocode-gui`)
    main.rs      — window bootstrap, tokio ⇄ gpui bridge (+ --smoke auto-prompt)
    chat/        — the chat view: state and command dispatch (mod), split
                   like the TUI's app/ — agent events (events), completion
                   and attachments (input), model switching (models),
                   system prompt (prompt), /resume + autosave (sessions),
                   /remote (workspace) — plus the rendering: entries
                   (transcript), dialogs and popups (dialogs), status bar
                   and its menus (status)
    highlight.rs — tree-sitter syntax highlighting for diffs (cached)
    math.rs      — TeX span extraction and Unicode fallback (unicodeit)
    tex.rs       — display-math typesetting via RaTeX (cached PNGs)
    assets.rs    — embedded icon SVGs served to gpui-component
```

The agent worker runs on tokio in both front ends; the TUI drives it from
its own tokio main loop, while the GUI creates a runtime beside gpui's
executor and pumps the event channel from a gpui task.

## Subagents

Subagent tools are opt-in: `subagents = true` in the config registers both
tools and includes their default workflow instructions. The default is
`false`. An explicit `disable_tools = ["delegate_task"]` also removes both
tools, even when the opt-in is set.

The `delegate_task` tool starts a Tokio task with a fresh child conversation
and immediately returns its ID. The parent can keep working and collect
reports through `agent_result`, which can wait or poll. See
[subagents.md](subagents.md) for usage. Parent and child use the same
completion model, request settings, workspace backend, permission handles,
MCP connections, background jobs and undo journal. Each child uses the
common project tools; coordination tools are registered on the parent.

Each agent has independent read stamps. A shared file-access lock keeps
`read_file` read-and-stamp operations and `edit_file` read-modify-write
operations together. Shared write revisions detect stale edits even when
filesystem timestamps match. The undo journal tracks the last write
separately from each agent's last read. Model requests and shell commands
can run concurrently.

The worker passes the human's request through Rig's runtime tool extensions
so the child's approval reviewer uses the original intent. Each child tool
call passes through `ApprovalHook`. A shared approval gate serializes
interactive dialogs from the parent and children, including plan approval.

`SubagentScope` owns the children for the entire parent turn. Before ending
the turn, the worker collects any remaining reports and resumes the parent
to integrate them, retaining the original human intent for approval
reviews. Cancellation aborts and joins children before emitting the turn's
completion events; dropping the scope also aborts outstanding tasks.
Children therefore share the parent's undo frame across automatic report
continuations. Child lifecycle, tool activity and usage events carry IDs;
child usage never replaces the parent's context meter. Only final reports
join the parent's model history.
