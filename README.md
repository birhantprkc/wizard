# Wizard

[![CI](https://github.com/teddytennant/wizard/actions/workflows/ci.yml/badge.svg)](https://github.com/teddytennant/wizard/actions/workflows/ci.yml)

**One line. Your sovereign agent. Self-extending. Bring any model.**

![Wizard fixing a bug: provider list, prompt, live edit, diff](demo/demo.gif)

```bash
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | bash
```

One link, and you have a ready-to-go setup: the `wizard` binary installed and working out of the box. It's a Ratatui TUI agent with tool calling, git integration, skills, MCP, and `/evolve` self-extension. The installer puts down the binary and the default loadout — a Playwright browser (MCP) and a roster of four subagents (reviewer, researcher, tester, documenter) — and nothing else; the first `wizard` run greets you and asks which provider to use. Pick Local and Wizard handles the rest itself: it detects your hardware, downloads a Qwen 3 GGUF sized to it, and sets up [llama.cpp](https://github.com/ggml-org/llama.cpp)'s `llama-server` (or reuses an existing Ollama install) — no API key needed. Or bring a key for OpenRouter, xAI (Grok), OpenAI, Anthropic, or any OpenAI-compatible endpoint (OpenAI, Groq, vLLM, LM Studio, llama.cpp, Ollama), and switch providers live with `/provider`. Still one fast Rust binary; the loadout is plain TOML under `~/.wizard/` that you can edit or delete ([details](docs/loadout.md)).

**One installer, four flavors.** The default above installs no model and writes no config — onboarding on first run does that. `WIZARD_LOCAL=1` preinstalls the local stack non-interactively (llama-server, a GGUF tier picked for your VRAM, and a ready config) — useful for headless boxes and provisioning scripts:

```bash
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | WIZARD_LOCAL=1 bash
```

Prefer to start lean? `WIZARD_MINIMAL=1` installs just the binary — the same onboarding on first run, without the loadout:

```bash
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | WIZARD_MINIMAL=1 bash
wizard   # launches onboarding on first run
```

Running your own Ollama model (a fine-tune, a private registry tag, a local GGUF via Modelfile)? `WIZARD_BYOM=1` walks you through bringing it ([details](docs/byom.md)):

```bash
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | WIZARD_BYOM=1 bash
```

---

## Why Wizard

**Any provider, switchable live.** The agent loop speaks the OpenAI-compatible chat API (streaming and native `tool_calls`, with a prompt-based JSON fallback for models without native tool support), so any compatible endpoint works: OpenAI, Groq, vLLM, LM Studio, llama.cpp, plus OpenRouter (`kind = "openrouter"`, hundreds of models behind one key, with attribution headers sent automatically), Ollama (`kind = "ollama"`, native API), Anthropic, and xAI (`kind = "xai"` with an API key, or `kind = "xaioauth"` to sign in with your xAI account via `wizard --login xai`, no API key needed). `/provider add` registers a new endpoint, `/provider use` switches the live agent to it, and API keys are read from environment variables rather than stored on disk. Running a model locally is one fully-managed option: pick Local in onboarding (or preinstall it with `WIZARD_LOCAL=1` at install time) and Wizard downloads a GGUF sized to your hardware and manages `llama-server`'s lifecycle itself: when nothing answers at the configured port, it starts `llama-server` with your model, waits for it to load, and leaves it serving after you exit so the next launch is instant.

**Onboarding from scratch.** The first run (or `wizard --onboard` any time) opens a clean Ratatui onboarding wizard that asks which provider to use (Local is one pick — no model question), then messaging gateway and mode, and writes your `~/.wizard/config.toml`. No editing TOML by hand; no defaults you didn't choose.

**Messaging gateway.** Run Wizard headless as a bot you can talk to from your phone. `wizard --gateway` connects the configured gateway (Telegram, or `none` for terminal-only), runs each inbound message as a sovereign agent turn in your project, and replies with the result; chat-ID allow-list and env-var token included. Configure it in onboarding or `[gateway]` in config.toml.

**Tiered `/evolve` self-extension.** Wizard extends itself at runtime. New skills, MCP servers, scripted tools, and subagents are plain files under `~/.wizard/`, live after `/reload` and reverted by deleting the file. When a change needs new Rust, `/evolve --deep` proposes a diff to Wizard's own source and, gated by your approval, a successful `cargo build --release`, and a `--version` smoke test, replaces its own binary. The old binary is kept as `wizard.prev` beside the new one, so rollback is a single `mv`, and every evolution is logged with its diff to `~/.wizard/evolution.jsonl`.

**Runtime MCP.** Wizard is an MCP client (stdio and HTTP). Declare a server in `~/.wizard/mcp.toml`, or have `/evolve` register one, and its tools merge into the registry on `/reload` without a rebuild. Stdio servers are spawned with a cleared, allowlisted environment and dynamic-linker variables stripped, and every request is time-bounded. This is the path for computer use, browser control, databases, and anything else shipped as an MCP server.

**Smaller attack surface by construction.** Wizard is harder to attack than agent harnesses built on memory-unsafe languages: a single Rust binary with no garbage-collected runtime, no interpreter to inject into, and the memory-safety guarantees that rule out the buffer overflows and use-after-frees that plague C/C++ tooling. Self-extension shrinks the attack surface further. Because every user assembles a different loadout of skills, MCP servers, and scripted tools through `/evolve`, there is no single uniform tool surface to target: an exploit written against one person's Wizard does not transfer to the next, and unused capabilities simply aren't present to be attacked. Each install converges on the minimal set of tools its owner actually uses.

**Genie / Sovereign dual modes.** Genie is the interactive default: a full Ratatui TUI that bypasses permissions and acts directly. It reads, writes, shells, and runs git without pausing to ask, narrating briefly as it goes. Sovereign is the autonomous mode: headless-capable, self-directing, circuit-breaking on repeated failures, and controllable mid-run via a loop-control file. Both modes auto-approve tool calls by default; the difference is interactivity and continuity. Switch live with `/genie` and `/sovereign`.

**Perpetual `--continuous` mode.** Given one goal, `--continuous` runs sovereign mode indefinitely. It persists a durable mission to `.wizard/mission.toml`, picks the next most valuable action each cycle, sleeps through transient model-server outages instead of dying, and compacts its own context so it never overflows. When it improves itself via `evolve`, up to rebuilding its own binary, it re-execs into the new image and resumes the mission. No human is in the loop; the kill switch is one line in `.wizard/loop-control`, and deep self-modification stays behind the same automated build, smoke-test, and rollback gates. See [docs/modes.md](docs/modes.md#continuous-mode-perpetual-sovereign).

**`wizard bench`: measure, don't vibe.** Wizard records every headless task it runs as a trajectory (prompt, starting commit, outcome, duration) in `.wizard/trajectories.jsonl`. Promote the good ones into benchmark cases with a check command, and `wizard bench run` replays them in isolated git worktrees against any harness (this build, a candidate build, or another agent CLI entirely), scoring each case and printing pass rates. `wizard bench compare` then shows the case-by-case delta, so "the new model is better" becomes a number measured on your own work. No LLM or config needed to run the bench itself. See [docs/bench.md](docs/bench.md).

**Make it your own Wizard.** After a deep evolve modifies Wizard's source, run `/publish` (or `wizard --publish`) to fork the upstream repo to your GitHub account, push your modified `~/.wizard/src` to a branch, and get a one-line installer for your variant:

```
curl -fsSL https://raw.githubusercontent.com/<owner>/wizard/<ref>/install.sh | WIZARD_REPO=<owner>/wizard WIZARD_REF=<ref> WIZARD_BUILD_FROM_SOURCE=1 bash
```

Anyone who runs it gets your Wizard, built from your source on their machine and carrying your behavioral charter ([WIZARD.md](WIZARD.md)) in the binary. Publish is approval-gated and logged, and requires an authenticated `gh` (`gh auth login`). Forks install from source because they don't ship prebuilt release binaries by default. See [docs/market.md](docs/market.md).

---

## Quick start

```bash
# Install (binary + default loadout)
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | bash

# Launch the interactive TUI (genie mode, the default)
# First run opens onboarding: pick a provider, done
wizard

# Sovereign autonomous mode
wizard --mode sovereign -p "refactor the auth module and add tests"

# Perpetual mode: keeps working and self-improving until you stop it
wizard --continuous -p "keep hardening this codebase: tests, docs, performance"

# Self-extension: add a capability live (skill / MCP server / scripted tool)
wizard --evolve -p "add a skill for conventional commit messages"

# Replay your recorded tasks against this build (see docs/bench.md)
wizard bench run

# Re-run onboarding from scratch (provider, model, gateway, mode)
wizard --onboard

# Run as a messaging bot (reads the [gateway] section of config.toml)
wizard --gateway
```

Add or switch model providers at any time from inside the TUI, with any OpenAI-compatible endpoint, OpenRouter, Anthropic, or xAI:

```
/provider add openai openai https://api.openai.com/v1 gpt-4o OPENAI_API_KEY
/provider add xai xai https://api.x.ai/v1 grok-4.3 XAI_API_KEY
/provider add openrouter openrouter https://openrouter.ai/api/v1 openrouter/auto OPENROUTER_API_KEY
/provider use openai          # switch the live agent to it
/provider list                # show configured providers
```

Or sign in with your xAI account instead of using an API key: run `wizard --login xai` (or `/login xai` inside the TUI) to complete the OAuth flow in your browser, then `/provider add xai xaioauth https://api.x.ai/v1 grok-4.3`.

### Run models locally

Wizard runs its own model server. When the active provider is llama.cpp and nothing answers at its port, Wizard spawns `llama-server` with the configured GGUF (`gguf_path` in `~/.wizard/config.toml`), logs it to `~/.wizard/llama-server.log`, and waits for the model to load. The server is detached and keeps serving after Wizard exits. Control it from the TUI:

```
/server status   # ready / loading its model / not running
/server stop     # stop the server Wizard started (and only that one)
/server start    # start it again
```

Picking Local in onboarding (or installing with `WIZARD_LOCAL=1`) detects GPU VRAM (NVIDIA via `nvidia-smi`, AMD via `rocm-smi` or amdgpu sysfs; system RAM on CPU-only boxes) and downloads the right GGUF tier from Hugging Face into `~/.wizard/models/`:

| Available VRAM | GGUF downloaded | Approx. size |
|----------------|-----------------|--------------|
| ≥ 24 GB | `Qwen3.6-35B-A3B-UD-Q4_K_M.gguf` | ~21 GB (MoE) |
| 18–24 GB | `Qwen3.6-27B-Q4_K_M.gguf` | ~16 GB (dense) |
| 8–18 GB | `Qwen3.5-9B-Q4_K_M.gguf` | ~6 GB |
| < 8 GB / undetectable | `Qwen3.5-9B-Q4_K_M.gguf` (CPU / partial offload) | ~6 GB |

Release tarballs are verified against the release's `checksums.txt` before install. To use a different model, set `WIZARD_MODEL=<tag>` or point `gguf_path` at any GGUF ([BYOM](docs/byom.md)). Prefer Ollama? `WIZARD_USE_OLLAMA=1` at install time sets up the same auto-tiered model on Ollama instead. Full details in [docs/getting-started.md](docs/getting-started.md).

**Migrating from Ollama?** An explicit `[[providers]]` entry with `kind = "ollama"` keeps working exactly as before. The synthesized local default is llama.cpp: a legacy config that only sets top-level `model` / `ollama_host` now resolves to `llama-server` at `http://127.0.0.1:8080` (set `gguf_path` so Wizard can start it for you). To stay on Ollama, add the provider explicitly: `/provider add local ollama http://127.0.0.1:11434 <model>`. And if the local backend isn't installed or can't start, Wizard doesn't give up: it falls back to any configured cloud provider, then to `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` / `XAI_API_KEY` / `OPENROUTER_API_KEY` from the environment, and finally opens setup so you can bring your own provider.

---

## How it compares

Verified against each tool's documentation as of June 2026:

| | **Wizard** | **aider** | **goose** (Block / AAIF) | **opencode** |
|---|---|---|---|---|
| Providers | Any OpenAI-compatible endpoint + OpenRouter + Anthropic + xAI (API key or account sign-in), switchable at runtime via `/provider`; local llama.cpp with Wizard managing `llama-server` itself; Ollama | Ollama + any OpenAI-compatible endpoint; top results come from cloud models | 15+ providers incl. Ollama | 75+ providers incl. Ollama |
| MCP | Yes: stdio + HTTP, registerable at runtime via `/evolve` | No native support (open RFC) | Yes: one of the earliest and deepest integrations, 70+ documented extensions | Yes: local + remote servers, OAuth for remote |
| Self-extension | Tiered `/evolve`, up to and including rebuilding its own binary (gated + rollback) | None | Extensions and recipes via MCP | TypeScript/JS plugin system |
| Interface | Ratatui TUI | Terminal chat CLI | CLI + native desktop app (macOS/Linux/Windows) | Polished TUI |
| Language | Rust | Python | Rust (TS desktop app) | TypeScript |
| License | MIT | Apache-2.0 | Apache-2.0 | MIT |

aider's git workflow (clean auto-commits per change) is still the reference; goose has the broadest MCP ecosystem and is now vendor-neutral under the Linux Foundation's Agentic AI Foundation; opencode has the widest provider support and a polished TUI.

Wizard's bet is narrower: one binary, any model you choose, an onboarding that starts from your choices rather than someone's defaults, and an agent that grows its own capabilities through audited, reversible steps.

---

## Limitations (v0.1)

- **Linux only.** x86_64 and aarch64. macOS is planned for v0.2; the installer currently refuses Darwin rather than half-working.
- **Small local models are worse than frontier models.** If you pick the local option, a 9B–36B quantized Qwen will misformat tool calls, miss context, and need more steering than Claude or GPT-class models. Wizard mitigates this with native tool-call probing, a JSON fallback, and retry prompts. The 27B+ tiers make much better agents than the 9B tier.
- **No sandbox.** Tools run with your privileges; Wizard auto-approves tool calls by default in both modes. Read [SECURITY.md](SECURITY.md) before running on anything you don't trust, and prefer a container/VM for autonomous or continuous work.
- **Context windows are finite.** Large codebases exceed what a model can hold; Wizard searches and reads selectively rather than ingesting the repo, and long sessions will eventually push out early context.

---

## Docs

- [Getting started](docs/getting-started.md): install, tiers, first run, troubleshooting
- [Modes](docs/modes.md): genie vs sovereign
- [Self-extension](docs/evolve.md): `/evolve` tiers, gates, rollback
- [Fork and distribute](docs/market.md): publish your evolved Wizard; one-line installer for your fork
- [Default loadout](docs/loadout.md): the preconfigured browser MCP and subagent roster
- [Bring your own model](docs/byom.md): any GGUF, or custom Ollama models
- [Architecture](docs/architecture.md): how it's built
- [Security](SECURITY.md): threat model
- [WIZARD.md](WIZARD.md): the agent's bundled behavioral charter; inherited and editable by every fork

## Development

Rust 2024, Ratatui, Tokio. Single binary, < 60 MB stripped.

```bash
git clone https://github.com/teddytennant/wizard
cd wizard
cargo build --release
./target/release/wizard
```

## Acknowledgements

Local inference is powered by [llama.cpp](https://github.com/ggml-org/llama.cpp) (ggml-org): Wizard installs its `llama-server` when you pick the local option, and Wizard's run-it-locally option stands on that project's work. [Ollama](https://ollama.com) is a first-class supported provider.

## License

MIT; see [LICENSE](LICENSE).

## Author

Teddy Tennant ([github.com/teddytennant](https://github.com/teddytennant))
