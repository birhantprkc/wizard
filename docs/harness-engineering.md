# Harness engineering

How Wizard's defaults got where they are. Three passes, each measured before anything was kept.

## 1. AHE: evolving the harness bundle (July 2026)

`wizard harness export` writes everything the model sees as editable files: the system prompt, every tool description, the skills and the subagents. [wizard-ahe](https://github.com/teddytennant/wizard-ahe), a fork of [Agentic Harness Engineering](https://github.com/china-qijizhifeng/agentic-harness-engineering) ([arXiv:2604.25850](https://arxiv.org/abs/2604.25850)), runs Wizard on a Docker task set with a candidate bundle, reads the transcripts, and has an evolve agent rewrite the bundle. Each edit is a git commit. An edit that makes the score worse is rolled back. A person reviews what survives, and the accepted edits are baked into the source as the new defaults.

On a 10-task Terminal-Bench sample with Grok 4.5, pass@1 went from 80% to 100% (measured 2026-07-12, one errored trial in the run). Ten tasks is a smoke test, not a benchmark result, and those ten are part of the 89 in the full run below, so treat the full run as the real number.

## 2. Terminal-Bench post-mortem (September 2026)

The full Terminal-Bench 2.1 run with Grok 4.6 resolved 67 of 89 once five passes that had fetched the task's own tests were counted as failures. Wizard, Terminus 2 and Grok Build ran on the same box, and every miss was read by hand. What cost tasks:

| cause | tasks |
|---|---:|
| no sense of time, nothing on disk when the clock ran out | 7 |
| checked against the example, not the grader's scenario, then stopped early | 5 |
| `read_file` could not open images | 4 |
| `apt` piped to `tail` reported success without `pipefail` | 1 to 3 |
| broken verifier | 2 |

Context loss, truncation, edit failures and harness overhead were checked and ruled out. 53 of 53 edits and 116 of 116 writes applied, and the harness was about 1% of wall time.

What shipped from it:

- 3.1.1: `execute` runs under `pipefail`, `read_file` returns images, reasoning tokens are counted, old tool results shrink between compactions, and the stall detector works behind a proxy.
- 3.2.0: a clock for runs with a deadline, a completion review before a headless run calls itself done, and the xAI prompt cache key that was never being sent. Cached input went from 8.9% to 97.2%, read at a quarter of the price.

3.1.1 scored 66/89 (74.2%) with every public copy of the benchmark blocked, the same as 3.0.1's honest 67 within noise. Three tries each on the 35 hardest tasks resolved 46.7% against 34.3% for one try, so a single run moves by several tasks. The per-task list is in [`tbench/RESULTS.md`](../tbench/RESULTS.md).

## 3. Tokens (3.5)

A clean first request cost 16,101 prompt tokens. 881 session logs (55,396 tool calls) showed where they went: 25 native tool schemas (5.2k), a Playwright MCP server called 33 times in all those sessions (4.7k), and a skill whose whole body went out on every request because the frontmatter parser read a nested `always: true` as a top-level one. Over a whole session the biggest costs were old tool-call arguments, which nothing pruned, and continuous mode appending the entire mission every cycle. One long session held 47 copies.

Four profiles were run on Grok 4.6 on 8 tasks taken from a real project's history, each a "failing tests" commit followed by the commit that makes them pass. Every profile got 2 tries per task, and a run that edited a protected test or broke another test counted as a failure.

| profile | first request | pass | prompt tokens per task | est. cost per task | wall |
|---|---:|---:|---:|---:|---:|
| stock (3.2.5) | 15,517 | 16/16 | 610k | $0.61 | 243 s |
| **safe (3.5 default)** | 13,857 (10,087 shipped) | 16/16 | **522k** | **$0.54** | 262 s |
| lean | 5,852 | 16/16 | 578k | $0.66 | 405 s |
| min | 1,724 | 16/16 | 568k | $0.66 | 349 s |

The lesson was not the obvious one. `min` sends a ninth of what `stock` sends on the first request, but on a whole task it costs more than `safe`. A stripped prompt makes the model work in smaller steps, three to five more per task, and each extra step sends new tokens that are not cached. The prompt that was cut was the cached part. So 3.5 ships `safe` as the default: the bug fixes and the de-duplication, with no cuts to what the model is told. It also lists MCP tools by name and loads them when called, which came after these runs and takes its first request from 13,857 to 10,087. `lean` and `min` are there for when the first request is what you pay for. [Token profiles](token-profiles.md) has what each one does.
