# Ultra: `/ultra`

Mixture of agents. While ultra is on, every turn starts by fanning the request
out to N **candidate subagents** — all on the *same* provider and model you are
already using — each with a different lens on the problem. A judge compares their
drafts head-to-head. Then the main agent executes the turn with the drafts and
the verdict injected as guidance.

```
/ultra          toggle ultra on/off
/ultra config   choose the lenses, and whether a judge compares them
```

While it is on, the status bar carries an `ULTRA ×3` chip in a loud accent and
the model label goes bold, because the mode is sticky and multiplies what a turn
costs.

## How it differs from `/fusion`

`/fusion` is **model-level**: a panel of *different providers* argue as text-only
advisors, and a synthesizer answers. It swaps the active client, so toggling it
resets the session.

`/ultra` is **agent-level**: the candidates are real subagents on the *one* model
that is already active, and they have read-only tools. They open files, grep, and
run searches against the actual repository before they commit to a draft, so they
disagree about what the code *is*, not about what it might be. Nothing about the
provider changes, so the toggle is instant and the conversation survives it.

The two are mutually exclusive, and each refuses to turn on over the other: every
ultra candidate would re-run the entire fusion panel, billing the turn at
candidates × panel × rounds before the first token.

## How an ultra turn works

1. **Candidates.** One read-only subagent per lens, spawned in parallel on the
   active client and model, each investigating the repository itself and
   producing a complete proposed answer. Each gets its own live pane on the
   subagent rail under the composer — press Enter on a dot to watch one work.
2. **Compare.** The judge (also read-only) receives every draft, re-reads the
   code wherever two drafts disagree about it, and rules: which draft is best,
   what each got right and wrong, and the merged best approach. `judges = 0`
   skips this and hands the raw drafts over uncompared.
3. **Act.** The main agent — full tools, and the **sole tool-caller**, exactly as
   in fusion — receives the drafts and the verdict as a system message and runs
   the turn normally. Candidates never write a file, so there are never
   conflicting edits.

The drafts and the verdict also land in the transcript as one folded card
(`ultra ×3 · … · 1 judge` — click it, or Ctrl-T, to open it). That card is where
they stay readable: a candidate's pane retires off the rail a few seconds after
it finishes, which is usually minutes before the main agent is done working from
what it wrote.

The guidance is scoped to the turn it was built for. It is dropped from the
conversation once that turn ends and is never written to the session, so it does
not accumulate in the context window, is not re-sent on later turns, and does not
come back on `/resume` — it is advice about a request that has already been
answered.

**Interrupting.** Ctrl-C during the pre-phase asks the turn to stop: the fan-out
notices between polls, every candidate pane closes out, and the turn ends without
the agent being torn down. (A turn parked inside a long tool call cannot be asked
— after a short grace period the task is killed, as it always was.)

## Configuring the roster

`/ultra config` opens a multi-select: Space toggles a lens, Enter saves. There is
no separate candidate-count knob because there is no separate number — **one
toggled lens is one candidate**. The final row is the judge.

The built-in lenses:

| Lens | Posture |
| --- | --- |
| `implementer` | Reads what is there and proposes the direct change. |
| `skeptic` | Hunts for what the obvious approach breaks. |
| `minimalist` | Finds the smallest correct diff. |

The rows also include every subagent in `~/.wizard/subagents/`, so any subagent
you have already written can serve as a lens. A file there **shadows a built-in
of the same name**, which is how you retune one: write
`~/.wizard/subagents/skeptic.toml` and it replaces ultra's skeptic. The same rule
applies to `judge.toml`. A lens contributes a name and a system prompt and
nothing else — ultra overrides `max_steps` and `tool_scope`, so a lens file
cannot quietly make the pre-phase ten times more expensive than the roster says
it is.

This writes `[ultra]` to `~/.wizard/config.toml`:

```toml
[ultra]
lenses = ["implementer", "skeptic", "minimalist"]  # one subagent each
judges = 1                  # 0 skips the compare phase; up to 3
candidate_max_steps = 10    # tool-call budget for one candidate
judge_max_steps = 6         # tool-call budget for one judge
timeout_secs = 300          # wall-clock cap on one candidate or judge
max_draft_chars = 6000      # per-draft ceiling inside the injected guidance
```

`judges` above 1 has no UI: the checkbox says none-or-one, and a higher count set
here survives a save from the picker. Everything is validated when ultra is
built — an unknown lens name, a duplicate, a count out of range — and a roster
that would not run is refused with the offending field named rather than clamped
into something you did not ask for.

`timeout_secs` is mandatory and may not be zero. Without a deadline, a throttled
provider parks a candidate inside the subagent retry ladder and the turn hangs on
a spinner for five minutes with nothing to show.

## The read-only invariant

Candidates and judges run with a registry stripped to its read-only tools, and
`spawn_subagent` is `ToolAccess::Execute`, so it is stripped too — which is what
stops a candidate from recursing into another fan-out. The main agent remains the
only thing in the process that can touch the filesystem or run a command.

The consequence is worth stating plainly: **candidates argue from reading,
because they cannot run anything.** No candidate can execute the test suite to
find out which draft is right. The judge settles disagreements on the code, not
on evidence from running it, and the main agent is the first thing in the turn
that can actually check.

## Cost

- **Ultra multiplies spend by roughly the candidate count**, plus the judge, plus
  the turn itself. A three-lens roster with one judge is five agent runs where
  you used to pay for one.
- **The pre-phase is a serial barrier before the first token.** Nothing streams
  until every candidate and the judge have finished. Candidates fan out
  concurrently on a cloud provider; against a single-slot local `llama-server`
  they serialize completely, and the wait is the sum of them.
- **Every candidate and judge is metered.** Their model calls bill the parent, so
  the status-bar token counter, `~/.wizard/usage.jsonl` and `/cost` all report
  what an ultra turn actually spent, not what the main agent alone spent. (This
  is true of `spawn_subagent` runs now as well — the accounting is per session,
  not per agent.)
