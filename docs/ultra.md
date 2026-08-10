# Ultra: `/ultra`

A council of lenses. While ultra is on, every turn starts by fanning the request
out to N **candidate subagents**, each with a different lens on the problem. A
judge compares their drafts head-to-head. Then the main agent executes the turn
with the drafts and the verdict injected as guidance.

```
/ultra          toggle ultra on/off
/ultra config   choose the lenses, and whether a judge compares them
```

While it is on, the status bar carries an `ULTRA ×3` chip in a loud accent and
the model label goes bold, because the mode is sticky and multiplies what a turn
costs.

## How it differs from `/fusion`

`/ultra` and `/fusion` are the same primitive (fan out N candidates, adjudicate,
hand the result to the one agent that acts) with different candidate sources.
What differs is where a candidate's answer comes from and what settles their
disagreements:

| | `/ultra` | `/fusion` |
| --- | --- | --- |
| Candidate | A subagent under a lens prompt, with read-only tools | A provider, answering as plain text |
| Adjudicator | Judge subagents rule head-to-head | The candidates critique each other over N rounds |
| Actor | The main agent, unchanged | The synthesizer |
| Toggling | Instant; the session survives | Swaps the active client, so the session resets |

`/ultra` is **agent-level**: the candidates are real subagents with read-only
tools. They open files, grep, and run searches against the actual repository
before they commit to a draft, so they disagree about what the code *is*, not
about what it might be.

`/fusion` is **model-level**: independent providers argue as text-only advisors
and a synthesizer answers, so disagreements come from genuinely different models
rather than from different postures on one.

## Running both

They stack. Turn both on and the lens roster is **dealt across the fusion
panel's providers**, round-robin: three lenses over a two-provider panel is the
`implementer` on the first provider, the `skeptic` on the second, the
`minimalist` back on the first. Three candidate runs, not three panel debates:
each candidate talks to one panel provider directly rather than through the fused
client.

That is what the two used to refuse each other over. Before they were one
primitive, each owned its own fan-out, so stacking them meant every ultra
candidate re-running the entire panel: candidates × panel × rounds before the
first token. Nesting one fan-out inside the other is still the wrong thing; what
changed is that dealing candidates across providers is now expressible, so the
combination is a roster instead of a refusal.

The `/fusion` and `/ultra` toggle notices and `/status` say so: the label picks up
`· across claude+openrouter`, because where a turn's spend is going is part of
what it costs. (The status-bar chip does not — it is just `ULTRA ×N`.) The judge
stays on the session's own client, since it reads drafts that already came from
everywhere and seating it would make *which model ruled* depend on how many
lenses there happened to be. That client is the fused one while `/fusion` is on,
so with both modes running the judge's steps do go through the panel — see
[Cost](#cost).

`[ultra]` still names no provider, and never will: which providers exist is a
question about the session, and the answer changes when you toggle `/fusion`
without a line of `[ultra]` changing.

## How an ultra turn works

1. **Candidates.** One read-only subagent per lens, spawned in parallel on the
   active client and model (or on its seat, when the roster is dealt across a
   fusion panel), each investigating the repository itself and producing a
   complete proposed answer. Each gets its own live pane on the subagent rail
   under the composer. Press Enter on a dot to watch one work.
2. **Compare.** The judge (also read-only) receives every draft, re-reads the
   code wherever two drafts disagree about it, and rules: which draft is best,
   what each got right and wrong, and the merged best approach. `judges = 0`
   skips this and hands the raw drafts over uncompared — and so does a roster
   that produced fewer than two drafts, since there is nothing to compare.
3. **Act.** The main agent — full tools, and the **sole tool-caller**, exactly as
   in fusion — receives the drafts and the verdict as a system message and runs
   the turn normally. Candidates never write a file, so there are never
   conflicting edits.

The drafts and the verdict also land in the transcript as one folded card
(`ultra ×3 · implementer+skeptic+minimalist · 1 judge` — click it to open it;
Ctrl-T also works, until the main agent's first tool call, after which Ctrl-T
toggles that call instead). The card is where they stay readable: a candidate's
pane retires off the rail eight seconds after it finishes, which is usually
minutes before the main agent is done working from what it wrote.

The guidance is scoped to the turn it was built for. It is dropped from the
conversation once that turn ends and is never written to the session, so it does
not accumulate in the context window, is not re-sent on later turns, and does not
come back on `/resume` — it is advice about a request that has already been
answered.

**Interrupting.** Ctrl-C during the pre-phase asks the turn to stop: each
candidate run notices on its next poll (mid-stream, not at its next step
boundary), its pane closes out, and the turn ends without the agent being torn
down. The interrupt belongs to the subagent runner itself now, so `spawn_subagent`
in the foreground honours it too; it is not something the council bolts on. (A
turn parked inside a long tool call cannot be asked; after a short grace period
the task is killed, as it always was.)

## Configuring the roster

`/ultra config` opens a multi-select: Space toggles a lens, Enter saves. There is
no separate candidate-count knob because there is no separate number — **one
toggled lens is one candidate**. The final row is the judge.

The built-in lenses — five, of which the first three are the default roster:

| Lens | Posture |
| --- | --- |
| `implementer` | Reads what is there and proposes the direct change. |
| `skeptic` | Hunts for what the obvious approach breaks. |
| `minimalist` | Finds the smallest correct diff. |
| `edge-cases` | Hunts the inputs and states the happy path misses. |
| `architect` | Weighs the change against the shape the codebase already has. |

The rows also include the built-in `worker` subagent and every subagent in
`~/.wizard/subagents/`, so any subagent you have already written can serve as a
lens. A file there **shadows a built-in of the same name**, which is how you
retune one: write `~/.wizard/subagents/skeptic.toml` and it replaces ultra's
skeptic. The same rule applies to `judge.toml`. (A `--harness-dir` bundle's
`subagents/` is layered last, so a bundle shadows your file in turn.) A lens
contributes a name, a description and a system prompt — ultra overrides
`max_steps` and `tool_scope`, so a lens file cannot quietly make the pre-phase
ten times more expensive than the roster says it is.

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
  you used to pay for one. Stacking `/fusion` does not multiply the *candidates*
  again: they are dealt across the panel, not run through it, so their count is
  the same and only which provider each one bills changes. It does multiply the
  rest. The judge sits on the session's own client, which under fusion *is* the
  fused client, so every judge step is a full panel fan-out — and so is every
  step of the main turn. Both modes on is the most expensive thing Wizard will do
  on your behalf.
- **The pre-phase is a serial barrier before the first token.** Nothing streams
  until every candidate and the judge have finished. Candidates fan out
  concurrently on a cloud provider; against a single-slot local `llama-server`
  they serialize completely, and the wait is the sum of them.
- **Every candidate and judge is metered.** Their model calls bill the parent, so
  `~/.wizard/usage.jsonl` and `/cost` report what an ultra turn actually spent,
  not what the main agent alone spent. (This is true of `spawn_subagent` runs now
  as well — the accounting is per session, not per agent.) The status bar's token
  figure is not this: it is the context meter, the size of the last prompt
  reported, which a candidate's own prompt will overwrite mid-pre-phase. Read
  `/cost` for spend.
