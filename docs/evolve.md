# Self-extension (`/evolve`)

Wizard can extend itself. `/evolve` lets the agent add new capabilities: a skill, an external tool server, a LuaJIT scripted tool, a subagent, or, when needed, new Rust in its own core.

The design borrows from the two self-modifying agents that pioneered this pattern:

- **[Pi](https://newsletter.pragmaticengineer.com/p/building-pi-and-what-makes-self-modifying)** modifies its own installed source in place and `/reload`s it live, which works because it's interpreted (Node/TS) and has no compile step.
- **[Hermes](https://hermes-agent.nousresearch.com/docs/)** never recompiles. It adds capability through portable skills, MCP servers (the channel for things like computer use), programmatic scripted tools (`execute_code`), and isolated subagents.

Wizard is compiled Rust for the core and the TUI — you do **not** need a TypeScript interpreter or a bloated Electron runtime to extend it. Tier-1 scripted tools run through **embedded LuaJIT**, the just-in-time compiler, in-process: Pi-style live reload for glue, without shipping Node. Deep evolve still recompiles only when a change has to live in the Rust binary. Two tiers.

---

## Tier 1: runtime extension (default, no recompile)

Works on every install, including the one-line `curl | bash` binary. `/evolve` writes config/data under `~/.wizard/` and `/reload` activates it live. Four channels:

### Skills

A Markdown file of guidelines, workflows, or domain knowledge, injected into the system prompt.

```
> /evolve add a skill for writing conventional commit messages
```

Wizard writes `skills/conventional-commits/SKILL.md` and reloads it into context.

### MCP servers

The path for capabilities that live outside Wizard: computer use, browser control, databases, search, anything shipped as an [MCP](https://modelcontextprotocol.io) server. Wizard is an MCP client; registering a server merges its tools into the registry with no rebuild.

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

### Scripted tools (LuaJIT by default)

The agent authors a small script (the Hermes `execute_code` analog), saved to `~/.wizard/tools/` and run by Wizard. **The default runtime is embedded LuaJIT** — the just-in-time compiler ships inside the binary, so evolve glue needs no `bash`/`python`/`node` on `PATH` and does not spawn a child interpreter.

```
> /evolve add a tool that slugifies a string
```

Saved as `~/.wizard/tools/slugify.lua` with a manifest; exposed as a normal tool after `/reload`. Arguments arrive as the Lua global `args`; print results with `print(...)` (or `return` a value). Host helpers live under `wizard` (`read_file`, `write_file`, `json_encode`, `json_decode`, `runtime`).

```toml
# ~/.wizard/tools/slugify.toml
name = "slugify"
description = "Slugify a string"
script = "slugify.lua"
runtime = "luajit"

[parameters]
type = "object"
required = ["text"]
[parameters.properties.text]
type = "string"
```

```lua
-- ~/.wizard/tools/slugify.lua
local s = tostring(args.text or ""):lower()
s = s:gsub("[^%w]+", "-"):gsub("^%-", ""):gsub("%-$", "")
print(s)
```

Shell, Python, and Node scripts still work when you set an external `interpreter` in the manifest — LuaJIT is the default, not a mandate.

### System prompt override

The baked-in base personality prompt can be replaced at runtime by a file: `~/.wizard/system_prompt.md` (or the path in `$WIZARD_SYSTEM_PROMPT`, which wins). When present and non-empty, its contents replace the compiled prompt for the active mode; absent, behavior is identical to the default. The bundled `WIZARD.md` charter, skills, project instructions, and memory sections are always appended on top, so this override tunes personality/instructions without dropping the charter. This is the surface external harness-evolution tooling (e.g. AHE) mutates to measure and improve prompt quality.

### Harness bundles

The full evolvable surface, not just the prompt, can be externalized as a *harness bundle*: a directory activated with `--harness-dir <dir>` (or `$WIZARD_HARNESS_DIR`) whose files shadow the compiled defaults per component:

```
<bundle>/
  system_prompt.md            # base personality prompt (highest-precedence override)
  tool_descriptions/<tool>.md # description advertised to the model for that native tool
  skills/<name>/SKILL.md      # shadows bundled and user skills by name
  subagents/<name>.toml       # shadows user-defined and built-in subagents by name
  HARNESS.md                  # generated guide for evolution agents
```

Any missing or empty file falls back to the compiled default, so a partial or broken bundle degrades gracefully and deleting a file reverts that component. `wizard harness export <dir>` dumps the current compiled defaults as a bundle: the seed an external harness-evolution loop (e.g. AHE) edits, measures, and hands back for review. Winning changes get baked into the source as new defaults and re-exported, which is what makes the loop recursive. Methodology credit: [Agentic Harness Engineering](https://github.com/china-qijizhifeng/agentic-harness-engineering) (arXiv:2604.25850).

The old local `wizard bench` trajectory recorder/replay runner has been removed; measuring and improving harness quality is AHE's job ([wizard-ahe](https://github.com/teddytennant/wizard-ahe)).

### Subagents

Configure a named, reusable subagent with its own prompt and tool scope, for fan-out or specialized sub-tasks. Subagents have no step ceiling by default; an optional positive `max_steps` can still hard-cap one.

```
> /evolve add a "reviewer" subagent that audits diffs for security issues
```

---

## Tier 2: deep evolve (recompiles core)

When a change needs new Rust in Wizard itself (a new built-in tool kind, a protocol change, a TUI panel), use `--deep`:

```
> /evolve --deep add a /status slash command showing token usage
```

The pipeline:

1. **Locate source**: `~/.wizard/src`, cloned from the upstream repo on first use (override the clone URL with `WIZARD_SOURCE_REPO` if you want a fork or mirror).
2. **Ensure a toolchain**: if `cargo` is absent, install it via `rustup --profile minimal` (~0.5–1 GB, first deep evolve only). The default installer ships no toolchain; you pay for the compiler only if you use this tier.
3. **Propose a diff** over Wizard's own source, in two model turns: a file-selection turn picks the relevant files from the repository listing (with a keyword-matching fallback when it fails), then the diff-authoring turn sees those files' actual contents (up to 8 files under a ~96 kB budget) so its hunks match the real source and survive `git apply --check`.
4. **`cargo build --release`**, then a **`--version` smoke test** on the fresh binary. Build or smoke failure reverts the diff and leaves the running binary alone.
5. **Install** the new binary over the running executable (keeping the prior one as `<name>.prev` in the same directory). If the install path is not writable, Wizard keeps the build under `~/.wizard/src/target/release/wizard` and runs from there instead.
6. **Restart into the new binary** when the surface supports it: the CLI path (`wizard --evolve --deep`) `exec`-replaces immediately; continuous mode writes an `evolve-reexec` marker and re-execs at a safe boundary; the interactive TUI/tool path reports the install and expects a restart (or continuous's own re-exec).

If there's no toolchain or source and one can't be provisioned (offline, no `rustup`), deep evolve falls back to Tier 1 and says so, rather than failing.

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
| Add small glue/automation | Scripted tool (LuaJIT) | No |
| Add a specialized sub-worker | Subagent (1) | No |
| Change Wizard's own built-in behavior or UI | Deep (2) | Yes |

If an MCP server or script can do it, stay in Tier 1: it's instant, reversible, and works on every install. Use `--deep` only when the capability has to live inside the binary.

---

## Logging and rollback

Every evolution, tier 1 or 2, is appended to `~/.wizard/evolution.jsonl` with a timestamp, the change, and (for deep evolve) the diff and build result. Inspect and roll back from the CLI:

```bash
# Numbered history, most recent first (#1 is the newest):
wizard evolve list

# Undo entry #N from the list:
wizard evolve undo 2
```

`undo` reverts what the entry recorded: a skill, scripted tool, or subagent undo deletes the created files (`/reload` to apply); an MCP-server undo removes its entry from `~/.wizard/mcp.toml`; a deep-evolve undo restores the `<binary>.prev` rollback copy over the installed binary (keeping the undone build beside it as `<binary>.undone`). Restart Wizard to run it. Undo is conservative: when the recorded artifacts are already gone it refuses with a clear message rather than guessing.

Everything is also plain files under `~/.wizard/`, so manual cleanup keeps working: delete the file and `/reload` to revert a tier-1 change; deep evolve keeps the prior binary as `<binary>.prev`.

---

## Safety

`/evolve` widens what the agent can do to your machine, so review what it adds. MCP servers and scripted tools run with your privileges and can make their own network and system calls. **Both modes apply `/evolve` changes directly: there is no approval gate.** Only run unattended evolution on machines and tasks where that's acceptable. See the [security model](architecture.md#security-model).
