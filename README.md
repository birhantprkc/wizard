# Wizard

[![CI](https://github.com/teddytennant/wizard/actions/workflows/ci.yml/badge.svg)](https://github.com/teddytennant/wizard/actions/workflows/ci.yml)

**One line. Your sovereign agent. Self-extending. Bring any model.**

![Wizard fixing a bug: provider list, prompt, live edit, diff](demo/demo.gif)

```bash
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | bash
```

One command installs the `wizard` binary. The first run asks which provider you want and sets up the rest. Pick **Local** and Wizard sizes a Qwen 3 GGUF to your hardware and runs [llama.cpp](https://github.com/ggml-org/llama.cpp)'s `llama-server` itself, no API key needed. Or sign in with an xAI or ChatGPT account, or bring a key for xAI, OpenAI, Anthropic, Google Gemini, DeepSeek, Groq, Mistral, Moonshot, Z.AI, MiniMax, Together, Fireworks, Cerebras, OpenRouter, Cloudflare Workers AI, or any OpenAI-compatible endpoint, and switch live with `/provider`. It's one fast Rust binary on Linux and macOS; everything it learns is plain TOML under `~/.wizard/` that you can edit or delete.

> **Other ways to install:** local-stack preinstall, minimal, bring-your-own-model, Nix, macOS, plus a first-run walkthrough, all in **[Getting started](docs/getting-started.md)**.

---

## What it does

- **Any model, switchable live.** Speaks the OpenAI-compatible chat API (streaming + native tool calls, with a prompt-based JSON fallback), so xAI, OpenAI, Anthropic, Gemini, DeepSeek, Groq, Mistral, Moonshot, Z.AI, MiniMax, Together, Fireworks, Cerebras, OpenRouter, Cloudflare Workers AI, Ollama, and any OpenAI-compatible endpoint (vLLM, LM Studio, …) all work. `/provider` switches the live agent between them; keys live in env vars or `~/.wizard/credentials.toml` (mode 0600), never in plaintext config. → [Providers](docs/getting-started.md#using-a-cloud-or-remote-provider)
- **Runs models locally, fully managed.** Pick Local and Wizard downloads a GGUF sized to your VRAM, then starts, supervises, and reuses `llama-server` for you, including a Metal build on Apple Silicon. → [Model tiers](docs/getting-started.md#model-tiers-automatic) · [Bring your own model](docs/byom.md)
- **Model fusion (`/fusion`).** Run a panel of your providers as a debate — the members critique each other's drafts — and synthesize one tool-capable answer. → [Fusion](docs/fusion.md)
- **Mixture of agents (`/ultra`).** Fan a turn out to N read-only subagents on the model you're already using (each with a different lens, each reading the actual repo), have a judge compare their drafts, then execute from the verdict. → [Ultra](docs/ultra.md)
- **Self-extension (`/evolve`), powered by LuaJIT.** Add skills, MCP servers, scripted tools, and subagents as plain files that go live on `/reload`. Scripted tools default to **embedded LuaJIT** — the just-in-time compiler runs your glue in-process, no external interpreter on `PATH`. Gated by a clean `cargo build` and a smoke test, Wizard can also rebuild its own binary. Every change is logged, and the prior binary is kept one `mv` from rollback. → [Self-extension](docs/evolve.md)
- **Runtime MCP, both directions.** stdio and HTTP MCP servers merge into the tool registry without a rebuild: the path for computer use, browser control, and databases. Wizard also serves *its own* tools over stdio (`wizard mcp-serve`), so any MCP client can call them. → [MCP](docs/mcp.md)
- **Editor embedding (ACP).** `wizard acp` runs Wizard as an [Agent Client Protocol](https://agentclientprotocol.com/) agent, so ACP editors (Zed, Neovim, Emacs) drive it as their coding agent, streaming assistant text, reasoning, and tool calls into the editor. → [ACP](docs/acp.md)
- **Genie / Sovereign modes, plus `--continuous`.** An interactive direct-action TUI (genie), headless autonomous runs (sovereign), or a perpetual mission (`--continuous`) that compacts its own context and self-heals through outages. → [Modes](docs/modes.md)
- **Agent-managed context.** Sessions are JSONL under `~/.wizard/sessions/`; auto-compaction keeps long runs inside the window; the agent is instructed to `/compact` (via `run_command`) when a thread is bloated, to save durable facts with `memory` and compact when the task changes, and to keep tool output lean so history stays useful. → [Agent-managed context](docs/usage.md#agent-managed-context)
- **Browser GUI and a desktop app** *(preview)*. `wizard gui` serves the same agent core as the TUI on 127.0.0.1: the transcript, a subagent rail you can open a running agent from, and a git panel whose changed files open their diff. `wizard app` puts that in a native window through the system webview (no bundled Chromium, ~10MB rather than ~200MB), and `wizard app --install` adds Wizard to your launcher. Still settling; the TUI remains the surface everything ships to first. → [Desktop app](docs/desktop.md)
- **Persistent memory.** The agent keeps what it learns as plain markdown under `~/.wizard/memory/<project>/`: typed (`user` / `feedback` / `project` / `reference`), linked with `[[name]]`, indexed into the system prompt each session. `/memory` reads it back; it is your file, not a black box. → [Memory](docs/memory.md)
- **Harness evolution (AHE).** The shipped defaults were improved offline by an evaluate→analyze→improve loop (80% → 100% pass@1 on a Terminal-Bench sample). The old local `wizard bench` trajectory runner is gone — AHE superseded it. → [Self-extension / harness](docs/evolve.md)
- **Messaging gateway.** Run headless as a bot you talk to from your phone (Telegram), each inbound message a sovereign agent turn in your project. → [Gateway](docs/gateway.md)
- **Make it your own.** After a deep evolve modifies its source, `/publish` forks upstream to your GitHub and hands out a one-line installer for your variant. → [Fork and distribute](docs/market.md)

**Smaller attack surface by construction.** A single memory-safe Rust binary with an embedded LuaJIT for self-extension — not a garbage-collected app runtime, not a separate interpreter process to inject into. Every install also converges on a different `/evolve` loadout, so there's no uniform tool surface to target. Read [SECURITY.md](SECURITY.md) before autonomous runs; tools execute with your privileges.

### haha suckers

You thought you needed an interpreter and to write your TUI in bloated TypeScript. No. Just have the just-in-time compiler for LuaJIT. Wizard is Rust + Ratatui for the surface, and when it extends itself the glue is Lua that the **JIT compiles** — in-process, fast, no Node, no Electron, no shipping a second runtime next to the binary. Self-extension without the bloat tax.

---

## Limitations

- **Platforms.** Linux (x86_64, aarch64) and macOS (Apple Silicon and Intel). Windows isn't supported; run it under WSL2. The installer downloads a prebuilt binary for your platform and builds from source when one isn't published yet.
- **Small local models are worse than frontier models.** A 9B–36B quantized Qwen will misformat tool calls, miss context, and need more steering than Claude- or GPT-class models. Wizard mitigates with native tool-call probing, a JSON fallback, and retry prompts; the 27B+ tiers make much better agents than the 9B tier.
- **No sandbox.** Tools run with your privileges, with no per-action approval gate in either mode. Read [SECURITY.md](SECURITY.md) before running on anything you don't trust, and prefer a container/VM for autonomous or continuous work.
- **Context windows are finite.** Wizard searches and reads selectively rather than ingesting the whole repo, auto-compacts older history, and instructs the agent to compact / reset deliberately when the task changes, but long sessions still eventually push out early detail. → [Agent-managed context](docs/usage.md#agent-managed-context)

---

## Docs

- [Getting started](docs/getting-started.md): install (all flavors, Nix, macOS), tiers, providers, first run, in-place updates (`wizard update`), troubleshooting
- [Usage](docs/usage.md): slash commands, `wizard agents`, the subagent rail, token usage and cost, todos, project instructions
- [Desktop app](docs/desktop.md): `wizard app`, the GUI in a native window, plus the launcher entry (`WIZARD_APP=1` to install)
- [Gateway](docs/gateway.md): run Wizard as a Telegram bot
- [Modes](docs/modes.md): genie, sovereign, and continuous
- [Self-extension](docs/evolve.md): `/evolve` tiers, gates, rollback
- [Fusion](docs/fusion.md): the `/fusion` debate panel
- [Ultra](docs/ultra.md): the `/ultra` mixture of agents
- [Bring your own model](docs/byom.md): any GGUF, or custom Ollama models
- [Custom commands & @files](docs/commands.md): your own `/commands`; `@path` file references
- [Memory](docs/memory.md): what Wizard remembers between sessions, and `/memory`
- [Hooks](docs/hooks.md) · [Tasks](docs/tasks.md) · [Web](docs/web.md) · [Headless output](docs/headless.md) · [Checkpoints](docs/checkpoints.md)
- [Doctor & status](docs/doctor.md): `wizard doctor` diagnostics, `/status`
- [Scheduler](docs/scheduler.md): cron-scheduled headless runs
- [Fleet](docs/fleet.md): parallel workers over git worktrees
- [Sync](docs/sync.md): `wizard sync` moves config, skills, and custom tooling between machines as a signed bundle
- [Fork and distribute](docs/market.md): publish your evolved Wizard
- [Architecture](docs/architecture.md): how it's built
- [Security](SECURITY.md): threat model
- [WIZARD.md](WIZARD.md): the agent's bundled behavioral charter, inherited and editable by every fork

## Development

Rust 2024, Ratatui, Tokio, embedded LuaJIT (`mlua`). Single binary, < 60 MB stripped.

```bash
git clone https://github.com/teddytennant/wizard
cd wizard
cargo build --release
./target/release/wizard
```

There's also a Nix flake: `nix run github:teddytennant/wizard`, or `nix develop` for a shell with the Rust toolchain and `llama-cpp`.

## Acknowledgements

Local inference is powered by [llama.cpp](https://github.com/ggml-org/llama.cpp) (ggml-org): Wizard installs its `llama-server` when you pick the local option. [Ollama](https://ollama.com) is a first-class supported provider.

## License

MIT; see [LICENSE](LICENSE).

## Author

Teddy Tennant ([github.com/teddytennant](https://github.com/teddytennant))
