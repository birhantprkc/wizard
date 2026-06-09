# Wizard

**One line. Your sovereign coding wizard. Self-extending. Fully local.**

```bash
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | bash
```

Wizard is a lightweight Rust + Ratatui coding agent that runs entirely on your machine. One command installs the binary, Ollama, and the official **Qwen 3.6** model — then you get an interactive TUI with tool calling, git integration, skills, and `/evolve` self-extension.

No API keys. No cloud. Your code stays yours.

---

## Why Wizard?

Setting up a capable local coding agent on a fresh Linux server usually means:

1. Install Rust or download a binary
2. Install Ollama
3. Pull a model
4. Clone a repo
5. Configure provider keys or endpoints
6. Figure out which model tag actually works

Wizard collapses that into **one line** with sensible defaults: official Qwen 3.6 from the [Ollama library](https://ollama.com/library/qwen3.6), pre-wired for agentic coding.

---

## Quick start

```bash
# Install everything (binary + Ollama + model)
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | bash

# Launch interactive TUI (genie mode — default)
wizard

# Sovereign autonomous mode
wizard --mode sovereign -p "refactor the auth module and add tests"

# Self-extension: agent adds a capability to itself live (skill / tool / MCP)
wizard --evolve -p "add a /status slash command"
```

See [docs/getting-started.md](docs/getting-started.md) for full setup details.

---

## Personality modes

| Mode | Command | Description |
|------|---------|-------------|
| **Genie** | `wizard` | Interactive TUI. Eager, creative, confirms risky actions (writes, shell, git) unless `--auto`. |
| **Sovereign** | `wizard --mode sovereign` | Autonomous background agent. Proactive, long-running, auto-approves tools. Built for autonomous task loops. |

Details: [docs/modes.md](docs/modes.md)

---

## Features (v0.1)

- **One-liner install** — binary, Ollama, and model in a single `curl | bash`
- **Official Qwen 3.6** — `qwen3.6:27b` by default, with VRAM-aware fallbacks
- **Ratatui TUI** — chat UI with tool output, git diff preview, session history
- **Tool calling** — file I/O, shell, git, codebase search
- **MCP client** — plug in external capabilities (computer use, browser, more) with no rebuild
- **Skills** — drop markdown instructions in `skills/`; Wizard loads them into context
- **Self-extension** — `/evolve` adds skills, MCP servers, and scripted tools live (`/reload`); deep source-rebuild lands in v0.2
- **Lightweight** — single binary target < 60 MB (stripped release)

---

## Bring your own model

The default installer uses official Ollama models only. If you want a custom model (local GGUF, private registry, fine-tune):

```bash
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install-byom.sh | bash
```

You choose the model; Wizard wires up the config. See [docs/byom.md](docs/byom.md).

---

## Configuration

Config lives at `~/.wizard/config.toml`:

```toml
model = "qwen3.6:27b"
ollama_host = "http://127.0.0.1:11434"
mode = "genie"          # genie | sovereign
auto_approve = false    # skip confirmation prompts in genie mode
max_steps = 25          # agent loop limit (genie)
```

Sessions are stored in `~/.wizard/sessions/`. Evolution history in `~/.wizard/evolution.jsonl`.

---

## Slash commands (TUI)

| Command | Description |
|---------|-------------|
| `/help` | Show available commands |
| `/clear` | Clear conversation |
| `/model` | Show or switch model |
| `/mode` | Switch genie ↔ sovereign |
| `/genie` / `/sovereign` | Switch mode directly |
| `/evolve` | Self-extension: add skills, MCP servers, scripted tools (`--deep` rebuilds core) |
| `/reload` | Reload skills, tools, and MCP servers without restart |
| `/diff` | Show git diff sidebar |
| `/quit` | Exit |

---

## Architecture

```
install.sh  →  Ollama + qwen3.6  →  wizard binary  →  ~/.wizard/config.toml
                                              ↓
                                    ratatui TUI + agent loop
                                              ↓
                      tools (file, shell, git) + MCP + skills + subagents + /evolve
```

Full breakdown: [docs/architecture.md](docs/architecture.md)

---

## Requirements

- **OS:** Linux x86_64 or aarch64 (macOS planned v0.2)
- **RAM/VRAM:** 8 GB minimum; 18 GB+ recommended for `qwen3.6:27b`
- **Dependencies:** Ollama (installed automatically), `git`, `ripgrep` (optional, falls back to grep)

---

## Roadmap

| Version | Focus |
|---------|-------|
| **v0.1** | One-liner + TUI + genie/sovereign modes + MCP client + skills + scripted tools + runtime `/evolve` |
| **v0.2** | Subagent swarms + deep `/evolve` (source rebuild) + plugin marketplace |
| **v0.3** | `ollama launch wizard` integration |

---

## Development

Wizard is built with Rust 2024, Ratatui, and Tokio. Ollama is the only LLM backend in v0.1.

```bash
git clone git@github.com:teddytennant/wizard.git
cd wizard
cargo build --release
./target/release/wizard
```

---

## License

MIT — see [LICENSE](LICENSE).

---

## Author

Teddy Tennant — [github.com/teddytennant](https://github.com/teddytennant)