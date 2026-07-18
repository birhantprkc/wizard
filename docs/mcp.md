# MCP

Wizard speaks the [Model Context Protocol](https://modelcontextprotocol.io/)
in **both directions**.

- **As a client** it connects to external MCP servers declared in
  `~/.wizard/mcp.toml` and merges their tools into the registry with no
  rebuild. That is the path for computer use, browser control, and databases.
  See [Self-extension](evolve.md#mcp-servers).
- **As a server** it exposes its own native tools over stdio, so any MCP
  client (Claude Code, Cursor, another Wizard) can call them. That is what
  this page covers.

## `wizard mcp-serve`

```bash
wizard mcp-serve
```

Runs a Model Context Protocol server on stdin/stdout, advertising Wizard's
native tools:

`read_file`, `write_file`, `edit_file`, `list_files`, `search_files`, `execute`,
`git_status`, `git_diff`, `memory`, `todo`, `web_fetch`, `web_search`,
`generate_image`, `task_output`, `task_kill`, `subagent_status`, `subagent_kill`,
and `run_command`.

It is self-contained: no config load, no onboarding, no LLM. It serves until
stdin closes. Note that `run_command` is only useful inside an interactive
Wizard surface that drains the slash-command queue; over plain MCP it will
refuse most calls because there is no TUI/GUI attached.

Tools run in the directory the server starts in; pass `--cwd <dir>` to serve a
specific project. Add `--scripted` to also advertise agent-authored scripted
tools from `~/.wizard/tools/`. Agent-loop-only tools such as `spawn_subagent`,
`exit_plan`, `interview`, `evolve`, and `publish` are **not** on this server.

```bash
wizard --cwd ~/code/myproject mcp-serve --scripted
```

## Wiring it into a client

Point any stdio-transport MCP client at the command. For Claude Code, in
`~/.mcp.json` (or a project `.mcp.json`):

```json
{
  "mcpServers": {
    "wizard": {
      "command": "wizard",
      "args": ["--cwd", "/abs/path/to/project", "mcp-serve"]
    }
  }
}
```

The client then sees Wizard's tools alongside its own.

## Protocol

Newline-delimited JSON-RPC 2.0 over stdio, protocol revision `2025-03-26`
(the same revision Wizard's client speaks). Methods answered:

| Method | Result |
| --- | --- |
| `initialize` | `protocolVersion`, `capabilities.tools`, `serverInfo` (`wizard` + version) |
| `tools/list` | every native tool as `{ name, description, inputSchema }` |
| `tools/call` | dispatches to the registry; returns `content` blocks + `isError` |
| `ping` | `{}` |

A tool that runs but reports failure (missing file, non-zero exit) returns a
normal result with `isError: true`. A call that cannot be carried out at all
(unknown tool, unparseable arguments) returns a JSON-RPC error. Notifications
(no `id`, e.g. `notifications/initialized`) are accepted and not answered.

## Scope

The server is intentionally minimal: stdio only (no HTTP/SSE transport), no
auth, and it does not chain the tools of MCP servers Wizard is itself a client
of, it advertises Wizard's own tools. Run it behind a client you trust; the
`execute` tool runs shell commands with your privileges, exactly as it does
inside a Wizard session.
