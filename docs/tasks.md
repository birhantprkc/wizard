# Background tasks

Long-running commands (dev servers, builds, watchers) don't have to block the agent loop. The `execute` tool takes an optional `run_in_background` flag:

```json
{ "command": "cargo build --release", "run_in_background": true }
```

The command is spawned detached and registered as a background task; the call returns immediately with `Background task #N started: <command>`. The agent keeps working while the task runs.

## Lifecycle

- Each task captures combined stdout/stderr into a per-task buffer capped at ~200 KB — when output exceeds the cap, only the most recent tail is kept
- Background tasks are killed after **30 minutes**; the status reflects the timeout
- At the top of every agent step (and every `--continuous` cycle), finished tasks are reported to the model exactly once, as a history note like:

  ```
  [background task #3 finished (exit 0)] cargo build --release
  <last ~2 KB of output>
  ```

- The TUI and headless surfaces print a one-line notice when a task finishes
- All still-running tasks are killed when the agent shuts down

The spawn still flows through the regular tool dispatch pipeline, so `pre_tool_use`/`post_tool_use` hooks apply to it like any other `execute` call.

## Managing tasks

Two companion tools:

| Tool | Arguments | Does |
|------|-----------|------|
| `task_output` | `id`, `tail_bytes` (optional, default 20 000) | Return the task's status and the tail of its buffered output. Read-only — works in plan mode. |
| `task_kill` | `id` | Terminate a running task. |

Statuses: `running`, `exit <code>`, `killed`, `timed out`.
