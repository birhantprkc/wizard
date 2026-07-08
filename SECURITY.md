# Security

Wizard is an agent that runs shell commands, writes files, and (if you ask it to) recompiles and replaces its own binary. This document describes what protections exist, what they actually cover, and where you are trusting the model, the tools, or yourself. It is written to be honest rather than reassuring.

## The short version

- Everything Wizard does runs **as you**, with your privileges. There is no sandbox.
- **There is no per-action approval gate, by design.** Every tool call runs as soon as the model makes it, in both modes. Sovereign mode additionally runs non-interactively and continuously, self-directing with no human in the loop.
- MCP servers and scripted tools are programs you chose to run. Wizard scrubs their environment and bounds their time, but it cannot make an untrustworthy program trustworthy.
- Deep `/evolve` is gated by a clean `cargo build` and a smoke test, and keeps the old binary for one-`mv` rollback. There is no diff-approval step.
- API keys live in environment variables or `~/.wizard/credentials.toml` (written atomically, file mode 0600). `config.toml` only ever names the env var, never the key.

## No approval gate

Wizard has no per-action confirmation, in either mode (genie or sovereign). Every file write, shell command, MCP call, scripted tool, and `/evolve` runs the moment the model calls it. The state-changing tools are:

- **File writes:** `write_file` and `edit_file`
- **Shell:** `execute` (this is also how git commits, pushes, and any other command happen)
- **Scripted tools:** agent-authored scripts in `~/.wizard/tools/`
- **MCP tools:** every tool served by an MCP server
- **Subagents:** spawning a subagent (which runs its own loop, equally ungated)
- **`/evolve`:** runtime and deep evolutions run without confirmation

Read-only tools (`read_file`, `list_files`, `search_files`, `git_status`, `git_diff`) are always non-destructive.

There is no config key that restores a y/n gate. Earlier releases had an `auto_approve` flag; it was removed, and a config that still carries it loads fine: the key is ignored and never written back.

**Sovereign mode** adds non-interactive continuous operation on top of this: it completes the task then keeps going, self-directing and self-improving via `evolve` with no human in the loop, persisting a durable mission. A confused or prompt-injected model (in either mode) can run arbitrary commands as you. Only run Wizard on tasks and machines where that is acceptable, and prefer a container or VM for anything you would not run by hand (see "No sandbox" below).

Tool calls run with your full privileges. The boundary that matters is the machine and task you point Wizard at, not a prompt.

## MCP servers

Wizard is an MCP client: servers declared in `~/.wizard/mcp.toml` (stdio or HTTP) have their tools merged into the registry. What Wizard does to limit the damage a server can do *by accident*:

- **Cleared environment.** Stdio servers are spawned with `env_clear()`, so they do not inherit the wizard process's environment: API keys and other secrets in your shell do not leak into child processes. Only an allowlist is forwarded from the parent: `PATH`, `HOME`, `LANG`, `LC_ALL`, `TERM`, `USER`, `SHELL`, `TMPDIR`.
- **Dynamic-linker variables are dropped.** `env` entries in `mcp.toml` are passed through to the child, *except* `LD_PRELOAD`, `LD_LIBRARY_PATH`, `LD_AUDIT`, `DYLD_INSERT_LIBRARIES`, and `DYLD_LIBRARY_PATH`. Each of those is a code-injection vector into the spawned process, so they are never forwarded (a warning is logged when one is dropped).
- **Per-request timeouts.** Connect/initialize is bounded at 20 s, `tools/list` at 30 s per page (with a hard cap on pagination), and each `tools/call` at 120 s. A wedged or malicious server can waste your time, not hang Wizard forever.
- **No name shadowing.** An MCP tool that advertises a native tool's name (`execute`, `write_file`, …) is namespaced `server__tool` instead of replacing the built-in.

MCP tool calls run without confirmation, like every other tool.

What Wizard does **not** do: an MCP server is an arbitrary program *you* configured Wizard to run. It executes with your full privileges, can open its own network connections, read your files, and do anything else your user can. The environment scrubbing limits accidental secret leakage and one specific injection vector; it does not contain a server that is itself malicious. Register only servers you trust, the same way you would vet anything you pipe to `sh`.

The same applies to scripted tools: they are scripts under `~/.wizard/tools/` that run as you.

## Deep `/evolve` (self-recompilation)

`/evolve --deep` is the unusual one: the agent proposes a diff to Wizard's own Rust source, builds it, and replaces the running binary. There is no approval step; the gates are mechanical, in order:

1. **Build.** `cargo build --release` must succeed. On failure, the diff is reverted and the running binary is untouched.
2. **Smoke test.** The freshly built binary is executed with `--version` and must exit 0 and print a `wizard` version string. On failure, the diff is reverted and the current binary is kept.

Only after both does Wizard install the new binary over the running executable, and it first moves the old one aside as `<name>.prev` in the same directory. To roll back a deep evolution:

```bash
mv /usr/local/bin/wizard.prev /usr/local/bin/wizard
```

(Adjust the path if you installed elsewhere; Wizard prints the exact rollback command when it installs.)

Be clear about what the smoke test is: it proves the new binary launches and reports a version, nothing more. It does not prove the change is correct, safe, or what you asked for. Deep evolve is the model rewriting its own agent loop, checked only by the compiler and the smoke test. The record and the rollback are the safety net: every deep evolution is logged with its diff to `~/.wizard/evolution.jsonl`, the source checkout at `~/.wizard/src` keeps the change as a git commit, and the prior binary stays one `mv` away.

## No sandbox

All tools run directly with your user's privileges. The `execute` tool runs real shell commands and cannot be confined to the working directory: absolute paths, `cd ..`, pipes, and network access are all reachable. The same is true of MCP servers and scripted tools. Treat tool execution as full local access, because it is.

Also note that the model reads files and tool output as instructions-adjacent context. A hostile string in a repository you point Wizard at (a README, a test fixture, a commit message) can attempt to steer the model's tool calls (classic prompt injection). There is no confirmation gate to catch a steered tool call; the defense against prompt injection is isolation, per the recommendation below.

Recommendation: for any Wizard run on untrusted or semi-trusted tasks (third-party repos, code review of unknown patches, anything internet-derived), run Wizard inside a container or VM with only the project mounted. With a local provider (llama.cpp or Ollama) a fully offline container works; with a cloud provider, allow only that provider's API endpoint.

## Install-path trust

The recommended install is `curl | bash`, and you should be honest with yourself about what that means: you are executing a script from the network with your privileges (and it may use `sudo` to place the binary in `/usr/local/bin`). Mitigations, in increasing order of paranoia:

1. **Read it first.** Download `install.sh`, read it, then run it. It is a few hundred lines of plain bash.
2. **Checksum verification.** The installer downloads release tarballs from GitHub releases and verifies their SHA-256 against the release's `checksums.txt`. A mismatch aborts the install. (Honest caveats: if a release has no `checksums.txt` or `sha256sum` is missing on the host, the installer warns and proceeds without verification; and the checksums file comes from the same GitHub release as the tarball, so this defends against corrupted/tampered downloads, not a fully compromised release.)
3. **Build from source.** Clone the repo, audit it, `cargo build --release`. This removes trust in the release pipeline entirely:

   ```bash
   git clone https://github.com/teddytennant/wizard
   cd wizard && cargo build --release
   install -m 755 target/release/wizard ~/.local/bin/wizard
   ```

The default installer also downloads `llama-server` from llama.cpp's official GitHub releases and a GGUF from Hugging Face; with `WIZARD_USE_OLLAMA=1` it instead runs Ollama's official install script (`curl -fsSL https://ollama.com/install.sh | sh`) if Ollama is absent: same trust consideration, different vendor. Skip these with `WIZARD_SKIP_LLAMACPP_INSTALL=1` / `WIZARD_SKIP_OLLAMA_INSTALL=1` if you manage the model runtime yourself.

## Where your data goes

Inference goes to whichever provider is active: the core loop sends prompts, code context, and tool output to that endpoint and nowhere else. With the default local provider that endpoint is `llama-server` on your machine (`http://127.0.0.1:11435`); with a cloud provider it is that vendor's API, under their data-handling terms. The other network actors are the things you add: MCP servers and scripted tools can make whatever calls they like, and deep evolve clones the source repo and may install a Rust toolchain via rustup on first use.

## Reporting a vulnerability

If you find a security issue in Wizard:

- Open a private report via **GitHub security advisories** on [teddytennant/wizard](https://github.com/teddytennant/wizard/security/advisories)

Please include reproduction steps and the version (`wizard --version`). Reports are read by a human; expect an acknowledgment, not an SLA; this is a small open-source project.
