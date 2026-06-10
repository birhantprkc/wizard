# Getting started

Wizard installs in one command and launches as a terminal UI agent powered by [llama.cpp](https://github.com/ggml-org/llama.cpp)'s `llama-server` and a local Qwen 3 GGUF. Ollama remains fully supported — see [Using Ollama instead](#using-ollama-instead).

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | bash
```

The installer:

1. Detects your OS and CPU architecture
2. Installs `llama-server` from official llama.cpp GitHub releases if it is not already on your `PATH` (Vulkan build when a GPU and Vulkan loader are present, CPU build otherwise; the release tree lands in `~/.wizard/llama.cpp/` with a symlink at `~/.wizard/bin/llama-server`)
3. Selects a model tier based on available VRAM
4. Downloads the matching Qwen 3 GGUF (Q4_K_M) from Hugging Face into `~/.wizard/models/` (resumable; re-running picks up where it left off)
5. Downloads the `wizard` binary from GitHub releases and verifies its SHA-256 against the release's `checksums.txt` (a mismatch aborts the install)
6. Writes `~/.wizard/config.toml` (an existing config is never touched)

The installer does **not** start a model server — Wizard starts `llama-server` itself on first run.

### Model tiers (automatic)

| Available VRAM | GGUF downloaded | Approx. size |
|----------------|-----------------|--------------|
| ≥ 24 GB | `Qwen3.6-35B-A3B-UD-Q4_K_M.gguf` | ~21 GB (MoE, 36B total / 3B active — fast, but all weights must fit in memory) |
| 18–24 GB | `Qwen3.6-27B-Q4_K_M.gguf` | ~16 GB (dense) |
| 8–18 GB | `Qwen3.5-9B-Q4_K_M.gguf` | ~6 GB |
| < 8 GB | `Qwen3.5-9B-Q4_K_M.gguf` (CPU / partial offload — slower) | ~6 GB |

Tiers are ordered so the model's total footprint fits in available memory. An MoE model still needs all expert weights resident, which is why the 35B lands in the top tier despite its small active-parameter count.

VRAM detection uses `nvidia-smi` for NVIDIA and `rocm-smi` for AMD, falling back to the amdgpu sysfs counter (`/sys/class/drm/card*/device/mem_info_vram_total`). On CPU-only systems, total system RAM is used as a heuristic. If nothing can be detected, the installer warns and falls back to the smallest tier; override with `WIZARD_MODEL=<tag>`.

### Installer environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `WIZARD_INSTALL_DIR` | `/usr/local/bin` | Where to place the `wizard` binary |
| `WIZARD_MODEL` | auto-detected | Force a model tier (`qwen3.6:35b`, `qwen3.6:27b`, `qwen3.5:9b`) |
| `WIZARD_SKIP_MODEL_PULL` | `0` | Set to `1` to skip the GGUF download |
| `WIZARD_SKIP_LLAMACPP_INSTALL` | `0` | Set to `1` if `llama-server` is managed elsewhere |
| `WIZARD_USE_OLLAMA` | `0` | Set to `1` for the previous Ollama-based flow |
| `WIZARD_SKIP_OLLAMA_INSTALL` | `0` | With `WIZARD_USE_OLLAMA=1`: Ollama is already managed elsewhere |
| `WIZARD_WITH_TOOLCHAIN` | `0` | Set to `1` to eagerly install a Rust toolchain for deep evolve |
| `WIZARD_BESPOKE` | `0` | Set to `1` to skip config and model download; first run launches onboarding |

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

On first launch, Wizard:

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

Wizard reads files, applies changes, runs tests, and shows git diffs. Everything runs locally.

## Configuration

`~/.wizard/config.toml` written by the installer:

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

## Migrating from Ollama

Existing installs keep working without changes:

- A legacy config that sets top-level `model` / `ollama_host` still resolves to an Ollama provider — only the from-scratch default changed.
- Explicit `[[providers]]` entries with `kind = "ollama"` behave exactly as before.
- `WIZARD_OLLAMA_HOST` still works and opts the local provider back into Ollama.

To switch an existing install to llama.cpp, add a provider from the TUI and point it at a GGUF:

```
/provider add local-llamacpp llamacpp http://127.0.0.1:8080 Qwen3.6-27B-Q4_K_M
/provider use local-llamacpp
```

Then set `gguf_path` on that provider in `~/.wizard/config.toml` so Wizard can start the server for you. Or simply re-run onboarding: `wizard --onboard`.

## Using Ollama instead

Install with the previous flow (installs Ollama, starts it, pulls the model):

```bash
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | WIZARD_USE_OLLAMA=1 bash
```

Or pick "Local — Ollama" in onboarding (`wizard --onboard`). Wizard speaks Ollama's native `/api/chat` for these providers, as before.

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

Common causes: the GGUF at `gguf_path` is missing or truncated (re-run the installer — the download resumes), or the model doesn't fit in memory (see below).

### llama-server not found

Wizard looks for `llama-server` on `PATH`. The installer links it into the install dir and `~/.wizard/bin/`; if neither is on your `PATH`, add one, or install llama.cpp yourself:

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

- [Personality modes](modes.md) — genie vs sovereign
- [Self-extension](evolve.md) — how `/evolve` adds capabilities
- [Bring your own model](byom.md) — any GGUF, or custom Ollama models
- [Architecture](architecture.md) — how Wizard works under the hood
