# Self-extension (`/evolve`)

Wizard can extend itself. `/evolve` lets the agent give itself new capabilities — a new skill, an external tool server, a scripted tool, a subagent, or (when needed) brand-new Rust in its own core.

The design follows the two self-modifying agents that pioneered this pattern:

- **[Pi](https://newsletter.pragmaticengineer.com/p/building-pi-and-what-makes-self-modifying)** modifies its own installed source in place and `/reload`s it live. It gets away with this because it's interpreted (Node/TS) — no compile step.
- **[Hermes](https://hermes-agent.nousresearch.com/docs/)** never recompiles. It adds capability through portable **skills**, **MCP servers** (the channel for things like computer use), programmatic **scripted tools** (`execute_code`), and isolated **subagents**.

Wizard is compiled Rust, so it can't do Pi-style edit-and-reload on its core for free. Instead it copies Hermes' model for the common case and reserves real recompilation for the rare one. Hence **two tiers**.

---

## Tier 1 — runtime extension (default, no recompile)

Works on every install, including the one-line `curl | bash` binary. `/evolve` writes config/data under `~/.wizard/` and `/reload` activates it live. Four channels:

### Skills

Markdown capability — guidelines, workflows, domain knowledge — injected into the system prompt.

```
> /evolve add a skill for writing conventional commit messages
```

Wizard writes `skills/conventional-commits/SKILL.md` and reloads it into context.

### MCP servers

The path for capabilities that live **outside** Wizard — **computer use**, browser control, databases, search, anything shipped as an [MCP](https://modelcontextprotocol.io) server. Wizard is an MCP client; registering a server merges its tools into the registry with no rebuild.

```
> /evolve give yourself computer use via an MCP server
```

Wizard adds the server to `~/.wizard/mcp.toml`, connects, lists its tools, and they become callable on `/reload`.

```toml
# ~/.wizard/mcp.toml
[[server]]
name = "computer-use"
transport = "stdio"
command = "uvx"
args = ["mcp-computer-use"]
```

### Scripted tools

The agent authors a small script (the Hermes `execute_code` analog), saved to `~/.wizard/tools/` and run through the `execute` sandbox. Good for glue and project-specific automation that doesn't warrant a whole MCP server.

```
> /evolve add a tool that renders a mermaid diagram to PNG
```

Saved as `~/.wizard/tools/mermaid-png.sh` with a manifest describing its name, arguments, and description; exposed as a normal tool after `/reload`.

### Subagents

Configure a named, reusable subagent with its own prompt, tool scope, and step budget — for fan-out or specialized sub-tasks.

```
> /evolve add a "reviewer" subagent that audits diffs for security issues
```

---

## Tier 2 — deep evolve (recompiles core)

When a change genuinely needs new Rust in Wizard itself — a new built-in tool kind, a protocol change, a TUI panel — use `--deep`:

```
> /evolve --deep add a /status slash command showing token usage
```

The pipeline:

1. **Locate source** — `~/.wizard/src`, cloned from the repo on first use.
2. **Ensure a toolchain** — if `cargo` is absent, install it just-in-time via `rustup --profile minimal` (~0.5–1 GB, only on the first deep evolve). The default installer ships **no** toolchain, so the lightweight install stays lightweight; you pay for the compiler only if you actually deep-evolve.
3. **Propose a diff** over Wizard's own source.
4. **Approve** (skipped with `--auto`).
5. **`cargo build --release`.**
6. **`exec`-replace** the running process with the new binary.

If there's no toolchain or source and one can't be provisioned (offline, no `rustup`), deep evolve **falls back to Tier 1** and tells you so, rather than failing.

To install the toolchain eagerly at setup time (air-gapped or offline-first machines):

```bash
WIZARD_WITH_TOOLCHAIN=1 curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | bash
```

---

## Picking a tier

| You want to… | Tier | Recompile? |
|--------------|------|------------|
| Add knowledge or a workflow | Skill (1) | No |
| Add an external capability (computer use, browser, DB) | MCP (1) | No |
| Add small glue/automation | Scripted tool (1) | No |
| Add a specialized sub-worker | Subagent (1) | No |
| Change Wizard's own built-in behavior or UI | Deep (2) | Yes |

Rule of thumb: if an MCP server or script can do it, stay in Tier 1 — it's instant, reversible, and works on every install. Reach for `--deep` only when the capability has to live inside the binary.

---

## Logging and rollback

Every evolution — tier 1 or 2 — is appended to `~/.wizard/evolution.jsonl` with a timestamp, the change, and (for deep evolve) the diff and build result. Tier-1 changes are plain files under `~/.wizard/`; delete the file and `/reload` to revert. Deep evolve keeps the prior binary so you can roll back to it.

---

## Safety

`/evolve` widens what the agent can do to your machine — review what it adds. MCP servers and scripted tools run with **your** privileges and can make their own network and system calls. In **sovereign mode**, `/evolve` changes are auto-approved along with everything else; only run unattended evolution on machines and tasks where that's acceptable. See the [security model](architecture.md#security-model).
