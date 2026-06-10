# Wizard Arsenal

**One line. Wizard, batteries included.**

```bash
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard-arsenal/main/install.sh | bash
```

Wizard Arsenal is [Wizard](https://github.com/teddytennant/wizard) with a fuller default loadout. The binary is upstream Wizard — the same fully local, self-extending Ratatui agent — built from source on your machine. What Arsenal adds is the *configuration*: a preconfigured browser, a roster of ready-to-use subagents, and a hardware-sized model, all written into `~/.wizard/` for you so the first run is already equipped.

Everything still runs locally. There are no API keys and no cloud services unless you add a provider yourself.

---

## What it sets up

The installer does everything upstream Wizard's installer does — installs Ollama, picks a Qwen model tier that fits your VRAM, pulls it, and builds the `wizard` binary — and then lays down the Arsenal configuration:

- **A preconfigured browser.** `~/.wizard/mcp.toml` declares the [Playwright MCP](https://github.com/microsoft/playwright-mcp) server (`npx -y @playwright/mcp@latest`). After Wizard starts (or `/reload`), the browser's navigate / click / type / snapshot tools are in the registry. No `/evolve` step needed — the canonical browser recipe from [WIZARD.md §2](https://github.com/teddytennant/wizard/blob/main/WIZARD.md) ships ready. Needs Node/`npx` on your PATH.
- **A roster of subagents.** `~/.wizard/subagents/` ships four specialists the parent model can delegate to with `spawn_subagent`:
  - `reviewer` — code-review specialist, scoped to read/search/git tools (read-only).
  - `researcher` — web research using the Playwright browser tools; gathers facts and reports back.
  - `tester` — runs the test suite, diagnoses failures, and fixes them.
  - `documenter` — writes and updates README/docs/comments to match the code.
- **Hardware-aware model selection.** Same VRAM detection as upstream (NVIDIA `nvidia-smi`, AMD `rocm-smi`/sysfs, system RAM on CPU-only): the largest single-GPU-fitting Qwen tier, written into `config.toml`.
- **A provider-shaped config.** `config.toml` uses Wizard's `[[provider]]` blocks with `active_provider = "local"` (Ollama). Commented-out OpenAI and Anthropic blocks are included so adding a cloud model later is uncommenting four lines — or just run `/provider` in the TUI.

The installer never clobbers an existing `~/.wizard/config.toml`, `mcp.toml`, or any subagent file you already have. It only writes what is missing.

---

## Adding cloud providers

Arsenal is local-first, but you are not locked to local. `/provider` in the TUI lets you add and switch between providers (OpenAI, Anthropic, or any other configured `[[provider]]`) without editing files by hand. The shipped `config.toml` already carries commented templates for both, each reading its key from an environment variable (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`) rather than storing it on disk.

See [docs/arsenal.md](https://github.com/teddytennant/wizard-arsenal/blob/main/docs/arsenal.md) for the full walkthrough.

---

## Relationship to upstream Wizard

Arsenal is a thin distribution layer over [`teddytennant/wizard`](https://github.com/teddytennant/wizard). It carries **no source changes** — the binary you get is upstream Wizard at the pinned ref, built from source. Everything Wizard does (genie/sovereign modes, `/evolve` self-extension, runtime MCP, `--continuous`, `/publish`) works exactly as documented upstream. Arsenal only changes the defaults under `~/.wizard/`.

If you want to follow upstream development, read the [Wizard README](https://github.com/teddytennant/wizard/blob/main/README.md) and [docs](https://github.com/teddytennant/wizard/tree/main/docs). To customize *behavior* (not just config), deep-evolve upstream Wizard and `/publish` your own fork — Arsenal is for shipping a richer *configuration*, not a forked binary.

---

## Quick start

```bash
# Install: Ollama + a VRAM-sized model + the wizard binary + the Arsenal config
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard-arsenal/main/install.sh | bash

# Launch (genie mode — interactive, bypass-permissions by default)
wizard

# Try the preconfigured browser (Playwright MCP must have started — `/reload` if needed)
> research the latest Ratatui release notes and summarize the breaking changes

# Delegate to a subagent
> use the reviewer subagent to review my staged changes
```

---

## Requirements

- Linux (x86_64 or aarch64) — same as upstream Wizard.
- `curl`, `git`, and a Rust toolchain (the installer installs `rustup` if `cargo` is absent).
- **Node / `npx`** for the Playwright browser server. Without it, every other capability still works; only the browser tools are unavailable until Node is installed.

---

## License

MIT, inherited from upstream Wizard.

## Author

Teddy Tennant — [github.com/teddytennant](https://github.com/teddytennant)
