# Usage

Day-to-day reference: the TUI's slash commands, the `wizard agents`
dashboard, and the core mechanics (token usage, todos, project
instructions) that work identically in every mode (genie TUI, sovereign
headless, perpetual, gateway).

## Slash commands

Everything typed as `/command` in the TUI. Tab-completion lists these with
inline hints.

| Command | What it does |
|---------|--------------|
| `/help` | List the available commands |
| `/clear` | Clear the conversation |
| `/model [tag]` | Show the current model, or switch to `tag` |
| `/mode [genie\|sovereign]` | Show or switch personality mode (`/genie` and `/sovereign` are shortcuts) |
| `/plan` | Toggle plan mode (also Shift+Tab): read-only investigation until a plan is approved |
| `/omakase` | Toggle omakase: chef's-choice plan mode, the agent decides and auto-approves its own plan ([modes.md](modes.md)) |
| `/evolve [--deep] <desc>` | Self-extend: add a skill, MCP server, scripted tool, or subagent; `--deep` rebuilds the binary ([evolve.md](evolve.md)) |
| `/reload` | Reload skills, scripted tools, and MCP servers without a restart |
| `/rewind [turn]` | Restore file checkpoints and truncate history; no argument opens the turn picker ([checkpoints.md](checkpoints.md)) |
| `/resume [id]` | Reopen a past session and continue it; no argument opens the session picker |
| `/compact` | Summarize older history into a progress note now, instead of waiting for the automatic threshold |
| `/agents` | Browse the subagent roster; Enter pre-fills a delegation request |
| `/subagents` | Toggle the in-session subagent monitor: every subagent run this session, with live status |
| `/dashboard` | Toggle the machine-wide session manager, same view as `wizard agents` (below) |
| `/bashes` | List background tasks (`execute` with `run_in_background`), running and finished ([tasks.md](tasks.md)) |
| `/goal [text]` | Show or set the standing mission goal (drives sovereign/continuous mode; persists to `.wizard/mission.toml`) |
| `/diff` | Toggle the git diff sidebar |
| `/todos` | Toggle the todo side panel |
| `/cost` | Session token usage, with cost estimates when per-provider rates are configured |
| `/memory` | Show the saved project memories |
| `/status` | Session status: model, provider, mode, session id, usage, todo progress, background tasks |
| `/doctor` | Environment diagnostics, same checks as `wizard doctor` ([doctor.md](doctor.md)) |
| `/provider …` | Add, remove, or switch LLM providers; no arguments opens the interactive menu |
| `/fusion [config]` | Toggle model fusion, or configure the panel ([fusion.md](fusion.md)) |
| `/server [status\|start\|stop]` | Manage the local llama-server |
| `/login <provider>` | OAuth sign-in for providers that support it (currently `xai`) |
| `/publish [branch]` | Fork Wizard to your GitHub and get a one-line installer ([market.md](market.md)) |
| `/settings` | Open the in-app settings menu |
| `/vim` | Toggle modal (vim-style) editing of the input composer |
| `/quit` | Exit (`/q` and `/exit` work too) |

Your own commands (markdown files that expand into prompts) sit alongside
these; see [commands.md](commands.md), which also covers `@path` file
references.

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
in context when it finishes. `/subagents` monitors them live, and the status
bar shows a `⏵ N bg task(s)` marker while background `execute` tasks run
(`/bashes` lists those).

## Token usage and cost

Wizard accumulates the prompt/completion token counts every provider reports
on its final stream chunk.

- **TUI**: the status bar shows a compact session total (`12.3k tok`);
  `/cost` prints the full prompt/completion breakdown.
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

## Todo list

The native `todo` tool lets the agent maintain a working todo list (action
`write` replaces the whole list of `{content, status}` items; `read` returns
it; statuses: `pending` / `in_progress` / `completed`). It is read-only for
the plan gate, so the agent can draft its list while planning.

- **TUI**: a side panel mirrors the list (`/todos` toggles it; it auto-shows
  on the first update).
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
