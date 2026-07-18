---
name: evolve
description: How to extend Wizard's own capabilities via /evolve. Picking the cheapest tier, the four runtime channels (skills, MCP servers, scripted tools, subagents), and when a deep recompile is justified.
---

# Self-extension (/evolve)

When asked to give yourself a new capability, add it through the cheapest
tier that works. Tier 1 changes are plain files under `~/.wizard/`, take
effect on `/reload`, and are reverted by deleting the file. Tier 2
(`--deep`) recompiles Wizard itself and should be rare.

## Picking a tier

| The capability is… | Channel | Recompile? |
|--------------------|---------|------------|
| Knowledge, a workflow, domain guidelines | Skill | No |
| External (computer use, browser, database, search) | MCP server | No |
| Small glue or project automation | Scripted tool | No |
| A specialized sub-worker with its own prompt/budget | Subagent | No |
| A change to Wizard's own built-in behavior or UI | Deep (`--deep`) | Yes |

Rule of thumb: if an MCP server or a script can do it, stay in Tier 1. It's
instant, reversible, and works on every install. Reach for `--deep` only
when the capability must live inside the binary.

## Tier 1 channels

### Skills

Write `~/.wizard/skills/<name>/SKILL.md`: optional `---` frontmatter with
`name` and `description`, then a markdown body that is injected into the
system prompt. Keep skills focused and imperative: instructions, not
essays. User skills shadow bundled ones with the same name.

### MCP servers

Append a `[[server]]` block to `~/.wizard/mcp.toml`:

```toml
[[server]]
name = "computer-use"
transport = "stdio"        # or "http" with url = "..."
command = "uvx"
args = ["mcp-computer-use"]
# env = { API_KEY = "..." }
```

On `/reload`, Wizard connects and merges the server's tools into the
registry (name collisions become `server__tool`). Verify the `command` (or
`url`) actually exists on this machine before registering it.

### Scripted tools

Author a script plus a TOML manifest, both in `~/.wizard/tools/`. The
manifest is `<name>.toml` beside the script:

```toml
name = "mermaid-png"
description = "Render a mermaid diagram source file to a PNG"
script = "mermaid-png.sh"   # filename, relative to ~/.wizard/tools/
# interpreter = "bash"      # optional; otherwise the script must be executable
# timeout_secs = 60

[parameters]                # JSON Schema for the arguments
type = "object"
required = ["input", "output"]
[parameters.properties.input]
type = "string"
description = "Path to the .mmd source file"
[parameters.properties.output]
type = "string"
description = "Path for the rendered PNG"
```

The tool receives its arguments as a single JSON object string in `argv[1]`,
parsed inside the script (e.g. with `jq`). Print results to stdout and
exit non-zero on failure. Test the script once with `execute` before
declaring the evolution done.

### Subagents

Write `~/.wizard/subagents/<name>.toml`:

```toml
name = "reviewer"
description = "Audits diffs for security issues"
system_prompt = "You are a security reviewer. Examine the diff for injection, authz, and secret-handling flaws. Report findings with file:line."
# tool_scope = ["read_file", "search_files", "git_diff"]  # omit = all tools
# max_steps = 0  # default: unlimited; set a positive number only to hard-cap
```

Keep `tool_scope` as narrow as the job allows; subagents run with
auto-approval inside their isolated context.

## Tier 2, deep evolve

Only when the change requires new Rust in Wizard's core (a new built-in
tool kind, a protocol change, a TUI panel). The pipeline: source checkout
at `~/.wizard/src` (cloned on first use), just-in-time minimal Rust
toolchain if `cargo` is absent, propose a diff, `cargo build
--release`, then exec-replace the running process. If no toolchain or
source can be provisioned, fall back to Tier 1 and say so. Do not fail.

## Always

- Every evolution is logged to `~/.wizard/evolution.jsonl`; tell the user
  what was added and where the file lives so they can review or delete it.
- One file (or one config block) per capability. Keep evolutions
  independently reversible.
- After writing the files, `/reload` and confirm the new capability is
  actually present (skill in prompt, tool in registry) before reporting
  success.
- Safety: MCP servers and scripted tools run with the user's privileges
  and can make their own network and system calls. Prefer well-known
  servers and small auditable scripts; never add a capability the user did
  not ask for.
