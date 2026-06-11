# Usage tracking, todos, and project instructions

Core agent upgrades that work identically in every mode (genie TUI,
sovereign headless, perpetual, gateway).

## Token usage and cost

Wizard accumulates the prompt/completion token counts every provider reports
on its final stream chunk.

- **TUI**: the status bar shows a compact session total (`12.3k tok`);
  `/cost` prints the full prompt/completion breakdown.
- **Headless**: the final summary line includes the run's totals:
  `[run finished: Completed — 1234 prompt + 567 completion tokens]`.
- **Log**: every turn appends one JSON line to `~/.wizard/usage.jsonl`:

  ```json
  {"ts":1760000000,"project":"/home/u/proj","model":"claude-fable-5","provider":"claude","prompt_tokens":1234,"completion_tokens":567,"mode":"genie"}
  ```

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
- **Headless**: each update prints `≡ todo: 2/5 done — current: <item>`.
- **Subagents** get the tool too, with their own isolated list.

## Project instructions hierarchy

The system prompt's project-instructions section is assembled from every
directory between the filesystem root and the project root — in each
directory the first of `WIZARD.md` > `AGENTS.md` > `CLAUDE.md` wins — plus
the global `~/.wizard/WIZARD.md`. Files are concatenated outermost-first
(the project root's file has the last word), each prefixed with a comment
naming its path.

A line consisting of `@relative/path` inlines that file (one level deep,
~10 kB per include); the whole block is capped at ~40 kB.
