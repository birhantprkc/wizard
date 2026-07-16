# `wizard bench`: trajectory recording and replay benchmarks

`wizard bench` turns your own real work into a benchmark. Wizard records
every headless agent turn as a *trajectory*; you promote the interesting ones
into *cases* by attaching a check command; the runner replays cases against
any harness command in isolated git worktrees and prints pass-rate numbers.
Compare two result files and you have an A/B answer to "did this model /
prompt / build actually get better?", measured on your tasks, not someone
else's leaderboard.

Bench is fully self-contained: it never loads `~/.wizard/config.toml`, never
triggers onboarding, and needs no LLM at all (the runner is just a command).
Everything lives project-locally:

```
.wizard/trajectories.jsonl                        # append-only recorded turns
.wizard/bench/cases/<id>.toml                     # one case per file
.wizard/bench/results/<label>-<ts>.json           # one file per run
.wizard/bench/logs/<label>/<id>.harness.log       # harness stdout/stderr per case
.wizard/bench/logs/<label>/<id>.check.log         # check stdout/stderr per case
```

## The flow

1. **Record.** Run wizard headless as usual (`wizard --mode sovereign -p "..."`
   or `--continuous`). Every completed turn appends one line to
   `.wizard/trajectories.jsonl`: the prompt, the HEAD sha before the turn,
   whether the repo was dirty, the done reason, duration, model, and mode.
   Recording is best-effort and silent (it can never break a run), and it is
   suppressed inside bench replays (the runner sets `WIZARD_BENCH=1`) and
   fleet workers (the coordinator sets `WIZARD_FLEET=1`).

2. **Promote.** Inspect what you have, then attach a check command:

   ```bash
   wizard bench list --trajectories
   wizard bench promote 3f2a91c0 --check "cargo test -q"
   ```

   Promote refuses trajectories with no recorded sha (not a git repo) or a
   dirty working tree at record time; a replay could not reproduce that
   starting state. You can also write a case by hand:

   ```bash
   wizard bench add --id fix-auth --prompt "fix the auth module tests" \
       --check "cargo test -q -p auth" --tag rust --timeout 1200
   ```

   List and prune cases with tags:

   ```bash
   wizard bench list --tag rust     # only cases tagged rust
   wizard bench remove fix-auth     # delete a case by id
   ```

3. **Run.** Replay every case, or a subset by `--case` id and/or `--tag`
   (their union), against a harness:

   ```bash
   wizard bench run --label baseline
   wizard bench run --label rust-only --tag rust
   ```

   The run exits 0 only when every selected case passed (1 otherwise), so
   `wizard bench run` can gate CI directly.

4. **Compare.** Diff two runs case-by-case:

   ```bash
   wizard bench compare .wizard/bench/results/baseline-*.json \
                        .wizard/bench/results/candidate-*.json
   ```

## Case format

`.wizard/bench/cases/<id>.toml`:

```toml
id = "fix-auth"                       # [a-zA-Z0-9_-]+; becomes a file/worktree name
prompt = "fix the auth module tests"  # handed to the harness via {prompt}
base_ref = "0123abcd…"                # full commit sha the worktree starts from
check = "cargo test -q -p auth"       # exit 0 = pass, run after the harness
timeout_secs = 900                    # harness wall-clock budget
check_timeout_secs = 300              # check wall-clock budget
tags = ["rust"]
source = "manual"                     # or "recorded" (promoted trajectory)
created = "2026-06-10T12:00:00Z"
```

## Runner templates

The harness is an arbitrary shell command template; `{prompt}` is replaced
with the shell-quoted case prompt. A template without `{prompt}` runs
verbatim. Examples:

```bash
# Default: the wizard binary you invoked, in sovereign mode
wizard bench run --label this-build

# A different wizard build
wizard bench run --label v2 \
    --runner '/path/to/wizard-v2 --mode sovereign -p {prompt}'

# A different agent entirely (Claude Code)
wizard bench run --label claude \
    --runner 'claude -p {prompt} --dangerously-skip-permissions'

# Grok Build (xAI's terminal agent)
wizard bench run --label grok \
    --runner 'grok -p {prompt} --output-format json' --harness-json
```

### Recording token usage (`--harness-json`)

Pass `--harness-json` when the harness emits a JSON usage object on stdout —
`--output-format json` (one object) or `streaming-json` (NDJSON, the last
usage-bearing line wins). The run then records each case's total tokens and
stop reason alongside the pass/fail result, so you can compare agents by cost,
not just correctness. It reads the usage shapes emitted by Wizard
(`prompt_tokens` / `completion_tokens`), Grok Build and Anthropic
(`input_tokens` / `output_tokens`), and OpenAI (`total_tokens`); a harness
that emits no usage simply leaves the fields empty. The per-case line then
shows a trailing `<n> tok`.

Each case runs in its own detached worktree of `base_ref` under a temp dir,
with stdin closed and `WIZARD_BENCH=1` in the environment. Harness and check
output is captured to `.wizard/bench/logs/<label>/<id>.{harness,check}.log`
(the per-case line points there on FAIL), so failures stay diagnosable.
The harness's exit code is recorded but does not decide the outcome; only
the check command does (some harnesses exit nonzero on benign conditions).
A harness that overruns `timeout_secs` is killed and scored `timeout`;
infrastructure failures (missing ref, worktree errors) score `error`.
On a terminal, a progress bar on stderr tracks the case being replayed and
elapsed time; per-case lines and the summary still go to stdout as before.
Pass `--keep-worktrees` to inspect the replayed trees afterwards.

## Results and compare output

Each run writes a JSON file with the resolved runner, per-case results
(status, exit codes, timings), and the aggregate pass rate, then prints:

```
touch-case  PASS  harness 0.0s  check 0.0s
1/1 passed (100%), results: .wizard/bench/results/baseline-1781234567.json
```

The exit code mirrors the summary: 0 when every case passed, 1 when any
case failed, timed out, or errored.

`compare` prints the union of case ids with a marker per case (`↑` a pass
gained from A to B, `↓` a pass lost, blank unchanged, an em dash for a case
missing on one side), followed by both summaries and the delta in
percentage points.

## Current limitations

- Cases run sequentially; there is no parallelism yet.
- Only headless (sovereign / continuous) turns are recorded; genie TUI
  sessions don't land in the trajectory log.
- Replays need the recorded base sha to still exist and require a clean tree
  at record time; dirty trajectories cannot be promoted.
