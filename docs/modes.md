# Personality modes

Wizard ships with two personalities that share the same tools and model but differ in autonomy, interaction style, and sampling temperature.

## Genie mode (default)

```bash
wizard
wizard --mode genie
```

Genie is the interactive, conversational mode. It's eager and creative ("your wish is my command", hence the name) and acts without asking: it bypasses permissions and executes file writes, shell commands, git commits, and evolutions directly, narrating briefly as it goes.

### Behavior

- Full Ratatui interface with chat history and tool output panels
- Executes all tool calls directly; there is no per-action y/n prompt
- Temperature: 0.8 (more creative responses)
- Loop limit: none by default (`max_steps = 0`). A turn runs until the model stops calling tools; Esc interrupts it
- Best for: collaboration, exploration, incremental changes

### Flags

| Flag | Effect |
|------|--------|
| `-p "task"` | Pre-fill the first message |
| `--resume` | Continue the last session |

### Example session

```
wizard
> Review src/auth.rs for security issues

[Wizard reads auth.rs, runs grep for hardcoded secrets, shows findings]

> Fix the issues you found

[Wizard applies patches directly → runs cargo test → reports results]
```

## Sovereign mode

```bash
wizard --mode sovereign -p "implement rate limiting on all API routes"
```

Sovereign mode is the autonomous, proactive agent. It runs headless for a single task with minimal human intervention and keeps working until that task is done or limits are hit. It does **not** keep going after "done"; that is continuous mode (below).

### Behavior

- Runs headless (no TUI). On a terminal, a busy spinner (same configurable verbs as the TUI's `[ui] spinner_verbs`) shows while the model thinks or a tool runs
- Auto-approves all tool calls (no per-action y/n)
- Temperature: 0.6 (tighter tool-call formatting)
- Loop limit: none by default; a `max_steps` capped below 100 is raised to 100, since nobody is at the prompt to say "continue"
- Circuit breaker: stops after 3 consecutive identical failures
- Best for: long-running refactors, test suites, multi-file features, CI/scripted runs

### Flags

| Flag | Effect |
|------|--------|
| `--max-hours 2` | Time limit for the run |
| `--loop 10` | Max outer loop iterations |
| `--continuous` | Run perpetually, never stopping at "done" (implies sovereign). See below |
| `--cwd /path/to/repo` | Set project root |

### Control file

During a long sovereign-mode run, write to `.wizard/loop-control` in the project:

| Value | Effect |
|-------|--------|
| `stop` | Graceful shutdown after current step |
| `pause` | Wait until file is removed or set to `resume` |
| `skip` | Skip the current sub-task |

### Example

```bash
wizard --mode sovereign \
  -p "add comprehensive tests for the payment module" \
  --max-hours 1 \
  --cwd ~/projects/myapp
```

## Continuous mode (perpetual sovereign)

```bash
wizard --continuous -p "keep hardening this codebase: tests, docs, performance"
```

`--continuous` turns sovereign mode into a perpetual, self-directing agent. Given an
initial goal, it doesn't stop when a sub-task completes: it records the cycle,
re-examines the project, and picks the next most valuable action. Once the mission is
done, it moves on to high-value improvements (tests, docs, hardening) or extends its own
capabilities via the `evolve` tool. There's no human in the loop, so the automated rails
below keep it safe.

### What makes it run forever

- **Durable mission.** The goal is persisted to `<project>/.wizard/mission.toml` along
  with a cycle count and rolling progress log. It survives restarts and binary
  self-replacement: relaunch with `--continuous` (no `-p`) and it resumes the mission.
- **Sleep-and-wake.** Transient provider failures (server unreachable, busy, `429`/`5xx`,
  dropped stream) do not abort the run. The loop backs off exponentially
  (`retry_base_secs` → `retry_max_secs`) and retries indefinitely, so a paused or
  restarting model server is waited out.
- **Context compaction.** When the conversation grows past `compact_threshold_bytes`,
  older history is summarized into a compact progress note so a run can continue
  indefinitely without overflowing the model's context window. The agent is also
  taught to compact deliberately (and to save durable facts with `memory` when the
  task changes); every turn is already on disk as session JSONL under
  `~/.wizard/sessions/`. See [Agent-managed context](usage.md#agent-managed-context).
- **Self-evolution + re-exec.** When the agent calls `evolve` (adding a skill, MCP
  server, scripted tool, or subagent, or rebuilding its own binary with `deep`), the
  loop saves the mission, re-execs into the freshly built image to load the new
  capabilities, and resumes the mission.

### Stopping it

The same `.wizard/loop-control` file is the kill switch: write `stop` for a graceful
shutdown after the current step, or `pause` to hold. `--max-hours` and the circuit
breaker (3 identical failures in a row) also terminate a continuous run. Deep
self-modification remains gated by an automated `cargo build` + `--version` smoke test,
with the previous binary kept as `wizard.prev` for one-`mv` rollback and every evolution
appended to `~/.wizard/evolution.jsonl`.

### Tuning (`~/.wizard/config.toml`)

| Key | Default | Effect |
|-----|---------|--------|
| `continuous` | `false` | Start in perpetual mode without the flag |
| `retry_base_secs` | `5` | Base backoff when the model server is unavailable |
| `retry_max_secs` | `300` | Cap on backoff between retries |
| `cycle_pause_secs` | `0` | Pause between continuous cycles |
| `compact_threshold_bytes` | `48000` | History size that triggers compaction |
| `rollback_failed_cycles` | `false` | Restore a failed cycle's file checkpoints (see [checkpoints.md](checkpoints.md)) |

> **Run it in a container or VM.** Continuous mode executes every tool call with no
> human in the loop and can rewrite its own binary. Point it only at work you're willing
> to let it touch unattended, and read [SECURITY.md](../SECURITY.md) first.

## Plan mode

Plan mode is an overlay that works in every mode (genie, sovereign, continuous, gateway): the agent first investigates with read-only tools, presents a plan, and only executes once the plan is approved.

While plan mode is on:

- Only read-only tools run (`read_file`, `list_files`, `search_files`, `git_status`, `git_diff`, ...). Every other tool, including `execute`, file writes, scripted/MCP tools, and `spawn_subagent`, returns a "blocked by plan mode" error to the model. These blocks are fed back as ordinary tool errors (not fatal) and are exempt from the circuit breakers.
- The one way out is the `exit_plan` tool: the model calls it with the finished plan as markdown. The plan is saved to `<project>/.wizard/plan.md` and presented for a verdict.
- Approval ends plan mode and the model executes the plan in the same turn. Rejection (with optional feedback) keeps plan mode on; the feedback is fed back so the model can revise and call `exit_plan` again.
- Before finishing, the model can call the read-only `interview` tool to ask a short batch of clarifying questions (see below) when answers would change the plan.

### Interview

When the agent has explored enough to understand the shape of the task but still has genuine open questions whose answers would change the plan (scope, trade-offs, ambiguous intent), it calls the `interview` tool with a short batch of questions, each optionally offering suggested answers. In the TUI an interview modal opens: type a free-text answer, or press `1`–`9` to fill in a suggested option (then edit or accept it); `Enter` commits the current answer and advances; `Esc` dismisses the whole interview. The answers are fed back to the model, which folds them into the plan before calling `exit_plan`. Headless runs, the gateway, and the fleet have no interactive user, so the interview is declined automatically and the model proceeds on its best judgment. The tool is read-only, so it works mid-plan without tripping the gate.

### Omakase (chef's choice)

Omakase is the chef's-choice flavor of plan mode: it goes beyond a simple review gate. The agent still explores read-only, but then it **decides the approach itself and auto-approves its own plan**, with no interview and no human review. It's for when you want the result, not the deliberation. The plan it commits to is written to `<project>/.wizard/plan.md` and surfaced (in the TUI as a "chef's choice" card; headless prints it) before execution begins, so the chosen approach is never a black box. Because the agent is told there's no review gate, its `exit_plan` plan is self-justifying: it states the approach picked, the alternatives weighed, and the assumptions made. The `interview` tool declines to ask in omakase; the chef decides.

```
/omakase       # toggle omakase mode (implies plan mode)
```

### TUI (genie)

```
/plan          # toggle plan mode (Shift+Tab does the same)
/omakase       # toggle omakase (chef's-choice) mode
```

The status bar shows `PLAN` while plan mode is active, or `OMAKASE` in omakase mode. When the model presents a plan for review (non-omakase), a review modal opens: `y`/Enter approves, `n` opens a feedback line (type the reason, Enter sends the rejection, Esc goes back), ↑/↓ scroll the plan.

### Headless (sovereign / continuous / gateway)

```bash
wizard --mode sovereign --plan -p "refactor the config loader"
wizard --omakase -p "ship the smallest fix that passes the suite"
```

| Knob | Effect |
|------|--------|
| `--plan` (flag) | This run starts in plan mode |
| `--omakase` (flag) | Sets `omakase` + `plan_first` in config. The GUI applies chef's-choice on the agent; headless, continuous, and the gateway currently turn on plan mode from `plan_first` and still auto-approve `exit_plan` (the usual unattended plan-then-execute path). Full chef's-choice prompting (no interview, self-justifying plan) is live in the TUI, the GUI, and via the gateway's `/omakase` chat toggle |
| `plan_first = true` (config) | Every session starts in plan mode |
| `omakase = true` (config) | Implies `plan_first`. Applied as chef's-choice on TUI/GUI agent construction; headless/gateway use the plan_first path above unless `/omakase` is toggled on the gateway |
| `plan_each_cycle = true` (config) | Continuous mode re-enters plan mode at the top of every cycle |

With no human in the loop, `exit_plan` is auto-approved on the plain plan path: the plan is printed (or, on the gateway, included in the chat reply), approval is sent automatically, and the same turn proceeds to execute. Omakase, where it is fully applied, makes the agent decide for itself (no interview, plan written to `.wizard/plan.md` and surfaced before execution). The gateway also accepts `/plan` and `/omakase` chat messages to toggle these modes for subsequent messages.

The last presented plan is always available at `<project>/.wizard/plan.md`.

## Switching modes in the TUI

```
/mode sovereign    # switch to autonomous behavior (still in TUI)
/mode genie        # switch back to interactive (Ratatui TUI) mode
/sovereign         # shorthand for /mode sovereign
/genie             # shorthand for /mode genie
```

Mode changes affect prompting, interaction style, and, if `max_steps` is capped, the step budget. Switching writes the new mode to `~/.wizard/config.toml`, so it survives a restart.

## System prompts

Each mode injects a different system prompt:

**Genie** emphasizes collaboration, explanation, and narrating actions as it goes.

**Sovereign** emphasizes autonomy, completing the full task end-to-end, running tests, and committing when appropriate.

Both prompts include loaded skills from the `skills/` directory and any project-level `AGENTS.md`.

## Choosing a mode

| Situation | Recommended mode |
|-----------|-----------------|
| Exploring unfamiliar code | Genie |
| Quick one-off fix | Genie |
| Large multi-file refactor | Sovereign |
| CI/automation/scripted runs | Sovereign |
| Learning what the agent will do | Genie |
| Overnight autonomous work | Continuous (`--continuous`) |
