# Getting started

Wizard installs in one command and launches as a terminal UI coding agent powered by local Ollama and official Qwen 3.6.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | bash
```

The installer:

1. Detects your OS and CPU architecture
2. Installs Ollama if it is not already present
3. Starts the Ollama server if needed
4. Selects a model tier based on available VRAM
5. Pulls the model from the official Ollama library
6. Downloads the `wizard` binary from GitHub releases
7. Writes `~/.wizard/config.toml`

### Model tiers (automatic)

| Available VRAM | Model pulled | Approx. size |
|----------------|--------------|--------------|
| ≥ 18 GB | `qwen3.6:27b` | ~17 GB |
| 8–18 GB | `qwen3.6:35b` | ~24 GB (MoE, active ~3B params) |
| < 8 GB | `qwen3.5:9b` | ~6 GB |

VRAM is detected via `nvidia-smi` when a GPU is present. On CPU-only systems, the installer uses available system RAM as a heuristic.

### Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `WIZARD_INSTALL_DIR` | `/usr/local/bin` | Where to place the `wizard` binary |
| `WIZARD_MODEL` | auto-detected | Force a specific Ollama model tag |
| `WIZARD_SKIP_MODEL_PULL` | `0` | Set to `1` to skip `ollama pull` |
| `WIZARD_SKIP_OLLAMA_INSTALL` | `0` | Set to `1` if Ollama is already managed elsewhere |

## First run

```bash
wizard
```

On first launch, Wizard:

- Verifies Ollama is reachable at `http://127.0.0.1:11434`
- Confirms the configured model is available (`ollama list`)
- Opens the Ratatui interface in **genie mode**

Type a task in natural language:

```
> Add error handling to the fetch_user function in src/api.rs
```

Wizard reads files, proposes changes, runs tests, and shows git diffs — all locally.

## Headless mode

Run a single task without the TUI:

```bash
wizard -p "find all TODO comments and list them by file"
```

Combine with wizard mode for autonomous execution:

```bash
wizard --mode wizard -p "implement JWT refresh tokens" --auto
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

Re-run the installer to get the latest binary and refresh config:

```bash
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | bash
```

To update only the model:

```bash
ollama pull qwen3.6:27b
```

## Troubleshooting

### Ollama not running

```bash
ollama serve
# or on systemd:
sudo systemctl start ollama
```

### Model not found

```bash
ollama pull qwen3.6:27b
```

### Out of memory

Switch to a smaller tier manually:

```bash
ollama pull qwen3.6:35b   # MoE — lower active memory
# or
ollama pull qwen3.5:9b
```

Then update `~/.wizard/config.toml`:

```toml
model = "qwen3.6:35b"
```

### Check Wizard logs

Logs are written to `~/.wizard/logs/` when tracing is enabled:

```bash
RUST_LOG=wizard=debug wizard
```

## Next steps

- [Personality modes](modes.md) — genie vs wizard
- [Bring your own model](byom.md) — custom Ollama models
- [Architecture](architecture.md) — how Wizard works under the hood