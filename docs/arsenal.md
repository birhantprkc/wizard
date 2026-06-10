# Wizard Arsenal

Wizard Arsenal is a preconfigured distribution of Wizard: the same local, self-extending coding agent, shipped with a fuller default loadout so the first run is already equipped. It is a separate repository ([`teddytennant/wizard-arsenal`](https://github.com/teddytennant/wizard-arsenal)) that installs the upstream `wizard` binary and then lays down a richer configuration under `~/.wizard/`.

The binary is unchanged upstream Wizard. Arsenal adds *configuration*, not source — everything in [Self-extension](evolve.md), [Modes](modes.md), and [Fork and distribute](market.md) works exactly as documented.

---

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard-arsenal/main/install.sh | bash
```

The installer:

1. Detects GPU VRAM (or system RAM on CPU-only boxes) and selects a Qwen model tier — the same logic as the [upstream installer](getting-started.md#model-tiers-automatic).
2. Stages `~/.wizard/config.toml` from the Arsenal template, substituting the selected model — **only if no config exists**.
3. Runs upstream Wizard's installer from source, which installs Ollama, pulls the model, and builds and installs the `wizard` binary.
4. Lays down the Arsenal configuration: `~/.wizard/mcp.toml` and `~/.wizard/subagents/*.toml` — each only if it is not already present.

Nothing under `~/.wizard/` that already exists is overwritten. If you have an existing Wizard install, Arsenal adds only what is missing.

---

## What Arsenal preconfigures

### A browser (Playwright MCP)

`~/.wizard/mcp.toml` declares the [Playwright MCP](https://github.com/microsoft/playwright-mcp) server:

```toml
[[server]]
name = "playwright"
transport = "stdio"
command = "npx"
args = ["-y", "@playwright/mcp@latest"]
```

This is exactly the browser recipe from [WIZARD.md §2](../WIZARD.md), shipped ready instead of acquired via `/evolve`. When Wizard starts (or after `/reload`), the server's navigate / click / type / snapshot tools merge into the registry, and the agent can read pages, fill forms, and do computer-use style tasks.

It requires **Node and `npx`** on your PATH. If Node is missing, the server is skipped with a warning at startup and the rest of Wizard works normally — install Node, then `/reload`.

### A roster of subagents

`~/.wizard/subagents/` ships four specialists the parent model can delegate to with the `spawn_subagent` tool. Each runs in an isolated context with its own step budget and tool scope, and returns a single summary to the parent.

| Subagent | What it does | Tools it can use |
|----------|--------------|------------------|
| `reviewer` | Reviews a diff or files for bugs, security issues, and style; read-only. | read / search / git only |
| `researcher` | Web research via the Playwright browser; gathers facts and reports sources. | full set (incl. browser MCP) |
| `tester` | Runs the test suite, diagnoses failures, fixes code or tests. | read / write / edit / search / execute |
| `documenter` | Writes and updates docs and comments to match the code. | read / write / edit / search |

These are plain TOML files. Edit them, add your own, or delete the ones you don't want — they follow the same format as any [user-defined subagent](evolve.md). Changes take effect on the next run or `/reload`.

```
> use the reviewer subagent to review my staged changes
> have the researcher subagent find the latest Tokio release notes
```

### Hardware-aware model selection

Arsenal uses the same VRAM detection as upstream and writes the selected tier into `config.toml`. See [Model tiers](getting-started.md#model-tiers-automatic) for the table and detection details. Override with `WIZARD_MODEL=<tag>` at install time.

---

## How it differs from vanilla Wizard

| | Vanilla Wizard | Wizard Arsenal |
|---|---|---|
| Binary | Upstream `wizard` | Same upstream `wizard` (no source changes) |
| Browser | Add via `/evolve` when needed | Preconfigured in `mcp.toml` |
| Subagents | One built-in `worker` | `worker` + reviewer / researcher / tester / documenter |
| `config.toml` | `model` / `ollama_host` / `mode` | `[[provider]]` blocks + `active_provider`, plus cloud templates |
| Model selection | VRAM-tiered | VRAM-tiered (identical) |

Everything else — genie/sovereign modes, `/evolve` tiers and gates, `--continuous`, `/publish` — is identical, because it is the same binary.

---

## Providers: local by default, cloud optional

Arsenal's `config.toml` uses Wizard's provider blocks. The default is local Ollama:

```toml
active_provider = "local"

[[provider]]
name = "local"
kind = "ollama"
base_url = "http://127.0.0.1:11434"
model = "qwen3.6:27b"   # whatever tier the installer selected
```

### Adding a provider

The fastest way is `/provider` in the TUI, which adds and switches providers without editing files. To do it by hand, the shipped config carries commented templates for OpenAI and Anthropic — uncomment one and set `active_provider`:

```toml
[[provider]]
name = "anthropic"
kind = "anthropic"
base_url = "https://api.anthropic.com"
model = "claude-fable-5"
api_key_env = "ANTHROPIC_API_KEY"
```

The key is read from the named environment variable (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`) at runtime — it is never written to `config.toml`. Export it in your shell before launching:

```bash
export ANTHROPIC_API_KEY=sk-...
wizard
```

### Removing a provider

Comment it out (or delete its `[[provider]]` block) and make sure `active_provider` points at one that remains, or switch with `/provider`. Drop back to fully local at any time by setting `active_provider = "local"`.

---

## Relationship to upstream

Arsenal is a distribution layer, not a fork of the binary. To change Wizard's *behavior* (new built-in tools, TUI features, an amended charter), deep-evolve upstream Wizard and [publish your own fork](market.md) — that ships modified source. Arsenal is for shipping a richer *configuration* on top of stock Wizard.

Follow upstream at [`teddytennant/wizard`](https://github.com/teddytennant/wizard).

---

## See also

- [Getting started](getting-started.md) — install, tiers, first run
- [Self-extension](evolve.md) — `/evolve`, subagents, MCP servers
- [Fork and distribute](market.md) — publish a modified-source fork
- [WIZARD.md](../WIZARD.md) — the behavioral charter (browser recipe in §2)
