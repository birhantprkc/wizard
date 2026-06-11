# Getting started

Wizard installs in one command and launches as a terminal UI agent. The default install puts down the binary and the [default loadout](loadout.md) — no model, no config — and the first `wizard` run opens [onboarding](#first-run) to pick a provider. Local is one pick: Wizard detects your hardware, downloads a fitting GGUF, and sets up [llama.cpp](https://github.com/ggml-org/llama.cpp)'s `llama-server` itself (or reuses an existing Ollama install), so no API key is needed. Or bring a key for any OpenAI-compatible endpoint (OpenAI, OpenRouter, Groq, vLLM, LM Studio, llama.cpp, Ollama), Anthropic, or xAI (API key or account sign-in). See [Using a cloud or remote provider](#using-a-cloud-or-remote-provider) and [Using Ollama instead](#using-ollama-instead).

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | bash
```

The installer:

1. Detects your OS and CPU architecture
2. Downloads the `wizard` binary from GitHub releases and verifies its SHA-256 against the release's `checksums.txt` (a mismatch aborts the install)
3. Lays down the [default loadout](loadout.md): `~/.wizard/mcp.toml` (Playwright browser MCP) and `~/.wizard/subagents/*.toml` (reviewer, researcher, tester, documenter), each file only if it is not already present

It installs no model and writes no config; the first `wizard` run starts onboarding. To preinstall the local stack instead (non-interactive; what the default used to do), set `WIZARD_LOCAL=1`:

```bash
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | WIZARD_LOCAL=1 bash
```

With `WIZARD_LOCAL=1` the installer additionally:

1. Installs `llama-server` from official llama.cpp GitHub releases if it is not already on your `PATH` (Vulkan build when a GPU and Vulkan loader are present, CPU build otherwise; the release tree lands in `~/.wizard/llama.cpp/` with a symlink at `~/.wizard/bin/llama-server`)
2. Selects a model tier based on available VRAM
3. Downloads the matching Qwen 3 GGUF (Q4_K_M) from Hugging Face into `~/.wizard/models/` (resumable; re-running picks up where it left off)
4. Writes `~/.wizard/config.toml` (an existing config is never touched)

The installer does **not** start a model server; Wizard starts `llama-server` itself on first run.

### Install flavors

The same script has four mutually exclusive flavors:

| Install | What you get |
|---------|--------------|
| (default) | binary + loadout; no model, no config — the first `wizard` run starts [onboarding](#first-run) |
| `WIZARD_LOCAL=1` | the default plus a preinstalled local stack: llama.cpp runtime + VRAM-tiered Qwen GGUF + `config.toml` |
| `WIZARD_MINIMAL=1` | binary only: no loadout either; onboarding on first run as with the default |
| `WIZARD_BYOM=1` | Ollama runtime + a model of your choice (interactive, or `WIZARD_MODEL=<tag>`) + binary + config + loadout; see [byom.md](byom.md) |

`WIZARD_USE_OLLAMA=1` is the Ollama variant of the local flavor (installs Ollama, starts it, pulls the same auto-tiered model) and implies it — no need to also set `WIZARD_LOCAL`. Combining `WIZARD_LOCAL`, `WIZARD_MINIMAL`, or `WIZARD_BYOM` is an error. `WIZARD_BESPOKE=1` is a deprecated alias for `WIZARD_MINIMAL=1`; note it is stricter than the old bespoke flavor, which still installed the model runtime. Minimal installs nothing but the binary and leaves everything to onboarding.

### Model tiers (automatic)

Picking Local in onboarding and the `WIZARD_LOCAL=1` / `WIZARD_USE_OLLAMA=1` flavors size the model to your hardware:

| Available VRAM | GGUF downloaded | Approx. size |
|----------------|-----------------|--------------|
| ≥ 24 GB | `Qwen3.6-35B-A3B-UD-Q4_K_M.gguf` | ~21 GB (MoE, 36B total / 3B active: fast, but all weights must fit in memory) |
| 18–24 GB | `Qwen3.6-27B-Q4_K_M.gguf` | ~16 GB (dense) |
| 8–18 GB | `Qwen3.5-9B-Q4_K_M.gguf` | ~6 GB |
| < 8 GB | `Qwen3.5-9B-Q4_K_M.gguf` (CPU / partial offload, slower) | ~6 GB |

Tiers are ordered so the model's total footprint fits in available memory. An MoE model still needs all expert weights resident, which is why the 35B lands in the top tier despite its small active-parameter count.

VRAM detection uses `nvidia-smi` for NVIDIA and `rocm-smi` for AMD, falling back to the amdgpu sysfs counter (`/sys/class/drm/card*/device/mem_info_vram_total`). On CPU-only systems, total system RAM is used as a heuristic. If nothing can be detected, the installer warns and falls back to the smallest tier; override with `WIZARD_MODEL=<tag>`.

### Installer environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `WIZARD_INSTALL_DIR` | `/usr/local/bin` | Where to place the `wizard` binary |
| `WIZARD_LOCAL` | `0` | Set to `1` to preinstall the llama.cpp stack and an auto-tiered model (conflicts with `WIZARD_MINIMAL` and `WIZARD_BYOM`) |
| `WIZARD_MINIMAL` | `0` | Set to `1` for the binary-only install; first run launches onboarding |
| `WIZARD_BYOM` | `0` | Set to `1` to bring your own Ollama model (conflicts with `WIZARD_MINIMAL`) |
| `WIZARD_BESPOKE` | `0` | Deprecated alias for `WIZARD_MINIMAL` |
| `WIZARD_MODEL` | auto-detected | Local flavors: force a model tier (`qwen3.6:35b`, `qwen3.6:27b`, `qwen3.5:9b`); with `WIZARD_BYOM=1`, use this tag as-is and skip the interactive prompts |
| `WIZARD_SKIP_MODEL_PULL` | `0` | Local flavors: set to `1` to skip the model download |
| `WIZARD_SKIP_LLAMACPP_INSTALL` | `0` | With `WIZARD_LOCAL=1`: set to `1` if `llama-server` is managed elsewhere |
| `WIZARD_USE_OLLAMA` | `0` | Set to `1` for the Ollama variant of the local flavor (implies `WIZARD_LOCAL`) |
| `WIZARD_SKIP_OLLAMA_INSTALL` | `0` | With Ollama flavors: Ollama is already managed elsewhere |
| `WIZARD_WITH_TOOLCHAIN` | `0` | Set to `1` to eagerly install a Rust toolchain for deep evolve |

### Runtime environment variables

These override `~/.wizard/config.toml` for a single run:

| Variable | Description |
|----------|-------------|
| `WIZARD_MODEL` | Override the model tag |
| `WIZARD_LLAMACPP_HOST` | Override the llama-server URL (default `http://127.0.0.1:8080`) |
| `WIZARD_GGUF_PATH` | Override the GGUF file Wizard uses when it starts `llama-server` |
| `WIZARD_OLLAMA_HOST` | Point Wizard at an Ollama host (also switches the synthesized local provider to Ollama) |

## First run

```bash
wizard
```

With no config present (the default and minimal installs), the first launch opens onboarding: a Ratatui wizard that asks which provider to use (provider, model, messaging gateway, mode) and writes `~/.wizard/config.toml`. Picking Local is one step — Wizard detects your hardware, downloads a GGUF sized to it, and installs and starts `llama-server` itself (or reuses an existing Ollama install). The other options take an API key: OpenRouter, xAI (Grok), OpenAI, Anthropic, or any OpenAI-compatible endpoint. Re-run it any time with `wizard --onboard`.

With a config present (after onboarding, or a `WIZARD_LOCAL=1` install), launching Wizard with a local llama.cpp provider:

- Probes `llama-server`'s health endpoint (`GET http://127.0.0.1:8080/health`)
- If nothing answers, starts `llama-server` itself with your GGUF and waits (up to 60 s) for the model to load
- Opens the Ratatui interface in genie mode

The server Wizard starts is detached: it keeps serving after Wizard exits, so the next launch connects instantly. Its output goes to `~/.wizard/llama-server.log`, and its PID is recorded in `~/.wizard/llama-server.pid` so `/server stop` never kills anything else.

Auto-start requires three things: the provider's `base_url` points at this machine, `llama-server` is on `PATH`, and the provider's `gguf_path` names an existing file. Otherwise Wizard prints exactly what to run by hand (`llama-server -m <model.gguf> --port 8080`).

Manage the server from the TUI:

```
/server status   # ready / loading its model / not running
/server start    # start llama-server for the active provider
/server stop     # stop the server Wizard started (refuses anything else)
```

Type a task in natural language:

```
> Add error handling to the fetch_user function in src/api.rs
```

Wizard reads files, applies changes, runs tests, and shows git diffs.

## Configuration

`~/.wizard/config.toml` as written by onboarding's Local pick (or a `WIZARD_LOCAL=1` install):

```toml
active_provider = "local"
mode = "genie"
auto_approve = false
max_steps = 25

[[providers]]
name = "local"
kind = "llamacpp"
base_url = "http://127.0.0.1:8080"
model = "Qwen3.6-27B-Q4_K_M"
gguf_path = "/home/you/.wizard/models/Qwen3.6-27B-Q4_K_M.gguf"
```

`gguf_path` is what lets Wizard start `llama-server` for you; without it (e.g. a server you run yourself, or on another machine) Wizard just connects to `base_url`. `gguf_path` only applies to `kind = "llamacpp"` providers, which never use an API key.

The installer also lays down `~/.wizard/mcp.toml` (Playwright browser MCP) and `~/.wizard/subagents/` (a four-subagent roster), each file only if absent; see [the default loadout](loadout.md).

### Spinner verbs (`[ui]`)

While Wizard works, the chat-area spinner shows a wizard-flavored verb ("Conjuring…", "Scrying…", "Brewing…"): one is picked pseudo-randomly per busy period and held until the turn finishes, and the next turn draws a new one. Customize the list with an optional `[ui]` section:

```toml
[ui]
spinner_verbs = ["Pondering", "Musing", "Noodling"]
```

A non-empty list fully replaces the defaults; omitting the section or setting `spinner_verbs = []` keeps the built-in wizard verbs. The status bar (`step x/y · Ns`) and tool spinners are unaffected.

## Migrating from Ollama

The local default is llama.cpp; Ollama stays fully supported but is opt-in:

- Explicit `[[providers]]` entries with `kind = "ollama"` behave exactly as before.
- A legacy config that only sets top-level `model` / `ollama_host` now resolves to llama.cpp at `http://127.0.0.1:8080`; add an explicit Ollama provider (`/provider add local ollama http://127.0.0.1:11434 <model>`) to stay on Ollama.
- If the local backend isn't installed or can't start, Wizard falls back to bring-your-own-provider: any configured cloud provider, then `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` / `XAI_API_KEY` / `OPENROUTER_API_KEY` from the environment, then interactive setup.

To switch an existing install to llama.cpp, add a provider from the TUI and point it at a GGUF:

```
/provider add local-llamacpp llamacpp http://127.0.0.1:8080 Qwen3.6-27B-Q4_K_M
/provider use local-llamacpp
```

Then set `gguf_path` on that provider in `~/.wizard/config.toml` so Wizard can start the server for you. Or simply re-run onboarding: `wizard --onboard`.

## Using Ollama instead

Install with the Ollama flavor (installs Ollama, starts it, pulls the auto-tiered model):

```bash
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | WIZARD_USE_OLLAMA=1 bash
```

Or pick the local Ollama option in onboarding (`wizard --onboard`). Wizard speaks Ollama's native `/api/chat` for these providers, as before.

## Using a cloud or remote provider

Any OpenAI-compatible endpoint, OpenRouter, Anthropic, or xAI works. Add one from the TUI and switch to it:

```
/provider add openai openai https://api.openai.com/v1 gpt-4o OPENAI_API_KEY
/provider add xai xai https://api.x.ai/v1 grok-4.3 XAI_API_KEY
/provider use openai
```

The last argument names the environment variable holding your API key; the key itself is never written to disk. Export it before launching (`export OPENAI_API_KEY=sk-...`). Onboarding offers the same choices interactively — the default install puts down no local stack, so picking a cloud provider on first run is all there is to it.

### Using OpenRouter

OpenRouter serves hundreds of hosted models behind one OpenAI-compatible endpoint and one API key:

```
/provider add openrouter openrouter https://openrouter.ai/api/v1 openrouter/auto OPENROUTER_API_KEY
/provider use openrouter
```

`openrouter/auto` is OpenRouter's Auto Router, which picks a model per prompt; any `vendor/model` tag from openrouter.ai/models works instead. Wizard sends OpenRouter's recommended attribution headers (`HTTP-Referer`, `X-Title`) on every request.

### Signing in with an xAI account

You can use xAI without an API key by signing in with your xAI account (OAuth 2.0 with PKCE):

```bash
wizard --login xai     # or /login xai from inside the TUI
```

Wizard opens your browser, captures the callback on localhost, and stores the tokens in `~/.wizard/xai_oauth.json` (file mode 0600); the access token is refreshed automatically. Then add the provider:

```
/provider add xai xaioauth https://api.x.ai/v1 grok-4.3
/provider use xai
```

Note: xAI gates OAuth API access to certain SuperGrok plans. If requests come back with HTTP 403, use the API-key flavor (`kind = "xai"` with `XAI_API_KEY`) instead.

## Headless mode

Run a single task without the TUI:

```bash
wizard -p "find all TODO comments and list them by file"
```

Combine with sovereign mode for autonomous execution:

```bash
wizard --mode sovereign -p "implement JWT refresh tokens" --auto
```

## Working in a project

`cd` into your repository before launching Wizard. It uses the current working directory as the project root.

For best results, add an `AGENTS.md` (or `WIZARD.md`) at the repo root with:

- Stack and versions
- Build and test commands
- Code style rules
- Directories that must not be edited

Example:

```markdown
# Agent Instructions

## Commands
- Build: `cargo build`
- Test: `cargo test`
- Lint: `cargo clippy`

## Rules
- Prefer minimal diffs
- Run tests after every change
- Do not edit generated files
```

## Updating

Re-run the installer to get the latest binary (an existing `~/.wizard/config.toml` is left untouched):

```bash
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | bash
```

To change models, download a GGUF into `~/.wizard/models/` (Hugging Face hosts Q4_K_M quants of most open models), update `model` and `gguf_path` in `~/.wizard/config.toml`, then `/server stop` and `/server start` (or restart Wizard).

## Troubleshooting

### llama-server won't start

Check the log first:

```bash
tail -50 ~/.wizard/llama-server.log
```

Common causes: the GGUF at `gguf_path` is missing or truncated (re-run the installer with `WIZARD_LOCAL=1`; the download resumes), or the model doesn't fit in memory (see below).

### llama-server not found

Wizard looks for `llama-server` on `PATH`. The local setup (onboarding's Local pick, or a `WIZARD_LOCAL=1` install) links it into the install dir and `~/.wizard/bin/`; if neither is on your `PATH`, add one, or install llama.cpp yourself:

```bash
brew install llama.cpp                  # Homebrew / Linuxbrew
nix profile install nixpkgs#llama-cpp   # Nix / NixOS
```

### Server status says "loading its model"

GGUF loads take a while (tens of seconds for the larger tiers). `/server status` shows `ready` when it's done; Wizard waits up to 60 s automatically on startup.

### Out of memory

Switch to a smaller tier: download `Qwen3.5-9B-Q4_K_M.gguf` (~6 GB) from Hugging Face into `~/.wizard/models/`, then update the provider entry in `~/.wizard/config.toml`:

```toml
[[providers]]
name = "local"
kind = "llamacpp"
base_url = "http://127.0.0.1:8080"
model = "Qwen3.5-9B-Q4_K_M"
gguf_path = "/home/you/.wizard/models/Qwen3.5-9B-Q4_K_M.gguf"
```

### Ollama not running (Ollama providers)

```bash
ollama serve
# or on systemd:
sudo systemctl start ollama
```

### Check Wizard logs

Logs are written to `~/.wizard/logs/` when tracing is enabled:

```bash
RUST_LOG=wizard=debug wizard
```

## Next steps

- [The default loadout](loadout.md): the preconfigured browser MCP and subagent roster
- [Personality modes](modes.md): genie vs sovereign
- [Self-extension](evolve.md): how `/evolve` adds capabilities
- [Bring your own model](byom.md): any GGUF, or custom Ollama models
- [Architecture](architecture.md): how Wizard works under the hood
