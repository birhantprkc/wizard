# Lifecycle hooks

Hooks are shell commands that Wizard runs at fixed points in the agent's lifecycle: before and after every tool call, when a prompt is submitted, and at session and turn boundaries. They fire in **every mode** (genie TUI, sovereign headless, perpetual `--continuous`, and the gateway) and apply to subagent tool calls too. Use them to enforce policy (block dangerous commands), inject context (project status, time of day), or log activity.

There is no permission prompting for the model's tool calls in Wizard. Hooks are the programmable seam where you put guardrails instead. There *is* one prompt about hooks themselves: a project's own hooks file needs a trust decision before it loads, see [Project trust](#project-trust).

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

**Project hooks do not run until you trust the project.** `~/.wizard/hooks.toml` is yours by construction and always loads; `<project>/.wizard/hooks.toml` arrives with a `git clone` and is gated. See [Project trust](#project-trust) below.

## Project trust

A hook is a shell command, and `session_start` fires before the model has said a word, so a repository that ships `.wizard/hooks.toml` is shipping code that would run the moment you launch Wizard in its directory. Cloning is not consent, so there is a gate.

A project with no `.wizard/hooks.toml` is never gated and never mentioned: there is nothing executable to decide about, and that case has to stay silent. A project that has one contributes no hooks at all until there is a recorded yes for exactly that file.

### The decision

There is one decision per project, and it is recorded in `~/.wizard/trusted_projects`: one JSON object per line, written atomically at mode 0600 (this file decides what may execute, so no other user gets to add a line to it).

- **The project root is canonicalised** before it is used as the key, so a symlink or a `..`-dressed path cannot ride another project's approval.
- **The hooks file is fingerprinted** (sha256 over its relative path, its length, and its bytes). Editing it, replacing it, or adding one after the fact re-opens the question: the old yes covers the old content only. A `git pull` that touches `.wizard/hooks.toml` therefore needs a fresh yes.
- **Both answers are recorded.** A no is a decision, not a deferral, so it is not asked again either.
- **A recorded no outranks everything**, including `WIZARD_TRUST_PROJECT`. To be asked again, delete that project's line from `~/.wizard/trusted_projects`.

### When Wizard asks, and when it just refuses

Two things have to be true before the question is put to you.

**A surface has to declare that the terminal is safe to block on.** That is a claim the calling code makes, not something inferred from `isatty`, because `isatty` answers the wrong question. Under the TUI there *is* a tty on fd 0 and prompting is still catastrophic: crossterm holds that same descriptor in raw mode behind the alternate screen, so the question would be painted invisibly over the frame, the keystroke answering it would be taken by the event stream, and the blocking read would park the very thread running the event loop. A foreground `wizard gui` passes the same probe and would block a window-driven task on a question painted into a terminal nobody is watching.

**And the terminal facts have to agree**: a tty on fd 0 *and* fd 1, with this process in the terminal's foreground process group. So `echo hi | wizard -p "…"` refuses rather than blocking on a pipe, and a backgrounded process (which would earn a SIGTTIN for reading stdin and stop) never gets that far.

Exactly two surfaces make the declaration, and both do it in the same place: once, up front, before anything has taken the terminal over.

| Surface | Can ask? | Where the refusal shows up |
|---------|----------|----------------------------|
| Genie TUI (`wizard`) | **Yes**, before raw mode and the alternate screen are set up | As a notice in the transcript, on the first draw |
| Headless (`wizard -p "…"`, `--mode sovereign`, `--continuous`) with the default `--output-format text` | **Yes**, before the spinner starts | `wizard: <reason>` on stderr |
| The same headless run under any other `--output-format` | No: stdout is a machine-readable stream and a prompt would be the first thing in it | The log |
| Gateway (`wizard --gateway`) | No | Printed to stdout at startup and logged, so it lands in `journalctl` next to the refusals |
| The window (`wizard gui`) | No | A notice in that chat's own event stream, plus the log |
| Editor embedding (`wizard acp`) | No: stdout is JSON-RPC to the editor | The log |
| Fleet (`wizard fleet`), both the planner and the workers | No | The log |
| Scheduler daemon jobs | No: the child is spawned with stdin on `/dev/null` | The job's log under `~/.wizard/logs/jobs/` |
| CI, systemd units, cron, anything piped or backgrounded | No | The log |
| Every mid-session agent rebuild (`/model`, a provider switch, `/fusion`, crash recovery) | No, on every surface | The log |

`wizard schedule run <name>` is the one scheduler case that inherits your terminal, so a job you start by hand can ask; the daemon that runs the same entry on its cron line cannot.

When it does ask, it names the file and takes one answer:

```
This project ships files that Wizard would run as shell commands:
  /home/you/src/theirrepo/.wizard/hooks.toml

There is no trust decision on record for /home/you/src/theirrepo.
Read those files first: a hook runs unsandboxed, with your privileges.
Trust this project and run its hooks? [y/N]
```

Anything but `y` / `yes` (including end of input) is a no. The file is never quoted back at you, only named: its contents are whatever its author wrote, and echoing them would hand a repository your terminal's escape sequences. Read it yourself.

The answer is recorded either way, and it is recorded once for the whole process: the rebuilds later in the session read it back and ask nothing. A `y` typed at a `wizard -p` run is as durable as one typed at the TUI.

**Everywhere the question cannot be put, the answer is no.** Those surfaces load no project hooks and say why:

```
not running project hooks (/srv/app/.wizard/hooks.toml): /srv/app has no trust decision
on record and there is no terminal to ask on. Start wizard once interactively in that
directory to decide, or set WIZARD_TRUST_PROJECT=1 for unattended runs.
```

Starting Wizard interactively in that directory is advice that works: the TUI is one of the two surfaces that asks, and one `y` there settles it for the gateway, the GUI, and CI afterwards.

Every refusal also goes to the log (`~/.wizard/logs/`, see [logging.md](logging.md); it is a warning, so the default filter records it). That is the per-rebuild trace, and it is deliberately the *only* place the rebuild path writes: the check re-runs on every agent rebuild, and printing there would put raw multi-line text on the TUI's alternate screen once per rebuild. Each surface says it once more where its own user is actually looking, per the table above. If a project's hooks are not firing and you expected them to, the log is where the reason is.

Nothing is recorded in the unattended case, so the next run that *can* ask still gets to decide.

### Trusting a project without the prompt

If you are never asked (an unattended run, or any surface that does not own the terminal outright), this is the way in.

**`WIZARD_TRUST_PROJECT=1`** (also `true` or `yes`; anything else, including `0`, is not an opt-in) trusts the project for that one process:

```bash
WIZARD_TRUST_PROJECT=1 wizard --mode sovereign -p "run the release checklist"
```

It answers an *open* question only. It cannot override a no you recorded, and it is never persisted, so it cannot leak a decision into a later interactive run. It is the right tool for a machine whose project hooks are your own: a CI job, a systemd unit running the gateway on a repo you control. Exporting it globally in a shell profile is not, since it then applies to every repository you happen to clone.

It also does not lift a refusal that an *edit* re-opened. Editing the hooks file normally puts the project back to "open" so a human can be re-asked, and the file belongs to whoever wrote the repository, so without this rule "append a blank line and push" would be a way around your no on every machine that exports the variable. A human at a terminal is a different matter: the content genuinely changed, so you are asked again, and a yes then stands.

Only project-supplied files go through this gate. The global `~/.wizard/hooks.toml` is not gated: it is yours, gating it would prompt in every directory on earth, and it would close no hole.

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

Appended context is reported **once per hook per session**, and every append after that goes to the log (`WIZARD_LOG=debug`) instead. A hook that injects context injects it every time it matches — that is what it is for — so a `post_tool_use` hook reporting each one costs a line per tool call, and a `session_start` one says the same sentence at every launch. The first notice is the useful one: text entering the model's context from outside the conversation is not otherwise visible anywhere. `/clear` starts a new session and resets the filter. Rewrites, blocks and warnings are never deduplicated — each describes something that varies per call, and swallowing the repeat would hide the occurrence that mattered.

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
