# Personality modes

Wizard ships with two personalities that share the same tools and model but differ in autonomy, prompting, and confirmation behavior.

## Genie mode (default)

```bash
wizard
wizard --mode genie
```

Genie is the interactive, conversational mode. It is eager and creative — "your wish is my command" — but asks before doing anything risky.

### Behavior

- Full Ratatui interface with chat history and tool output panels
- Confirms before: file writes, shell commands, git commits (unless `--auto`)
- Temperature: 0.8 (more creative responses)
- Default loop limit: 25 agent steps per turn
- Best for: pair programming, exploration, incremental changes

### Flags

| Flag | Effect |
|------|--------|
| `--auto` | Skip confirmation prompts (still interactive TUI) |
| `-p "task"` | Pre-fill the first message |
| `--resume` | Continue the last session |

### Example session

```
wizard
> Review src/auth.rs for security issues

[Wizard reads auth.rs, runs grep for hardcoded secrets, shows findings]

> Fix the issues you found

[Wizard proposes patches → asks "Apply changes? [y/n]" → runs cargo test]
```

## Wizard mode (sovereign)

```bash
wizard --mode wizard -p "implement rate limiting on all API routes"
```

Wizard mode is the autonomous, proactive agent. It runs with minimal human intervention and keeps working until the task is done or limits are hit.

### Behavior

- Can run headless (no TUI) or with a minimal status display
- Auto-approves all tool calls
- Temperature: 0.6 (tighter tool-call formatting)
- Default loop limit: 100 steps
- Circuit breaker: stops after 3 consecutive identical failures
- Best for: long-running refactors, test suites, multi-file features

### Flags

| Flag | Effect |
|------|--------|
| `--max-hours 2` | Time limit for the run |
| `--loop 10` | Max outer loop iterations |
| `--auto` | Implicit in wizard mode; included for consistency |
| `--cwd /path/to/repo` | Set project root |

### Control file

During a long wizard-mode run, write to `.wizard/loop-control` in the project:

| Value | Effect |
|-------|--------|
| `stop` | Graceful shutdown after current step |
| `pause` | Wait until file is removed or set to `resume` |
| `skip` | Skip the current sub-task |

### Example

```bash
wizard --mode wizard \
  -p "add comprehensive tests for the payment module" \
  --max-hours 1 \
  --cwd ~/projects/myapp
```

## Switching modes in the TUI

```
/mode wizard    # switch to sovereign behavior (still in TUI)
/mode genie     # switch back to interactive confirmations
```

Mode changes affect prompting and auto-approve behavior for the current session. The choice is not persisted unless you update `~/.wizard/config.toml`.

## System prompts

Each mode injects a different system prompt:

**Genie** emphasizes collaboration, explanation, and asking before destructive actions.

**Wizard** emphasizes autonomy, completing the full task end-to-end, running tests, and committing when appropriate.

Both prompts include loaded skills from the `skills/` directory and any project-level `AGENTS.md`.

## Choosing a mode

| Situation | Recommended mode |
|-----------|-----------------|
| Exploring unfamiliar code | Genie |
| Quick one-off fix | Genie |
| Large multi-file refactor | Wizard |
| CI/automation/scripted runs | Wizard |
| Learning what the agent will do | Genie |
| Overnight autonomous work | Wizard |