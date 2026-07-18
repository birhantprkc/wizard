# Getting started

Wizard installs in one command and launches as a terminal UI agent. The default install puts down the binary and the [default loadout](loadout.md) (no model, no config); the first `wizard` run opens [onboarding](#first-run) to pick a provider. Local is one pick: Wizard detects your hardware, downloads a fitting GGUF, and sets up [llama.cpp](https://github.com/ggml-org/llama.cpp)'s `llama-server` itself (or reuses an existing Ollama install), so no API key is needed. Or bring a key for any OpenAI-compatible endpoint (OpenAI, OpenRouter, Cloudflare Workers AI, Groq, vLLM, LM Studio, llama.cpp, Ollama), Anthropic, or xAI (API key or account sign-in). See [Using a cloud or remote provider](#using-a-cloud-or-remote-provider) and [Using Ollama instead](#using-ollama-instead).

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | bash
```

The installer:

1. Detects your OS and CPU architecture
2. Downloads the `wizard` binary from GitHub releases and verifies its SHA-256 against the release's `checksums.txt` (a mismatch aborts the install)
3. Lays down the [default loadout](loadout.md): `~/.wizard/mcp.toml` (Playwright browser MCP) and `~/.wizard/subagents/*.toml` (reviewer, researcher, tester, documenter), each file only if it is not already present

It installs no model and writes no config; the first `wizard` run starts onboarding. To preinstall the local stack instead (non-interactive), set `WIZARD_LOCAL=1`:

```bash
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | WIZARD_LOCAL=1 bash
```

With `WIZARD_LOCAL=1` the installer additionally:

1. Installs `llama-server` if it is not already on your `PATH`: on an NVIDIA GPU with `nvcc` present it compiles a CUDA build from source (llama.cpp publishes no Linux CUDA binary; skip with `WIZARD_LLAMACPP_NO_CUDA=1`), otherwise it downloads an official llama.cpp release (Vulkan build when a GPU and Vulkan loader are present, CPU build as the fallback). Either way the install lands in `~/.wizard/llama.cpp/` with a symlink at `~/.wizard/bin/llama-server`
2. Selects a model tier based on available VRAM
3. Downloads the matching Qwen 3 GGUF (Q4_K_M) from Hugging Face into `~/.wizard/models/` (resumable; re-running picks up where it left off)
4. Writes `~/.wizard/config.toml` (an existing config is never touched)

The installer does **not** start a model server; Wizard starts `llama-server` itself on first run.

### Install flavors

The same script has four mutually exclusive flavors:

| Install | What you get |
|---------|--------------|
| (default) | binary + loadout; no model, no config. The first `wizard` run starts [onboarding](#first-run) |
| `WIZARD_LOCAL=1` | the default plus a preinstalled local stack: llama.cpp runtime + VRAM-tiered Qwen GGUF + `config.toml` |
| `WIZARD_MINIMAL=1` | binary only: no loadout either; onboarding on first run as with the default |
| `WIZARD_BYOM=1` | Ollama runtime + binary + loadout; model choice happens in onboarding, which pulls the tag you pick on first run (or set `WIZARD_MODEL=<tag>` to pull + write the config headlessly); see [byom.md](byom.md) |

`WIZARD_USE_OLLAMA=1` is the Ollama variant of the local flavor (installs Ollama, starts it, pulls the same auto-tiered model) and implies it: no need to also set `WIZARD_LOCAL`. Combining `WIZARD_LOCAL`, `WIZARD_MINIMAL`, or `WIZARD_BYOM` is an error. `WIZARD_BESPOKE=1` is a deprecated alias for `WIZARD_MINIMAL=1`; it's stricter than the old bespoke flavor, which still installed the model runtime. Minimal installs nothing but the binary and leaves everything to onboarding.

### Platforms

| Platform | Notes |
|----------|-------|
| Linux x86_64 / aarch64 | Prebuilt glibc and static-musl binaries; the installer prefers musl on NixOS |
| macOS Apple Silicon / Intel | Same `curl … \| bash`; prebuilt binaries for both architectures; Metal-backed `llama-server` for the local stack |
| Windows | Not supported natively; use WSL2 |

The installer downloads the prebuilt binary matching your OS and architecture, verifies its checksum, and falls back to a source build when no prebuilt asset is available.

### Nix / NixOS

Wizard ships a flake, so on Nix you don't need the install script at all:

```bash
nix run github:teddytennant/wizard              # run without installing
nix profile install github:teddytennant/wizard  # add to your profile
```

The flake exposes `packages.default` (and `.wizard`), `apps.default`, `devShells.default` (Rust toolchain + `llama-cpp` for hacking on Wizard), `overlays.default`, and `homeManagerModules.default` for wiring it into a Home Manager config. On NixOS the curl installer detects the system, points you at these commands, and, if you run it anyway, installs the static musl binary into `~/.local/bin` rather than `/usr/local/bin` (which isn't on the FHS path there).

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
| `WIZARD_INSTALL_DIR` | `/usr/local/bin` (`~/.local/bin` on NixOS) | Where to place the `wizard` binary |
| `WIZARD_VERSION` | latest release | Release tag to install, e.g. `v0.4.0`; pin it for reproducible installs or to roll back to an earlier release |
| `WIZARD_LOCAL` | `0` | Set to `1` to preinstall the llama.cpp stack and an auto-tiered model (conflicts with `WIZARD_MINIMAL` and `WIZARD_BYOM`) |
| `WIZARD_MINIMAL` | `0` | Set to `1` for the binary-only install; first run launches onboarding |
| `WIZARD_BYOM` | `0` | Set to `1` to set up Ollama and bring your own model, picked in onboarding unless `WIZARD_MODEL` is set (conflicts with `WIZARD_MINIMAL` and `WIZARD_LOCAL`) |
| `WIZARD_BESPOKE` | `0` | Deprecated alias for `WIZARD_MINIMAL` |
| `WIZARD_MODEL` | auto-detected | Local flavors: force a model tier (`qwen3.6:35b`, `qwen3.6:27b`, `qwen3.5:9b`); with `WIZARD_BYOM=1`, pull this tag and write the config instead of deferring to onboarding |
| `WIZARD_SKIP_MODEL_PULL` | `0` | Local flavors: set to `1` to skip the model download |
| `WIZARD_SKIP_LLAMACPP_INSTALL` | `0` | With `WIZARD_LOCAL=1`: set to `1` if `llama-server` is managed elsewhere |
| `WIZARD_LLAMACPP_NO_CUDA` | `0` | Set to `1` to never compile a CUDA `llama-server`; use the prebuilt Vulkan/CPU build instead |
| `WIZARD_USE_OLLAMA` | `0` | Set to `1` for the Ollama variant of the local flavor (implies `WIZARD_LOCAL`) |
| `WIZARD_SKIP_OLLAMA_INSTALL` | `0` | With Ollama flavors: Ollama is already managed elsewhere |
| `WIZARD_WITH_TOOLCHAIN` | `0` | Set to `1` to eagerly install a Rust toolchain for deep evolve |
| `WIZARD_REPO` | `teddytennant/wizard` | `owner/repo` to install from: how a published fork ships itself |
| `WIZARD_REF` | latest release tag | Git ref/tag when building from source (falls back to `main` only when the repo has no release) |
| `WIZARD_BUILD_FROM_SOURCE` | `0` | Set to `1` to build from source instead of downloading a release binary |

### Runtime environment variables

These override `~/.wizard/config.toml` for a single run:

| Variable | Description |
|----------|-------------|
| `WIZARD_MODEL` | Override the model tag |
| `WIZARD_LLAMACPP_HOST` | Override the llama-server URL (default `http://127.0.0.1:11435`) |
| `WIZARD_GGUF_PATH` | Override the GGUF file Wizard uses when it starts `llama-server` |
| `WIZARD_OLLAMA_HOST` | Override `ollama_host` for explicitly configured Ollama providers (does not change the synthesized local default, which stays llama.cpp) |

## First run

```bash
wizard
```

With no config present (the default and minimal installs), the first launch opens onboarding: a Ratatui wizard that asks which provider to use (provider, model, messaging gateway, mode) and writes `~/.wizard/config.toml`. Picking Local is one step: Wizard detects your hardware, downloads a GGUF sized to it, and installs and starts `llama-server` itself (or reuses an existing Ollama install). The other options take an API key: OpenRouter, Cloudflare Workers AI (GLM 5.2), xAI (Grok), OpenAI, Anthropic, or any OpenAI-compatible endpoint. Alongside them sit two BYOM picks, llama.cpp (your own GGUF and server URL) and Ollama (any model tag, installed models are listed, and a missing tag is pulled automatically on first run), for bringing your own model. Re-run it any time with `wizard --onboard`.

With a config present (after onboarding, or a `WIZARD_LOCAL=1` install), launching Wizard with a local llama.cpp provider:

- Probes `llama-server`'s health endpoint (`GET http://127.0.0.1:11435/health`)
- If nothing answers, starts `llama-server` itself with your GGUF and waits (up to 60 s) for the model to load
- Opens the Ratatui interface in genie mode

The server Wizard starts is detached: it keeps serving after Wizard exits, so the next launch connects instantly. Its output goes to `~/.wizard/llama-server.log`, and its PID is recorded in `~/.wizard/llama-server.pid` so `/server stop` never kills anything else.

Auto-start requires three things: the provider's `base_url` points at this machine, `llama-server` is on `PATH`, and the provider's `gguf_path` names an existing file. Otherwise Wizard prints exactly what to run by hand (`llama-server -m <model.gguf> --port 11435`).

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

**Enter** sends the message; **Shift+Enter** inserts a newline for multi-line prompts (the composer grows to fit, then scrolls). Shift+Enter needs a terminal that supports the keyboard-enhancement protocol. Wizard enables it on launch when available; where it isn't, **Alt+Enter** does the same thing. Pressing Enter while a turn is already running queues the message, it lands in the transcript and runs automatically when the current turn finishes (see [Queued user messages](usage.md#queued-user-messages)).

## Configuration

`~/.wizard/config.toml` as written by onboarding's Local pick (or a `WIZARD_LOCAL=1` install):

```toml
active_provider = "local"
mode = "genie"
# 0 = no step limit: a turn runs until the model stops calling tools.
max_steps = 0

[[providers]]
name = "local"
kind = "llamacpp"
base_url = "http://127.0.0.1:11435"
model = "Qwen3.6-27B-Q4_K_M"
gguf_path = "/home/you/.wizard/models/Qwen3.6-27B-Q4_K_M.gguf"
```

`gguf_path` is what lets Wizard start `llama-server` for you; without it (e.g. a server you run yourself, or on another machine) Wizard just connects to `base_url`. `gguf_path` only applies to `kind = "llamacpp"` providers, which never use an API key.

`max_steps` bounds one turn (a step is one model → tool → model round trip). `0`, the default, means no limit: the turn ends when the model stops calling tools. An interrupt (Esc), the `--max-hours` limit, and the circuit breaker still end a turn. Set a positive number to cap it instead; the turn then stops when the budget runs out and Wizard says so.

The installer also lays down `~/.wizard/mcp.toml` (Playwright browser MCP) and `~/.wizard/subagents/` (a four-subagent roster), each file only if absent; see [the default loadout](loadout.md). To move this state (config, skills, commands, subagents, scripted tools) to another machine, see [Sync](sync.md).

### Spinner verbs (`[ui]`)

While Wizard works, the chat-area spinner shows a wizard-flavored verb ("Conjuring…", "Scrying…", "Brewing…"): one is picked pseudo-randomly per busy period and held until the turn finishes, and the next turn draws a new one. Customize the list with an optional `[ui]` section:

```toml
[ui]
spinner_verbs = ["Pondering", "Musing", "Noodling"]
```

A non-empty list fully replaces the defaults; omitting the section or setting `spinner_verbs = []` keeps the built-in wizard verbs. The status bar (`step x · Ns`, or `step x/y` under a capped `max_steps`) and tool spinners are unaffected.

### Vim mode (`[ui]`)

Modal (vim-style) editing for the input line, like Claude Code's. Toggle it live with `/vim` (or `/settings → Vim mode`), or set it as the default:

```toml
[ui]
vim = true
```

The composer starts in **INSERT** (ordinary typing); **Esc** drops to **NORMAL**, where keys are motions and operators instead of text. The status bar shows the active mode and a block cursor marks NORMAL. Single-line vim:

- **Motions:** `h`/`l` left/right, `0`/`^`/`$` line ends, `w`/`b`/`e` by word, `j`/`k` recall input history. A count prefix repeats them (`3w`, `2x`).
- **Insert:** `i`/`a` before/after the cursor, `I`/`A` line start/end, `o`/`O` end/start (single-line analogs).
- **Edits:** `x`/`X` delete a char, `r` replace one, `d`/`c`/`y` operators with a motion (`dw`, `c$`, `ye`) or doubled for the whole line (`dd`/`cc`/`yy`), `D`/`C`/`s`/`S`, `p`/`P` paste, `u` undo.

The Ctrl readline chords (`Ctrl-A/E/U/W/K`, history, etc.) keep working in both modes, and **Enter** submits from either.

## Migrating from Ollama

The local default is llama.cpp; Ollama stays fully supported but is opt-in:

- Explicit `[[providers]]` entries with `kind = "ollama"` behave exactly as before.
- A legacy config that only sets top-level `model` / `ollama_host` now resolves to llama.cpp at `http://127.0.0.1:11435`; add an explicit Ollama provider (`/provider add local ollama http://127.0.0.1:11434 <model>`) to stay on Ollama.
- If the local backend isn't installed or can't start, Wizard falls back to bring-your-own-provider: any configured cloud provider, then `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` / `XAI_API_KEY` / `OPENROUTER_API_KEY` from the environment, then interactive setup.

To switch an existing install to llama.cpp, add a provider from the TUI and point it at a GGUF:

```
/provider add local-llamacpp llamacpp http://127.0.0.1:11435 Qwen3.6-27B-Q4_K_M
/provider use local-llamacpp
```

Then set `gguf_path` on that provider in `~/.wizard/config.toml` so Wizard can start the server for you. Or re-run onboarding: `wizard --onboard`.

## Using Ollama instead

Install with the Ollama flavor (installs Ollama, starts it, pulls the auto-tiered model):

```bash
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | WIZARD_USE_OLLAMA=1 bash
```

Or pick the local Ollama option in onboarding (`wizard --onboard`). Wizard speaks Ollama's native `/api/chat` for these providers, as before.

## Using a cloud or remote provider

Any OpenAI-compatible endpoint, OpenRouter, Cloudflare Workers AI, Anthropic, or xAI works. The simplest path is `/provider` inside the TUI: it opens a menu of your configured providers (Enter switches to one) with an **Add provider…** entry that walks you through each type. Pick xAI (API key or account sign-in), OpenRouter, Cloudflare Workers AI, OpenAI, Anthropic, or an OpenAI-compatible custom endpoint; you type the API key inline (hidden) and it is stored in `~/.wizard/credentials.toml` (file mode 0600). xAI account sign-in runs the OAuth flow and adds the provider for you.

The same thing is scriptable with explicit arguments:

```
/provider add openai openai https://api.openai.com/v1 gpt-4o OPENAI_API_KEY
/provider add xai xai https://api.x.ai/v1 grok-4.5 XAI_API_KEY
/provider use openai
```

With `/provider add`, the last argument names the environment variable holding your API key (export it before launching, `export OPENAI_API_KEY=sk-...`); the key itself is never written to disk. The interactive menu instead stores the key in `~/.wizard/credentials.toml`. Onboarding offers the same choices interactively. The default install puts down no local stack, so picking a cloud provider on first run is all there is to it.

### Using OpenRouter

OpenRouter serves hundreds of hosted models behind one OpenAI-compatible endpoint and one API key:

```
/provider add openrouter openrouter https://openrouter.ai/api/v1 openrouter/auto OPENROUTER_API_KEY
/provider use openrouter
```

`openrouter/auto` is OpenRouter's Auto Router, which picks a model per prompt; any `vendor/model` tag from openrouter.ai/models works instead. Wizard sends OpenRouter's recommended attribution headers (`HTTP-Referer`, `X-Title`) on every request.

### Using Cloudflare Workers AI

[Cloudflare Workers AI](https://developers.cloudflare.com/workers-ai/) serves open models (GLM, Llama, Qwen, …) on serverless GPUs behind an account-scoped OpenAI-compatible endpoint. It needs two things: your **account id** (Cloudflare dashboard → Workers AI, or `wrangler whoami`) and an **API token** with the Workers AI permission. The default model is **GLM 5.2** (`@cf/zai-org/glm-5.2`).

The interactive `/provider` menu is the easiest path: pick **Cloudflare Workers AI (API token)**, paste the account id (folded into the endpoint URL) then the token (stored in `~/.wizard/credentials.toml`). Scripted, the account id goes in the base URL:

```
export CLOUDFLARE_API_TOKEN=...
/provider add cloudflare cloudflare https://api.cloudflare.com/client/v4/accounts/<ACCOUNT_ID>/ai/v1 @cf/zai-org/glm-5.2 CLOUDFLARE_API_TOKEN
/provider use cloudflare
```

Any `@cf/...` text-generation tag works in place of the model (see [the catalog](https://developers.cloudflare.com/workers-ai/models/)); `/model` lists what your account can serve. Workers AI's OpenAI-compatible surface exposes only chat completions (no `/v1/models`), so Wizard discovers models and probes health against Cloudflare's native account catalog.

### Signing in with an xAI account

You can use xAI without an API key by signing in with your xAI account (OAuth 2.0 with PKCE). Pick **Add provider… → xAI (Grok) sign-in** from the `/provider` menu, or run it directly:

```bash
wizard --login xai     # or /login xai from inside the TUI
```

Wizard opens your browser, captures the callback on localhost, and stores the tokens in `~/.wizard/xai_oauth.json` (file mode 0600); the access token is refreshed automatically. On success it adds the `xai-oauth` provider and switches the live agent to it; no `/provider add` needed. The browser GUI can start the same flow from Settings (see [Desktop app](desktop.md) / the GUI protocol).

Note: xAI gates OAuth API access to certain SuperGrok plans. If requests come back with HTTP 403, use the API-key flavor (`kind = "xai"` with `XAI_API_KEY`) instead.

### Signing in with a ChatGPT account

ChatGPT subscription access is OAuth too (OpenAI's Codex backend, not the public Chat Completions API):

```bash
wizard --login chatgpt
```

Tokens land in `~/.wizard/chatgpt_oauth.json` (mode 0600). On success Wizard adds a `chatgptoauth` provider pointed at `chatgpt.com/backend-api/codex`. The GUI Settings sign-in path accepts `chatgpt` the same way as `xai`. You still need a plan that the Codex backend accepts; a failed exchange or 403 is the usual signal that the account is not eligible.

## Headless mode

Run a single task without the TUI:

```bash
wizard -p "find all TODO comments and list them by file"
```

Combine with sovereign mode for autonomous execution:

```bash
wizard --mode sovereign -p "implement JWT refresh tokens"
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

`wizard update` upgrades the binary in place: it downloads the latest release from GitHub, verifies its sha256 against the published `checksums.txt`, and swaps it in. The previous binary is kept as `<name>.bak`, so the change takes effect on the next `wizard` launch.

```bash
wizard update              # download and install the latest release
wizard update --check      # report the current and latest version; install nothing
wizard update --to v0.4.0  # install a specific tag instead of the latest
wizard update --rollback   # restore the previous binary from <name>.bak
```

If the binary lives in a root-owned directory (e.g. `/usr/local/bin`), `wizard update` escalates the final move with `sudo` when run in a terminal; in a non-interactive context it prints the exact `sudo install` command instead.

Wizard also checks for a newer release at startup (once every 24 hours, cached in `~/.wizard/update-check.json`). By default it just prints a one-line notice; nothing is downloaded until you run `wizard update`. Configure it with an `[update]` block in `~/.wizard/config.toml`:

```toml
[update]
notify = true                 # print a one-liner when a newer release exists (default)
auto = false                  # download + install newer releases in the background at startup
repo = "teddytennant/wizard"  # GitHub owner/repo to check (point a fork elsewhere)
interval_hours = 24           # hours between startup checks
```

With `auto = true` the new binary is fetched in the background and takes effect on the next launch (the running process is never hot-swapped); it is skipped when the install directory needs `sudo`, falling back to the notice.

Re-running the installer still works and leaves an existing `~/.wizard/config.toml` untouched:

```bash
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | bash
```

To change models, download a GGUF into `~/.wizard/models/` (Hugging Face hosts Q4_K_M quants of most open models), update `model` and `gguf_path` in `~/.wizard/config.toml`, then `/server stop` and `/server start` (or restart Wizard).

To install a specific release via the installer instead, or to roll back after an update, pin the tag:

```bash
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | WIZARD_VERSION=v0.4.0 bash
```

## Uninstall

Everything Wizard installs lives in two places: the binary and its `~/.wizard/` state directory.

```bash
# stop a running llama-server first, if Wizard started one
kill "$(cat ~/.wizard/llama-server.pid)" 2>/dev/null

# the binary (and the llama-server symlink next to it)
sudo rm -f /usr/local/bin/wizard /usr/local/bin/wizard.prev /usr/local/bin/llama-server
# or, if it was installed to ~/.local/bin (NixOS, or no sudo at install time):
rm -f ~/.local/bin/wizard ~/.local/bin/wizard.prev ~/.local/bin/llama-server

# the managed runtime and models (large): llama.cpp tree, GGUFs, symlinks
rm -rf ~/.wizard/bin ~/.wizard/models ~/.wizard/llama.cpp
```

Removing the rest of `~/.wizard/` (config, credentials, sessions, loadout, evolution log) is optional: delete the whole directory with `rm -rf ~/.wizard` for a clean slate. If the installer set up Ollama (`WIZARD_USE_OLLAMA=1` / `WIZARD_BYOM=1`), that is a separate program; uninstall it per Ollama's own docs.

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
base_url = "http://127.0.0.1:11435"
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
- [Sync](sync.md): move your config and skills to another machine as a signed bundle
- [Architecture](architecture.md): how Wizard works under the hood
