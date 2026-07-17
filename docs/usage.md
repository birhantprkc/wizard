# Usage

Day-to-day reference: the TUI's slash commands, the `wizard agents`
dashboard, the subagent rail, and the core mechanics (token usage, todos,
project instructions) that work identically in every mode (genie TUI,
sovereign headless, perpetual, gateway).

## Slash commands

Everything typed as `/command` in the TUI. Tab-completion lists these with
inline hints.

| Command | What it does |
|---------|--------------|
| `/help` | List the available commands |
| `/clear` | Clear the conversation |
| `/model [tag]` | Show the current model, or switch to `tag` |
| `/mode [genie\|sovereign]` | Show or switch personality mode (`/genie` and `/sovereign` are shortcuts) |
| `/effort [low\|medium\|high\|default]` | Set reasoning effort for models that support it (xAI Grok 4.x, OpenAI o-series/gpt-5); no argument opens the picker, `default` clears to the provider default |
| `/plan` | Toggle plan mode (also Shift+Tab): read-only investigation until a plan is approved |
| `/omakase` | Toggle omakase: chef's-choice plan mode, the agent decides and auto-approves its own plan ([modes.md](modes.md)) |
| `/evolve [--deep] <desc>` | Self-extend: add a skill, MCP server, scripted tool, or subagent; `--deep` rebuilds the binary ([evolve.md](evolve.md)) |
| `/reload` | Reload skills, scripted tools, and MCP servers without a restart |
| `/rewind [turn]` | Restore file checkpoints and truncate history; no argument opens the turn picker ([checkpoints.md](checkpoints.md)) |
| `/resume [id]` | Reopen a past session and continue it; no argument opens the session picker |
| `/compact` | Summarize older history into a progress note now, instead of waiting for the automatic threshold |
| `/agents` | Browse the subagent roster; Enter pre-fills a delegation request |
| `/subagents` | Focus the subagent rail below the composer (same as ↓); from inside a subagent's pane, back out to the chat |
| `/dashboard` | Toggle the machine-wide session manager, same view as `wizard agents` (below) |
| `/bashes` | List background tasks (`execute` with `run_in_background`), running and finished ([tasks.md](tasks.md)) |
| `/goal [text]` | Show or set the standing mission goal (drives sovereign/continuous mode; persists to `.wizard/mission.toml`) |
| `/diff` | Toggle the git diff sidebar |
| `/todos` | Toggle the todo list above the input |
| `/cost` | Session token usage, with cost estimates when per-provider rates are configured |
| `/memory [read\|forget <name>]` | List the saved project memories, show one, or forget one ([memory.md](memory.md)) |
| `/status` | Session status: model, provider, mode, session id, usage, todo progress, background tasks |
| `/doctor` | Environment diagnostics, same checks as `wizard doctor` ([doctor.md](doctor.md)) |
| `/provider …` | Add, remove, or switch LLM providers; no arguments opens the interactive menu |
| `/fusion [config]` | Toggle model fusion, or configure the panel ([fusion.md](fusion.md)) |
| `/ultra [config]` | Toggle mixture of agents, or configure the roster ([ultra.md](ultra.md)) |
| `/server [status\|start\|stop]` | Manage the local llama-server |
| `/login <provider>` | OAuth sign-in for providers that support it (currently `xai`) |
| `/publish [branch]` | Fork Wizard to your GitHub and get a one-line installer ([market.md](market.md)) |
| `/settings` | Open the in-app settings menu |
| `/vim` | Toggle modal (vim-style) editing of the input composer |
| `/quit` | Exit (`/q` and `/exit` work too) |

Your own commands (markdown files that expand into prompts) sit alongside
these; see [commands.md](commands.md), which also covers `@path` file
references.

### Agent-run slash commands

The agent can run these same commands itself with the native `run_command`
tool — it passes a command line exactly as you would type it (e.g.
`/effort high`, `/model claude-sonnet-5`, `/compact`, `/reload`). So the agent
can raise its own reasoning effort for a hard task, switch models, compact its
context, or reload skills without you stepping in. Compaction is the main
lever for agent-managed context: when a thread is bloated or the task changes,
the agent is instructed to `/compact` (and save durable facts with `memory`)
rather than wait for the automatic threshold. See
[Agent-managed context](#agent-managed-context).

Because a turn already in flight can't be reconfigured, a queued command runs
the moment that turn finishes — effort, model, and mode changes therefore take
effect on the **next** turn. Commands that need you at an interactive picker
(`/effort` with no argument), that end or rewind the session (`/quit`,
`/clear`, `/rewind`, `/resume`), or that set up providers (`/provider`,
`/login`, `/publish`, `/evolve`) stay your call and are refused with a note the
agent sees. Only the interactive TUI applies these commands, so the tool is
refused outright in headless `-p` runs, the gateway, and subagents — nothing is
silently dropped.

### Queued user messages

While a turn is running you can keep typing and press **Enter**. The message
lands in the transcript immediately, is announced with a "queued — will send
after this turn" notice, and runs automatically once the current turn finishes
(after any slash commands the agent itself queued via `run_command`). Multiple
messages stack FIFO; the status bar shows `queued N` while any are waiting.
The queue is capped (32); overflow keeps the composer text so nothing is lost.
`/clear` and Ctrl-C interrupt both drop the queue — a cleared or interrupted
conversation shouldn't auto-fire prompts that no longer apply.

## `wizard agents` and background subagents

`wizard agents` opens the agent dashboard from the shell, the same view as
`/dashboard` inside a session. Every running Wizard session heartbeats a
record to `~/.wizard/running/`, so the dashboard lists every live session on
the machine, grouped by state (working / needs input / idle / completed /
failed). From it you can:

- **Dispatch** a new background session: type a prompt into the input at the
  bottom and it spawns a detached headless sovereign run (`wizard --bg`) that
  registers in the same dashboard and survives your session exiting.
- **Peek** at the selected session's recent transcript.
- **Stop** the selected background session (Ctrl-X).

Within a session, the agent delegates long-horizon work to subagents via
`spawn_subagent`, and by default detaches them (`background: true`): the
turn returns immediately, you keep chatting, and the subagent's report lands
in context when it finishes. The status bar shows a `⏵ N bg subagent(s)`
marker while detached subagents run, and a `⏵ N bg task(s)` marker while
background `execute` tasks run (`/bashes` lists those).

## The subagent rail

Every subagent run — foreground or background — gets a row on the rail, which
sits between the composer and the status bar. It costs no screen space until
something has been delegated.

```text
 ❯ ◉ researcher   web_fetch                            0:12
   ● reviewer     auditing the diff                    0:04 +3
   ✔ tester       all 214 tests pass                   1:31
```

A row is a status dot (pulsing while running, `✔` done, `✗` failed), the
subagent's name, what it is doing right now (the tool in flight by name, else
its latest message), the elapsed clock, and `+N`: how much it has done since
you last looked at it. Five rows show at most, then a `+N more` marker.

Enter opens the selected run: that subagent's own conversation replaces the
main chat — its messages and its collapsible tool cards, drawn by the same
renderer — under a header naming the run.

```text
 ▌ researcher · running · 0:42 · 6 steps
   find the latest Tokio release notes  esc back · ↑↓ next agent
```

↑/↓ keep walking the runs once you are inside one — each takes over the screen
in turn, wrapping around — so browsing does not end when you open something.
Esc is only for leaving.

A foreground run is marked `· foreground` there: the parent turn is blocked
until it reports. The composer stays live while a pane is open, so you can
keep talking to the main agent while you watch one work.

| Key | What it does |
|-----|--------------|
| ↓ (in the composer) | Focus the rail, on the first running run (the last run if none is running) |
| ↑ / ↓ (on the rail) | Move between runs; ↑ off the top row returns focus to the composer |
| Enter | Open the selected run |
| Esc (in a pane) | Back to the main chat, focus in the composer |
| ↑ / ↓ (in a pane) | Open the previous / next run, wrapping around; with only one run, scroll it |
| Shift+↑ / Shift+↓ (in a pane) | Scroll the pane |
| PageUp / PageDown (in a pane) | Scroll the pane by ten lines |
| Ctrl-X | Kill the selected run (background runs only) |
| Any other key | Focus returns to the composer and the key is typed there |

A finished run rests on the rail for a few seconds and then retires, so the rail
stays a picture of live work rather than a log of every subagent the session
ever ran. Nothing is lost: a run's report is the output of the `spawn_subagent`
card in the main chat, which a background run writes back to when it lands. A
run you are watching never retires under you — its clock starts when you leave.

↓ only enters the rail when you are not part-way through input history, where
it keeps walking history. Any key the rail does not use returns focus to the
composer *and* is typed there, so a keystroke is never lost. Ctrl-X refuses a
foreground run, since the parent turn is blocked on it; Ctrl-C interrupts that
turn instead.

A subagent's own steps stay in its pane, so the main transcript holds only the
parent's `spawn_subagent` card and, for a detached run, the notice when it
reports back. Headless (`-p`) has no rail: there, subagent tool calls print
inline as `<name> ▸ <tool>` ([headless.md](headless.md)).

## Token usage and cost

Wizard accumulates the prompt/completion token counts every provider reports
on its final stream chunk.

- **TUI**: the status bar shows how many tokens the **next** model call will
  load into context (`12.3k tok`) — the last reported prompt size, falling
  back to a char/4 estimate of the remaining history after `/clear` or
  `/compact`. It is *not* a session-lifetime sum (those double-count multi-step
  history and stay inflated after a clear). `/cost` still prints the full
  session prompt/completion breakdown.
- **Headless**: the final summary line includes the run's totals:
  `[run finished: Completed (1234 prompt + 567 completion tokens)]`.
- **Log**: every turn appends one JSON line to `~/.wizard/usage.jsonl`:

  ```json
  {"ts":1760000000,"project":"/home/u/proj","model":"claude-fable-5","provider":"claude","prompt_tokens":1234,"completion_tokens":567,"mode":"genie"}
  ```

- **Rollup**: `wizard usage` prints per-project and per-provider totals from
  that log (turns, prompt/completion tokens, cost where prices are set);
  `--since 7d` limits the window.
- **Cost estimates**: set per-provider prices (USD per million tokens) in
  `~/.wizard/config.toml` and `/cost` adds an estimate:

  ```toml
  [[providers]]
  name = "claude"
  kind = "anthropic"
  # ...
  usd_per_mtok_in = 3.0
  usd_per_mtok_out = 15.0
  ```

## Token-aware compaction

History compaction now triggers on **either** the byte threshold
(`compact_threshold_bytes`, default 48 kB) **or** the last prompt exceeding
~80% of the model's context window, when the window is known:

- anthropic / openai / xai: static tables per model family
- llama.cpp: live `GET /props` probe for the loaded model's `n_ctx` (cached)
- ollama / unknown models: byte threshold only

Compaction also runs *between steps inside a turn*, so a long tool loop
cannot overflow the window mid-turn. The most recent messages (including the
in-flight turn's tool results) are always preserved verbatim, and the
summary is instructed to carry over the todo list state and the plan file
path (`.wizard/plan.md`).

## Agent-managed context

Wizard already persists every turn to `~/.wizard/sessions/<id>.jsonl` and
auto-compacts as above. The agent is also taught — via a block in its system
prompt — to steward that window deliberately instead of waiting for the
threshold:

| Situation | What the agent should do |
|-----------|--------------------------|
| Long investigation, finished sub-goal, or older tool dumps drowning the current task | `run_command` → `/compact` (summarizes older history into a progress note; recent tail stays verbatim) |
| User pivots to an unrelated task | Save durable facts with `memory`, rewrite the todo list, then `/compact`. Full prior transcript remains on disk as the session JSONL |
| New task must not see the old work at all | Ask the user for `/clear` (agent cannot run it). `/clear` rotates to a fresh session file; the previous JSONL is kept under `~/.wizard/sessions/` |
| Noisy multi-step work | `spawn_subagent` so intermediate steps never enter the parent context — only the final report does |
| Need a pressure check | `run_command` → `/status` (interactive surfaces) reports the current context size |

`run_command` is only available on interactive surfaces (TUI / GUI). Headless
`-p`, the gateway, and continuous mode still auto-compact; there the agent
leans harder on lean tool output and subagents. Prefer compacting over asking
the user to clear whenever the prior thread is still useful as a summary.

## Todo list

The native `todo` tool lets the agent maintain a working todo list (action
`write` replaces the whole list of `{content, status}` items; `read` returns
it; statuses: `pending` / `in_progress` / `completed`). It is read-only for
the plan gate, so the agent can draft its list while planning.

- **TUI**: a compact band just above the input mirrors the list (`/todos`
  toggles it; it auto-shows on the first update). The band reserves layout
  space so it never covers chat text — the transcript shrinks above it.
- **Headless**: each update prints `≡ todo: 2/5 done (current: <item>)`.
- **Subagents** get the tool too, with their own isolated list.

## Project instructions hierarchy

The system prompt's project-instructions section is assembled from every
directory between the filesystem root and the project root. In each
directory the first of `WIZARD.md` > `AGENTS.md` > `CLAUDE.md` wins, plus
the global `~/.wizard/WIZARD.md`. Files are concatenated outermost-first
(the project root's file has the last word), each prefixed with a comment
naming its path.

A line consisting of `@relative/path` inlines that file (one level deep,
~10 kB per include); the whole block is capped at ~40 kB.
