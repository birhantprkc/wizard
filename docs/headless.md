# Headless output formats and exit codes

A sovereign run (`wizard --mode sovereign -p "task"`, or any `-p` run without a terminal) prints to stdout. `--output-format` selects how:

## `text` (default)

The human-readable stream you get today: assistant deltas as they arrive, dimmed reasoning, `→ tool` / `← tool [ok]` one-liners, a busy spinner on terminals, and a `[run finished: ...]` trailer with token totals.

A subagent's tool calls print inline under the subagent's name (`→ researcher ▸ web_fetch {…}` / `← researcher ▸ web_fetch [ok]`); the TUI puts them on its [subagent rail](usage.md#the-subagent-rail) instead. The structured formats below label them the same way in their `name` fields.

## `json`

Silent until the run ends, then exactly one JSON object on stdout:

```json
{
  "result": "the assistant's accumulated text",
  "reason": "completed",
  "turns": 1,
  "steps": 3,
  "usage": {"prompt_tokens": 1200, "completion_tokens": 240},
  "tool_calls": [{"name": "execute", "calls": 2, "errors": 0}],
  "errors": []
}
```

`reason` is one of `completed | max_steps | time_limit | stopped | circuit_breaker`.

## `stream-json`

One JSON object per line (JSONL) as events arrive, always terminated by a `done` line:

```
{"type":"tool_call","name":"execute","args":{"command":"cargo test"}}
{"type":"tool_result","name":"execute","is_error":false,"output":"..."}
{"type":"step","step":1}
{"type":"text_delta","text":"All tests pass."}
{"type":"usage","prompt_tokens":1200,"completion_tokens":240}
{"type":"turn_done","reason":"completed"}
{"type":"done","reason":"completed","usage":{"prompt_tokens":1200,"completion_tokens":240}}
```

Other line types you may see: `thinking_delta`, `todo`, `task_finished`, `plan` (auto-approved, with the plan text), `hook`, and `error`. `turn_done` closes each agent turn; the final `done` line carries the run outcome and total usage.

In both structured formats the spinner and decorative headers are suppressed: stdout is pure JSON, diagnostics go to stderr.

## Exit codes

The process exit code encodes why the run ended:

| Code | Meaning |
|------|---------|
| 0 | completed (or gracefully stopped via `.wizard/loop-control`) |
| 1 | hard error (config, provider unreachable, ...) |
| 2 | step budget exhausted (`max_steps`) |
| 3 | circuit breaker (repeated identical failures) |
| 4 | `--max-hours` time limit |

So a CI invocation can distinguish "the agent finished" from "the agent gave up":

```sh
wizard --mode sovereign -p "make the tests pass" --output-format json > result.json
case $? in
  0) echo ok ;;
  2) echo "ran out of steps" ;;
  *) echo "failed" ;;
esac
```

The TUI and the gateway are unaffected by `--output-format`; they always exit 0 on a clean quit.
