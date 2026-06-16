# Security

Wizard is an agent that runs shell commands, writes files, and (if you ask it to) recompiles and replaces its own binary. This document describes what protections exist, what they actually cover, and where you are trusting the model, the tools, or yourself. It is written to be honest rather than reassuring.

## The short version

- Everything Wizard does runs **as you**, with your privileges. There is no sandbox.
- **Both modes auto-approve tool calls by default**: everything runs without per-action confirmation. To restore a y/n gate, set `auto_approve = false` in your config. Sovereign mode additionally runs non-interactively and continuously, self-directing with no human in the loop.
- MCP servers and scripted tools are programs you chose to run. Wizard scrubs their environment and bounds their time, but it cannot make an untrustworthy program trustworthy.
- Deep `/evolve` requires a successful build and a smoke test, and keeps the old binary for rollback; with `auto_approve = false`, you also confirm the diff before the build starts.

## Tool-call approval model

Both modes (genie and sovereign) **auto-approve tool calls by default**. Every file write, shell command, MCP call, scripted tool, and `/evolve` runs without a per-action y/n prompt unless you opt in to the confirmation gate. The state-changing tools are:

- **File writes:** `write_file` and `edit_file`
- **Shell:** `execute` (this is also how git commits, pushes, and any other command happen)
- **Scripted tools:** agent-authored scripts in `~/.wizard/tools/`
- **MCP tools:** every tool served by an MCP server
- **Subagents:** spawning a subagent (which also runs with auto-approval by default)
- **`/evolve`:** runtime and deep evolutions run without confirmation by default

Read-only tools (`read_file`, `list_files`, `search_files`, `git_status`, `git_diff`) are always non-destructive.

**To restore per-action confirmation:** set `auto_approve = false` in `~/.wizard/config.toml`. With that flag, every state-changing tool call pauses for an explicit y/n before it runs. This is the only confirmation gate; it is off by default.

**Sovereign mode** adds non-interactive continuous operation on top of the shared auto-approve default: it completes the task then keeps going, self-directing and self-improving via `evolve` with no human in the loop, persisting a durable mission. A confused or prompt-injected model (in either mode) can run arbitrary commands as you. Only run Wizard on tasks and machines where that is acceptable, and prefer a container or VM for anything you would not run by hand (see "No sandbox" below).

The auto-approve default is a convenience choice, not a containment mechanism. Tool calls run with your full privileges whether or not the gate is on.

## MCP servers

Wizard is an MCP client: servers declared in `~/.wizard/mcp.toml` (stdio or HTTP) have their tools merged into the registry. What Wizard does to limit the damage a server can do *by accident*:

- **Cleared environment.** Stdio servers are spawned with `env_clear()`, so they do not inherit the wizard process's environment: API keys and other secrets in your shell do not leak into child processes. Only an allowlist is forwarded from the parent: `PATH`, `HOME`, `LANG`, `LC_ALL`, `TERM`, `USER`, `SHELL`, `TMPDIR`.
- **Dynamic-linker variables are dropped.** `env` entries in `mcp.toml` are passed through to the child, *except* `LD_PRELOAD`, `LD_LIBRARY_PATH`, `LD_AUDIT`, `DYLD_INSERT_LIBRARIES`, and `DYLD_LIBRARY_PATH`. Each of those is a code-injection vector into the spawned process, so they are never forwarded (a warning is logged when one is dropped).
- **Per-request timeouts.** Connect/initialize is bounded at 20 s, `tools/list` at 30 s per page (with a hard cap on pagination), and each `tools/call` at 120 s. A wedged or malicious server can waste your time, not hang Wizard forever.
- **No name shadowing.** An MCP tool that advertises a native tool's name (`execute`, `write_file`, …) is namespaced `server__tool` instead of replacing the built-in.
- **Auto-approved by default.** MCP tool calls run without confirmation unless `auto_approve = false` is set.

What Wizard does **not** do: an MCP server is an arbitrary program *you* configured Wizard to run. It executes with your full privileges, can open its own network connections, read your files, and do anything else your user can. The environment scrubbing limits accidental secret leakage and one specific injection vector; it does not contain a server that is itself malicious. Register only servers you trust, the same way you would vet anything you pipe to `sh`.

The same applies to scripted tools: they are scripts under `~/.wizard/tools/` that run as you.

## Deep `/evolve` (self-recompilation)

`/evolve --deep` is the unusual one: the agent proposes a diff to Wizard's own Rust source, builds it, and replaces the running binary. The gates, in order:

1. **Approval.** With `auto_approve = false`, you see the proposed unified diff and must confirm ("Apply this diff and rebuild Wizard?") before the build starts. With the default auto-approve setting, this gate is off and the build proceeds immediately.
2. **Build.** `cargo build --release` must succeed. On failure, the diff is reverted and the running binary is untouched.
3. **Smoke test.** The freshly built binary is executed with `--version` and must exit 0 and print a `wizard` version string. On failure, the diff is reverted and the current binary is kept.

Only after all three does Wizard install the new binary over the running executable, and it first moves the old one aside as `<name>.prev` in the same directory. To roll back a deep evolution:

```bash
mv /usr/local/bin/wizard.prev /usr/local/bin/wizard
```

(Adjust the path if you installed elsewhere; Wizard prints the exact rollback command when it installs.)

Be clear about what the smoke test is: it proves the new binary launches and reports a version, nothing more. It does not prove the change is correct, safe, or what you asked for. The meaningful gate is you reading the diff at step 1, which is why running deep evolve with the default auto-approval (both genie and sovereign) means letting the model rewrite its own agent loop unsupervised. Every deep evolution is logged with its diff to `~/.wizard/evolution.jsonl`, and the source checkout at `~/.wizard/src` keeps the change as a git commit, so there is always a record of what changed.

## No sandbox

All tools run directly with your user's privileges. The `execute` tool runs real shell commands and cannot be confined to the working directory: absolute paths, `cd ..`, pipes, and network access are all reachable. The same is true of MCP servers and scripted tools. Treat tool execution as full local access, because it is.

Also note that the model reads files and tool output as instructions-adjacent context. A hostile string in a repository you point Wizard at (a README, a test fixture, a commit message) can attempt to steer the model's tool calls (classic prompt injection). The `auto_approve = false` confirmation gate is your defense against prompt injection; by default, both modes operate without it.

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
