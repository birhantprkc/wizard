# Bring your own model (BYOM)

Wizard runs on whatever model you point it at. For hosted models, `/provider add` registers any OpenAI-compatible endpoint or Anthropic (see [getting started](getting-started.md#using-a-cloud-or-remote-provider)); this page covers bringing your own *local* model weights. The default install ships no model at all, and Wizard's managed local option (onboarding's Local pick, or `WIZARD_LOCAL=1` at install) downloads only official Qwen GGUF quants. Custom weights are always your call, and swapping them in is easy on both local backends.

## Any GGUF with llama.cpp (the default local backend)

With the llama.cpp default there is no special installer: `llama-server` loads any GGUF directly. Download one (Hugging Face hosts Q4_K_M-class quants of most open models) and point the provider at it in `~/.wizard/config.toml`:

```toml
[[providers]]
name = "local"
kind = "llamacpp"
base_url = "http://127.0.0.1:11435"
model = "my-coder-Q4_K_M"
gguf_path = "/home/you/.wizard/models/my-coder-Q4_K_M.gguf"
```

Wizard starts `llama-server` with that file automatically (see [getting started](getting-started.md#first-run)). Alternatively:

- Run `wizard --onboard` and pick "Type a custom GGUF path…" in the model step; it lists GGUFs already in `~/.wizard/models/` first.
- Override per run with `WIZARD_GGUF_PATH=/path/to/model.gguf` (and `WIZARD_MODEL=<tag>` for the label).

Prefer a model that supports tool calling; Wizard spawns the server with `--jinja` so OpenAI-style tool calls work, and falls back to a prompt-based JSON tool protocol for models without native support.

## BYOM with Ollama

If you run Ollama instead (a fine-tune, a private registry tag, a local GGUF via Modelfile), use the main installer's BYOM flavor.

### Install with BYOM

```bash
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | WIZARD_BYOM=1 bash
```

With `WIZARD_BYOM=1`, the installer:

1. Installs Ollama if needed and starts it
2. Does not pull any model automatically: it walks you through model selection first (set `WIZARD_MODEL=<tag>` to skip the prompts, e.g. for non-interactive installs)
3. Installs the `wizard` binary (same as the default flavor)
4. Writes your choice to `~/.wizard/config.toml`: a fresh config gets a full `[[providers]]` Ollama entry; an existing config keeps everything else and only has its `model =` line(s) updated
5. Lays down the [default loadout](loadout.md) (browser MCP + subagents), each file only if absent

The old `install-byom.sh` URL still works: it is now a thin shim that fetches `install.sh` and runs it with `WIZARD_BYOM=1`, passing all other `WIZARD_*` variables through (plus the shim-only `WIZARD_INSTALLER_REF` to pick which ref to fetch `install.sh` from, default `main`).

### Interactive flow

```
==> Wizard BYOM Setup

Choose how to configure your model:

  1) Pull an existing Ollama library model
  2) Pull a custom Ollama registry tag
  3) Create from a local Modelfile
  4) Use a model already installed (skip pull)

Selection [1-4]: 2
Enter Ollama model tag (e.g. myuser/my-model:27b): myuser/coder-v2
==> Pulling myuser/coder-v2 ...
```

The rest of the install (binary, config, loadout) then proceeds and ends with `==> Done. Run: wizard`.

### Option 1: Ollama library model

Pick any model from [ollama.com/library](https://ollama.com/library):

```bash
# Examples
ollama pull qwen3.6:27b
ollama pull qwen3-coder:30b
ollama pull deepseek-r1:32b
```

The BYOM install runs `ollama pull` for you and sets `model` in config.

### Option 2: Custom registry tag

For models published to Ollama Hub under a user or org namespace:

```bash
ollama pull myorg/internal-coder:latest
```

Requirements:

- The model must support tool calling (required for Wizard's agent loop)
- Chat template must be compatible with Ollama's `/api/chat` endpoint

### Option 3: Local Modelfile

Create a `Modelfile` pointing at a GGUF on disk or HuggingFace:

```dockerfile
# Modelfile.example
FROM /path/to/your-model.Q4_K_M.gguf
PARAMETER temperature 0.6
PARAMETER num_ctx 131072
SYSTEM You are a careful assistant. Use tools precisely.
```

Then:

```bash
ollama create my-coder -f Modelfile.example
```

The BYOM install sets `model = "my-coder"` in config.

### HuggingFace GGUF via URL

```dockerfile
FROM https://huggingface.co/SomeOrg/SomeModel/resolve/main/model-Q4_K_M.gguf
PARAMETER temperature 0.6
PARAMETER num_ctx 65536
```

### Option 4: Already installed

If `ollama list` shows your model, select it directly. No pull step.

### Manual configuration

You can also edit config by hand; Ollama is selected with an explicit provider entry:

```toml
# ~/.wizard/config.toml
mode = "genie"

[[providers]]
name = "local"
kind = "ollama"
base_url = "http://127.0.0.1:11434"
model = "my-custom-model"
```

Verify the model works:

```bash
ollama run my-custom-model "write a hello world in Rust"
wizard -p "list files in the current directory"
```

## Model requirements

Wizard expects models that support:

| Capability | Required | Notes |
|------------|----------|-------|
| Tool calling | Recommended | Native preferred; Wizard falls back to a prompt-based JSON tool protocol when a model lacks native support |
| Streaming chat | Yes | llama-server's `/v1/chat/completions` or Ollama's `/api/chat` |
| Context ≥ 32K | Recommended | 128K+ preferred for large codebases |
| Code quality | Recommended | Coding-oriented models perform best |

Native tool calling varies by model, so Wizard probes for it at startup: models that advertise tools support get native function calling, others fall back to a prompt-based JSON tool protocol. The JSON path is less reliable on weaker models, so prefer one with solid native tool calling.

### Remote servers

Point Wizard at a model server on another machine and it connects instead of spawning (Wizard only starts `llama-server` for loopback URLs):

```toml
[[providers]]
name = "gpu-box"
kind = "llamacpp"
base_url = "http://gpu-server.local:8080"
model = "Qwen3.6-27B-Q4_K_M"
```

For a remote Ollama instance, use an explicit provider entry too:

```toml
[[providers]]
name = "gpu-box"
kind = "ollama"
base_url = "http://gpu-server.local:11434"
model = "qwen3.6:27b"
```

Ensure the model is loaded/pulled on that server, not just locally.

## Disclaimer

BYOM lets you choose any GGUF or Ollama-compatible model. Wizard does not ship, endorse, or maintain third-party model weights. You are responsible for compliance with the model's license and acceptable use terms.

The default `install.sh` one-liner downloads no model weights; Wizard's managed local setup downloads only official Qwen quants.

## Switching back to official models

Run `wizard --onboard` and pick the recommended tier, or re-run the installer with the local flavor: it downloads the VRAM-matched official Qwen GGUF and leaves an existing config untouched, so update `model` / `gguf_path` in the provider entry afterwards:

```bash
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | WIZARD_LOCAL=1 bash
```

On Ollama: `ollama pull qwen3.6:27b` and set `model = "qwen3.6:27b"`.