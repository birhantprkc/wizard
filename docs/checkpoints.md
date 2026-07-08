# Checkpoints and /rewind

Wizard snapshots every file it is about to edit, per turn, so a turn (or a run of
turns) can be rewound: files restored to their before-state and the conversation
truncated to match. There are no approval prompts anywhere in this: every tool call
still executes directly; checkpoints are the undo, not a gate.

## How it works

Before an edit-class tool (`write_file`, `edit_file`) runs, and after pre-tool hooks have
had their chance to rewrite the arguments, the dispatcher copies the target file's
current content into the project's checkpoint store. Subagent edits go through the same
seam, snapshotted under the parent's current turn. The first snapshot of a path within a
turn wins: that is the turn's before-state, no matter how many times the turn rewrites
the file. A tool about to create a new file records that instead, so a rewind deletes
the file.

Snapshotting is best-effort by design: a checkpoint failure is logged and the tool call
proceeds. The agent can always work; it just might not be rewindable.

### Why not shadow-git

The repo Wizard runs in may be edited concurrently by other processes (other agents,
editors, builds). A git-based snapshot would capture and restore *their* changes too.
Copy-on-write snapshots only ever touch files Wizard itself modified.

## On disk

```
<project>/.wizard/checkpoints/
  index.jsonl        one record per snapshot: {turn, tool, path, snap, existed_before}
  <turn>/<n>.snap    copied file contents
```

Turn ids are monotonic per project (they continue across sessions). Corrupt index lines
are skipped on load. Like the rest of `.wizard/` (plan.md, mission.toml), the directory
is plain files you can inspect or delete.

## /rewind (TUI)

```
/rewind          # open the turn picker
/rewind 12       # rewind directly to before turn 12
```

The picker lists recent turns newest first, each with its turn number, the first line of
the prompt that started it, and the files its edits touched. ↑/↓ select, Enter rewinds,
Esc cancels.

Rewinding to turn N:

- restores every snapshot from turn N onward (the earliest before-state of each file
  wins), and files that did not exist before are deleted;
- truncates the session file at turn N's marker and reloads the in-memory conversation,
  so the model no longer remembers the rewound turns;
- prunes the rewound turns from the checkpoint store.

Session files carry one turn-marker line per turn for this; older session files without
markers still load (they just cannot anchor a conversation truncation).

## Perpetual rollback

```toml
# ~/.wizard/config.toml
rollback_failed_cycles = true   # default false
```

In continuous mode, when a cycle ends in a circuit breaker or a hard error, the cycle's
file edits are restored before the run ends, and the rollback is noted in the mission's
progress log (`.wizard/mission.toml`). Off by default: a failed cycle's partial work is
sometimes worth keeping.

## Retention

```toml
[checkpoints]
keep_turns = 50   # default
```

Snapshots of all but the most recent `keep_turns` turns are garbage-collected once at
session start.
