# Security

Wizard is a coding agent that runs shell commands, writes files, and — if you ask it to — recompiles and replaces its own binary. This document describes what protections exist, what they actually cover, and where you are trusting the model, the tools, or yourself. It is written to be honest rather than reassuring.

## The short version

- Everything Wizard does runs **as you**, with your privileges. There is no sandbox.
- **Genie mode** (the default) asks before anything that changes state. **Sovereign mode approves everything automatically** — treat it accordingly.
- MCP servers and scripted tools are programs you chose to run. Wizard scrubs their environment and bounds their time, but it cannot make an untrustworthy program trustworthy.
- Deep `/evolve` is gated by approval, a successful build, and a smoke test, and it keeps the old binary next to the new one for rollback.

## The approval gate (Genie mode)

In genie mode, every tool that can change state is gated behind an explicit y/n confirmation before it runs:

- **File writes** — `write_file` and `edit_file`
- **Shell** — `execute` (this is also how git commits, pushes, and any other command happen)
- **Scripted tools** — agent-authored scripts in `~/.wizard/tools/`
- **MCP tools** — every tool served by an MCP server
- **Subagents** — spawning a subagent (which runs with auto-approval inside its own isolated context)
- **`/evolve`** — runtime evolutions confirm before writing ("Apply this change?"); deep evolutions confirm before building ("Apply this diff and rebuild Wizard?")

Read-only tools (`read_file`, `list_files`, `search_files`, `git_status`, `git_diff`) run without confirmation.

Two things turn the gate off:

- **`--auto`** skips confirmations while keeping the interactive TUI.
- **Sovereign mode** auto-approves *all* tool calls — shell, writes, MCP, scripted tools, and `/evolve` included. That is the point of the mode: it runs unattended. The implication is that whatever the model decides to do, happens. A confused or prompt-injected model in sovereign mode can run arbitrary commands as you with no human in the loop. Only run sovereign mode on tasks and machines where that is acceptable, and prefer a container or VM for anything you would not run by hand (see "No sandbox" below).

The gate is a consent mechanism, not a containment mechanism. Approving a shell command means trusting everything that command does.

## MCP servers

Wizard is an MCP client: servers declared in `~/.wizard/mcp.toml` (stdio or HTTP) have their tools merged into the registry. What Wizard does to limit the damage a server can do *by accident*:

- **Cleared environment.** Stdio servers are spawned with `env_clear()` — they do not inherit the wizard process's environment, so API keys and other secrets in your shell do not leak into child processes. Only an allowlist is forwarded from the parent: `PATH`, `HOME`, `LANG`, `LC_ALL`, `TERM`, `USER`, `SHELL`, `TMPDIR`.
- **Dynamic-linker variables are dropped.** `env` entries in `mcp.toml` are passed through to the child, *except* `LD_PRELOAD`, `LD_LIBRARY_PATH`, `LD_AUDIT`, `DYLD_INSERT_LIBRARIES`, and `DYLD_LIBRARY_PATH` — each of those is a code-injection vector into the spawned process, so they are never forwarded (a warning is logged when one is dropped).
- **Per-request timeouts.** Connect/initialize is bounded at 20 s, `tools/list` at 30 s per page (with a hard cap on pagination), and each `tools/call` at 120 s. A wedged or malicious server can waste your time, not hang Wizard forever.
- **No name shadowing.** An MCP tool that advertises a native tool's name (`execute`, `write_file`, …) is namespaced `server__tool` instead of replacing the built-in.
- **Approval-gated.** In genie mode every MCP tool call asks first.

What Wizard does **not** do: an MCP server is an arbitrary program *you* configured Wizard to run. It executes with your full privileges, can open its own network connections, read your files, and do anything else your user can. The environment scrubbing limits accidental secret leakage and one specific injection vector — it does not contain a server that is itself malicious. Register only servers you trust, the same way you would vet anything you pipe to `sh`.

The same applies to scripted tools: they are scripts under `~/.wizard/tools/` that run as you.

## Deep `/evolve` (self-recompilation)

`/evolve --deep` is the unusual one: the agent proposes a diff to Wizard's own Rust source, builds it, and replaces the running binary. The gates, in order:

1. **Approval.** You see the proposed unified diff and must confirm ("Apply this diff and rebuild Wizard?") — unless you are in sovereign mode or passed `--auto`, in which case this gate is off.
2. **Build.** `cargo build --release` must succeed. On failure, the diff is reverted and the running binary is untouched.
3. **Smoke test.** The freshly built binary is executed with `--version` and must exit 0 and print a `wizard` version string. On failure, the diff is reverted and the current binary is kept.

Only after all three does Wizard install the new binary over the running executable — and it first moves the old one aside as `<name>.prev` in the same directory. To roll back a deep evolution:

```bash
mv /usr/local/bin/wizard.prev /usr/local/bin/wizard
```

(Adjust the path if you installed elsewhere; Wizard prints the exact rollback command when it installs.)

Be clear about what the smoke test is: it proves the new binary launches and reports a version, nothing more. It does not prove the change is correct, safe, or what you asked for. The meaningful gate is you reading the diff at step 1 — which is why running deep evolve with auto-approval (sovereign mode) means letting a local model rewrite its own agent loop unsupervised. Every deep evolution is logged with its diff to `~/.wizard/evolution.jsonl`, and the source checkout at `~/.wizard/src` keeps the change as a git commit, so there is always a record of what changed.

## No sandbox

All tools run directly with your user's privileges. The `execute` tool runs real shell commands and cannot be confined to the working directory — absolute paths, `cd ..`, pipes, and network access are all reachable. The same is true of MCP servers and scripted tools. Treat tool execution as full local access, because it is.

Also note that the model reads files and tool output as instructions-adjacent context. A hostile string in a repository you point Wizard at (a README, a test fixture, a commit message) can attempt to steer the model's tool calls — classic prompt injection. The genie approval gate is your defense; sovereign mode removes it.

Recommendation: for sovereign-mode runs on untrusted or semi-trusted tasks — third-party repos, code review of unknown patches, anything internet-derived — run Wizard inside a container or VM with only the project mounted. Inference is local via Ollama, so a fully offline container works fine.

## Install-path trust

The recommended install is `curl | bash`, and you should be honest with yourself about what that means: you are executing a script from the network with your privileges (and it may use `sudo` to place the binary in `/usr/local/bin`). Mitigations, in increasing order of paranoia:

1. **Read it first.** Download `install.sh`, read it, then run it. It is a few hundred lines of plain bash.
2. **Checksum verification.** The installer downloads release tarballs from GitHub releases and verifies their SHA-256 against the release's `checksums.txt`. A mismatch aborts the install. (Honest caveats: if a release has no `checksums.txt` or `sha256sum` is missing on the host, the installer warns and proceeds without verification — and the checksums file comes from the same GitHub release as the tarball, so this defends against corrupted/tampered downloads, not a fully compromised release.)
3. **Build from source.** Clone the repo, audit it, `cargo build --release`. This removes trust in the release pipeline entirely:

   ```bash
   git clone https://github.com/teddytennant/wizard
   cd wizard && cargo build --release
   install -m 755 target/release/wizard ~/.local/bin/wizard
   ```

The installer also runs Ollama's official install script (`curl -fsSL https://ollama.com/install.sh | sh`) if Ollama is absent — same trust consideration, different vendor. Skip it with `WIZARD_SKIP_OLLAMA_INSTALL=1` if you manage Ollama yourself.

## What stays local

All inference goes to your Ollama instance (`http://127.0.0.1:11434` by default). The core agent loop makes no outbound API calls in v0.1; prompts, code, and sessions stay on your machine. The exceptions are the things you add: MCP servers and scripted tools can make whatever network calls they like, and deep evolve clones the source repo and may install a Rust toolchain via rustup on first use.

## Reporting a vulnerability

If you find a security issue in Wizard:

- Preferred: open a private report via **GitHub security advisories** on [teddytennant/wizard](https://github.com/teddytennant/wizard/security/advisories)
- Or email **192647641+teddytennant@users.noreply.github.com**

Please include reproduction steps and the version (`wizard --version`). Reports are read by a human; expect an acknowledgment, not an SLA — this is a small open-source project.
