# Doctor and status

## `wizard doctor`

Diagnoses the environment and prints one line per check:

```
✓ config            /home/you/.wizard/config.toml parses
✓ provider local    llamacpp @ http://127.0.0.1:11435 (qwen3-30b) reachable
– provider openai   $OPENAI_API_KEY not set
✗ mcp playwright    spawn failed: No such file or directory (os error 2)
✓ native tools      18 tools registered
– hooks (global)    /home/you/.wizard/hooks.toml absent (no hooks)
✓ hooks (project)   2 hook(s) in .wizard/hooks.toml
✓ ~/.wizard         /home/you/.wizard writable
✓ project .wizard   .wizard writable
✓ sessions          /home/you/.wizard/sessions writable
✓ checkpoints       12 snapshot(s) across 4 turn(s)
```

Checks:

- **config**: `~/.wizard/config.toml` parses (missing file is fine: defaults apply)
- **provider \<name\>**: each configured LLM provider answers its health probe; skipped (`–`) when its API key env var is unset
- **mcp \<name\>**: each `[[server]]` in `~/.wizard/mcp.toml` spawns and completes the MCP handshake (with tool count)
- **native tools**: the compiled-in tool set is registered
- **platform**: host notes (Termux source-build expectations, NixOS flake preference, or plain OS/arch)
- **hooks**: global and project `hooks.toml` parse
- **writable**: `~/.wizard`, the project's `.wizard/`, and the sessions dir accept writes
- **checkpoints**: the snapshot index parses; stale snapshot directories are counted
- **gateway**: when configured, kind, token presence (never prints the secret), and whether a `wizard --gateway` process appears to be running; also flags a stored telegram token with `kind = "none"`

Every network probe is capped at 5 seconds, so doctor never hangs. Exit code: 0 when no check failed (`–` skips are not failures), 1 otherwise. Use it as a preflight in scripts:

```sh
wizard doctor && wizard --mode sovereign -p "task"
```

`/doctor` in the TUI runs the same battery and prints the report to the transcript.

## `/status`

A one-shot snapshot of the running session. The TUI prints something like:

```
model: qwen3-30b
provider: local (LlamaCpp @ http://127.0.0.1:11435)
mode: genie
effort: default
session: 2026-06-11T09-30-00
usage: 1200 prompt + 240 completion tokens
background tasks: 1 running
todos: 2/5 done
plan mode: off
ultra: off
```

The browser GUI adds a `context: N tokens` line (the size of the next model call, not a session lifetime sum) and a `steps:` line for the configured step budget. For a live next-call estimate in the TUI, use the status bar token readout instead of `/status`. Lifetime prompt/completion totals and cost estimates stay on `/cost`.
