# Subagents

Subagents are disabled by default. To opt in, add this top-level key to
the project's `picocode.toml` (before any table headers), then restart
picocode:

```toml
subagents = true
```

The same key in `~/.config/picocode/config.toml` enables them across
projects. A project's value overrides the global setting. Set
`subagents = false` to turn them off. When off, both delegation tools
and their default workflow instructions are omitted from model requests.

Ask picocode to delegate independent pieces of work, for example:

> Have two subagents review session loading and configuration in parallel,
> then combine their findings.

The agent calls `delegate_task` with a self-contained task. Each call starts
a child in the background and immediately returns its ID:

```json
{
  "task": "Inspect crates/picocode-core/src/session.rs for error-handling bugs. Read related code as needed. Return findings with file paths and evidence; leave files unchanged."
}
```

```json
{"agent_id": 1, "status": "running"}
```

The parent can immediately start another child with a different task or do
its own work. Up to four children run concurrently alongside the parent.
To get a child's final report, the parent calls `agent_result`:

```json
{"agent_id": 1}
```

This waits for completion by default. Passing `"wait": false` returns the
current status immediately. A completed result looks like:

```json
{"agent_id": 1, "status": "completed", "output": "Review findings..."}
```

Failures return `"status": "failed"` with an `error` message. IDs belong to
the current parent turn, and results can be read again during that turn.
The parent should collect every report before its final answer. If it
answers with children still uncollected, picocode waits for those children
and resumes the parent to integrate their reports before ending the turn.

Each child starts a fresh conversation with the human's current request
and the delegated task. Include relevant context, paths, constraints and
the desired output in the task. The parent receives the final report;
the child's intermediate tool results stay in its own conversation.

The child uses the parent's model, reasoning effort, reply limit, working
directory, project instructions, project tools and permission rules. Edits
affect the same project files, so assign separate files to tasks that edit
them. `edit_file` detects another agent's changes and asks the editing
agent to re-read the file before overwriting it. Shell commands and external
tools still need their tasks coordinated around the shared workspace.

Child `edit_file` changes belong to the parent's
turn, so `/undo` reverts them together with that turn's other file edits.
Plan mode blocks child file edits and shell commands. Child calls that
require confirmation use the usual approval dialog, labeled with the
child's ID. Dialogs are shown one at a time; other agents can continue
work that does not need a dialog.

Child tool activity and token usage appear in the TUI, GUI and headless
logs with the child's ID and completion status. The parent's context meter
continues to describe the parent's own conversation. Esc or Stop cancels
all active children along with the parent turn. Changes already made stay
available for `/undo`.

Each parent turn can start up to 32 children in total, with up to four
running at once. Each child run is limited to 32 model turns. Long final
reports are truncated to about 24 KB with a truncation marker. Model
requests are submitted concurrently; actual inference throughput depends
on the provider's capacity and concurrency limits.

An explicit `disable_tools` entry takes precedence over `subagents = true`.
Adding `delegate_task` removes both `delegate_task` and `agent_result`:

```toml
disable_tools = ["delegate_task"]
```
