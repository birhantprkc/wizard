# Bring your own model (BYOM)

The default Wizard installer pulls **official Ollama library models** only (`qwen3.6:27b`, `qwen3.6:35b`, or `qwen3.5:9b`). This keeps the one-liner simple and avoids shipping custom model weights.

If you need a different model — a fine-tune, a private registry tag, a local GGUF via Modelfile — use the BYOM installer.

## Install with BYOM

```bash
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install-byom.sh | bash
```

This script:

1. Installs the `wizard` binary (same as the main installer)
2. Installs Ollama if needed
3. **Does not** pull any model automatically
4. Walks you through model selection
5. Writes your choice to `~/.wizard/config.toml`

## Interactive flow

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
==> Writing ~/.wizard/config.toml
==> Done. Run: wizard
```

## Option 1: Ollama library model

Pick any model from [ollama.com/library](https://ollama.com/library):

```bash
# Examples
ollama pull qwen3.6:27b
ollama pull qwen3-coder:30b
ollama pull deepseek-r1:32b
```

The BYOM script runs `ollama pull` for you and sets `model` in config.

## Option 2: Custom registry tag

For models published to Ollama Hub under a user or org namespace:

```bash
ollama pull myorg/internal-coder:latest
```

Requirements:

- The model must support tool calling (required for Wizard's agent loop)
- Chat template must be compatible with Ollama's `/api/chat` endpoint

## Option 3: Local Modelfile

Create a `Modelfile` pointing at a GGUF on disk or HuggingFace:

```dockerfile
# Modelfile.example
FROM /path/to/your-model.Q4_K_M.gguf
PARAMETER temperature 0.6
PARAMETER num_ctx 131072
SYSTEM You are a coding assistant. Use tools precisely.
```

Then:

```bash
ollama create my-coder -f Modelfile.example
```

The BYOM script sets `model = "my-coder"` in config.

### HuggingFace GGUF via URL

```dockerfile
FROM https://huggingface.co/SomeOrg/SomeModel/resolve/main/model-Q4_K_M.gguf
PARAMETER temperature 0.6
PARAMETER num_ctx 65536
```

## Option 4: Already installed

If `ollama list` shows your model, select it directly. No pull step.

## Manual configuration

You can also edit config by hand:

```toml
# ~/.wizard/config.toml
model = "my-custom-model"
ollama_host = "http://127.0.0.1:11434"
mode = "genie"
auto_approve = false
```

Verify the model works:

```bash
ollama run my-custom-model "write a hello world in Rust"
wizard -p "list files in the current directory"
```

## Model requirements

Wizard v0.1 expects models that support:

| Capability | Required | Notes |
|------------|----------|-------|
| Tool calling | Recommended | Native preferred; Wizard falls back to a prompt-based JSON tool protocol when a model lacks native support |
| Ollama `/api/chat` | Yes | Streaming responses |
| Context ≥ 32K | Recommended | 128K+ preferred for large codebases |
| Code quality | Recommended | Coding-oriented models perform best |

Native tool calling through Ollama is historically inconsistent across models, so Wizard probes for it at startup: if the model advertises tools support it uses native function calling, otherwise it switches to a prompt-based JSON tool protocol. The JSON path is less reliable on weaker models — prefer a model with solid native tool calling for the smoothest agent loop.

## Remote Ollama

Point Wizard at a remote Ollama instance:

```toml
ollama_host = "http://gpu-server.local:11434"
model = "qwen3.6:27b"
```

Ensure the model is pulled on that server, not just locally.

## Disclaimer

The BYOM installer lets **you** choose any Ollama-compatible model. Wizard does not ship, endorse, or maintain third-party model weights. You are responsible for compliance with the model's license and acceptable use terms.

The default `install.sh` one-liner uses only official Qwen models from the Ollama library.

## Switching back to official models

```bash
ollama pull qwen3.6:27b
```

Edit `~/.wizard/config.toml`:

```toml
model = "qwen3.6:27b"
```

Or re-run the standard installer:

```bash
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | bash
```