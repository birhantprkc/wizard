# `wizard bench` — trajectory recording and replay benchmarks

`wizard bench` turns your own real work into a benchmark. Wizard records
every headless agent turn as a *trajectory*; you promote the interesting ones
into *cases* by attaching a check command; the runner replays cases against
any harness command in isolated git worktrees and prints pass-rate numbers.
Compare two result files and you have an A/B answer to "did this model /
prompt / build actually get better?" — measured on your tasks, not someone
else's leaderboard.

Bench is fully self-contained: it never loads `~/.wizard/config.toml`, never
triggers onboarding, and needs no LLM at all (the runner is just a command).
Everything lives project-locally:

```
.wizard/trajectories.jsonl              # append-only recorded turns
.wizard/bench/cases/<id>.toml           # one case per file
.wizard/bench/results/<label>-<ts>.json # one file per run
```

## The flow

1. **Record.** Run wizard headless as usual (`wizard --mode sovereign -p "..."`
   or `--continuous`). Every completed turn appends one line to
   `.wizard/trajectories.jsonl`: the prompt, the HEAD sha before the turn,
   whether the repo was dirty, the done reason, duration, model, and mode.
   Recording is best-effort and silent — it can never break a run — and it is
   suppressed inside bench replays (the runner sets `WIZARD_BENCH=1`).

2. **Promote.** Inspect what you have, then attach a check command:

   ```bash
   wizard bench list --trajectories
   wizard bench promote 3f2a91c0 --check "cargo test -q"
   ```

   Promote refuses trajectories with no recorded sha (not a git repo) or a
   dirty working tree at record time — a replay could not reproduce that
   starting state. You can also write a case by hand:

   ```bash
   wizard bench add --id fix-auth --prompt "fix the auth module tests" \
       --check "cargo test -q -p auth" --tag rust --timeout 1200
   ```

3. **Run.** Replay every case (or a `--case` subset) against a harness:

   ```bash
   wizard bench run --label baseline
   ```

4. **Compare.** Diff two runs case-by-case:

   ```bash
   wizard bench compare .wizard/bench/results/baseline-*.json \
                        .wizard/bench/results/candidate-*.json
   ```

## Case format

`.wizard/bench/cases/<id>.toml`:

```toml
id = "fix-auth"                       # [a-zA-Z0-9_-]+ — becomes a file/worktree name
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
    --runner '/path/to/wizard-v2 --mode sovereign --auto -p {prompt}'

# A different agent entirely (Claude Code)
wizard bench run --label claude \
    --runner 'claude -p {prompt} --dangerously-skip-permissions'
```

Each case runs in its own detached worktree of `base_ref` under a temp dir,
with stdin closed, output captured, and `WIZARD_BENCH=1` in the environment.
The harness's exit code is recorded but does not decide the outcome — only
the check command does (some harnesses exit nonzero on benign conditions).
A harness that overruns `timeout_secs` is killed and scored `timeout`;
infrastructure failures (missing ref, worktree errors) score `error`.
Pass `--keep-worktrees` to inspect the replayed trees afterwards.

## Results and compare output

Each run writes a JSON file with the resolved runner, per-case results
(status, exit codes, timings), and the aggregate pass rate, then prints:

```
touch-case  PASS  harness 0.0s  check 0.0s
1/1 passed (100%) — results: .wizard/bench/results/baseline-1781234567.json
```

`compare` prints the union of case ids with a marker per case — `↑` a pass
gained from A to B, `↓` a pass lost, blank unchanged, `—` missing on one
side — followed by both summaries and the delta in percentage points.

## Current limitations

- Cases run sequentially; there is no parallelism yet.
- Only headless (sovereign / continuous) turns are recorded — genie TUI
  sessions don't land in the trajectory log.
- Replays need the recorded base sha to still exist and require a clean tree
  at record time; dirty trajectories cannot be promoted.
