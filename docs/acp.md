# ACP (editor embedding)

Wizard speaks the [Agent Client Protocol](https://agentclientprotocol.com/),
so ACP editors — **Zed**, **Neovim** (CodeCompanion / avante), **Emacs** — can
drive it as their coding agent. Same agent core as the TUI and the browser GUI;
the surface is your editor on the other end of a pipe.

```bash
wizard acp
```

Runs an ACP agent over stdin/stdout. It loads your `~/.wizard` config (so it
uses whatever provider and model you've set) but never onboards or opens a TUI —
stdin and stdout carry the JSON-RPC protocol. You normally don't run it by hand;
you point an editor at the command.

## Wiring it into an editor

The command an editor needs is `wizard acp`. In **Zed** (`settings.json`):

```json
{
  "agent_servers": {
    "Wizard": {
      "command": "wizard",
      "args": ["acp"]
    }
  }
}
```

Neovim and Emacs ACP clients take the same command in their own config. Point
the editor at a project directory; each conversation you open becomes a Wizard
session rooted at that directory (`session/new` carries the cwd, and every tool
resolves paths against it).

## What you get

Per turn, Wizard streams back as it works:

- **Assistant text** and **reasoning** (`agent_message_chunk` /
  `agent_thought_chunk`) as they stream.
- **Tool calls** (`tool_call` / `tool_call_update`) — each file read, edit,
  shell command, search, or fetch shows up in the editor's tool view with a
  title, a kind (read / edit / execute / search / fetch), and a
  running → completed/failed status.
- A **stop reason** when the turn ends (`end_turn`, `cancelled`,
  `max_turn_requests`).

Cancelling in the editor (`session/cancel`) interrupts the turn cooperatively at
the next stream or tool boundary. Wizard runs tools without a per-action
approval prompt, so it does its own file and shell I/O and never has to ask the
editor for permission.

## Protocol and scope

Wire protocol **V1** (`agent-client-protocol` 0.10.4). Implemented:
`initialize`, `session/new`, `session/prompt`, `session/cancel`. Because the
crate's connection futures are `!Send`, the server runs on a single-threaded
`LocalSet`; the agent's own turns still use the multi-thread runtime underneath.

Text prompts only for now. Not yet surfaced over ACP: image prompts, the plan
panel (todos), background tasks and subagent runs, token-usage updates, and
client-delegated file/terminal operations — Wizard performs its own I/O rather
than routing it through the editor. These are additive and can come later
without changing the wiring above.
