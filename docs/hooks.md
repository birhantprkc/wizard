# Lifecycle hooks

Hooks are shell commands that Wizard runs at fixed points in the agent's lifecycle: before and after every tool call, when a prompt is submitted, and at session and turn boundaries. They fire in **every mode** (genie TUI, sovereign headless, perpetual `--continuous`, and the gateway) and apply to subagent tool calls too. Use them to enforce policy (block dangerous commands), inject context (project status, time of day), or log activity.

There is no permission prompting in Wizard. Hooks are the programmable seam where you put guardrails instead.

## Declaring hooks

Hooks live in TOML files, loaded when an agent is built:

- `~/.wizard/hooks.toml`: global, applies everywhere
- `<project>/.wizard/hooks.toml`: per project, appended after the global hooks

```toml
[[hooks]]
event = "pre_tool_use"           # which lifecycle event (see below)
matcher = "execute"              # optional glob over the tool name
command = "/path/to/script.sh"   # run via `sh -c` in the project root
timeout_secs = 30                # optional; default 60
```

`matcher` is a glob (`"execute"`, `"git_*"`, `"*file*"`) over the tool name and only applies to tool events; other events ignore it. Omit it to match every tool. Hooks for the same event run sequentially in declaration order, global first, then project.

A missing file means no hooks: default behavior is unchanged. An invalid file or matcher is skipped with a logged warning; it never prevents startup.

## Events

| Event | When | What the hook can do |
|-------|------|----------------------|
| `pre_tool_use` | Before a tool call executes | Rewrite the arguments, or block the call |
| `post_tool_use` | After a tool call executes | Append context to the tool result |
| `user_prompt_submit` | When a message starts a turn | Block the turn, or append context to the message |
| `session_start` | Once when a session begins | Append system context for the whole session |
| `session_end` | Once when a session ends | Observe only |
| `turn_end` | After every turn finishes | Observe only |

"Session" means: TUI launch to quit, one headless run (including all continuous cycles), or the gateway's whole serve lifetime.

## Payload

Each hook receives one JSON object on stdin:

```json
{
  "event": "pre_tool_use",
  "tool_name": "execute",
  "args": {"command": "cargo test"},
  "cwd": "/path/to/project",
  "session_id": "2026-06-11T09-30-00",
  "mode": "genie"
}
```

`tool_name` and `args` are `null` for the non-tool events. Two events carry extra fields:

- `user_prompt_submit`: `prompt`, the text of the user message starting the turn.
- `post_tool_use`: `tool_output`, the text the tool returned (truncated to 32 KB), and `is_error`, `true` when the tool reported failure.

A hook that doesn't care about the payload can just skip reading stdin.

## Exit-code semantics

- **Exit 0: continue.**
  - `pre_tool_use`: if stdout parses as JSON with `{"updated_args": {...}}`, the tool runs with those arguments instead (later hooks in the chain see the rewritten args). Any other stdout is ignored.
  - `post_tool_use`: non-empty stdout is appended to the tool result the model sees.
  - `user_prompt_submit` / `session_start`: non-empty stdout is appended to the user message / the session's system context.
  - `session_end` / `turn_end`: stdout is ignored.
- **Exit 2: block.** stderr is the reason.
  - `pre_tool_use`: the tool doesn't run; the model gets `blocked by pre_tool_use hook: <reason>` as an ordinary tool error and can adjust course. Repeated blocked calls count toward the same failure breakers as any other tool failure.
  - `user_prompt_submit`: the turn ends immediately with a notice; the prompt never reaches the model.
  - Other events can't block; exit 2 there is treated like any other failure.
- **Anything else: ignored.** A different exit code, a timeout (`timeout_secs`, default 60, the process is killed), or a spawn failure surfaces as a warning and the pipeline continues. Hooks must never wedge the agent.

Hook activity that changes something (a rewrite, appended context, a block, a warning) shows up as a dim log line in the TUI, or a printed line headless. Silent successes stay silent.

## Examples

Block shell commands that mention `rm -rf`:

```toml
[[hooks]]
event = "pre_tool_use"
matcher = "execute"
command = "jq -e '.args.command | test(\"rm -rf\") | not' > /dev/null || { echo 'rm -rf is not allowed' >&2; exit 2; }"
```

Append a note to every failed shell command's result:

```toml
[[hooks]]
event = "post_tool_use"
matcher = "execute"
command = "jq -r 'if .is_error then \"check the error above before retrying\" else empty end'"
```

Remind the model of the branch policy on every prompt:

```toml
[[hooks]]
event = "user_prompt_submit"
command = "echo \"current branch: $(git branch --show-current). Never commit to main directly.\""
```

Log every completed turn:

```toml
[[hooks]]
event = "turn_end"
command = "date >> ~/.wizard/logs/turns.log"
timeout_secs = 5
```
