# Commands

Wizard's built-in `/commands`, the custom ones you define as markdown files, and the `@path`
tokens that inline file contents. All three live in `src/commands.rs`, and every surface —
TUI, browser GUI, headless `-p` — reads them from there.

## Built-in commands

`COMMANDS` in `src/commands.rs` is the single source of truth: what a command is called, what
it does, and how each surface runs it. The TUI's suggestion popup, the GUI's slash menu
(`GET /api/commands`), and the allowlist the *agent* may invoke through its `run_command` tool
are all derived from it. Two hand-kept lists is how two surfaces drift into offering
different commands; there is one.

The TUI runs every command — it is the surface they were written against. The GUI is the
constrained one, and each command declares what it is there:

| | command | browser GUI |
|---|---|---|
| **Against the agent** | `/model`, `/mode`, `/genie`, `/sovereign`, `/effort`, `/plan`, `/omakase`, `/compact`, `/btw`, `/fork`, `/goal`, `/status`, `/cost`, `/memory`, `/doctor`, `/bashes`, `/agents`, `/reload`, `/rewind`, `/fusion`, `/ultra`, `/server`, `/evolve`, `/publish`, `/help` | `server` — a `command` frame; the reply is a `notice` in the chat |
| **The page's own** | `/clear`, `/diff`, `/todos`, `/subagents`, `/dashboard`, `/resume`, `/settings`, `/provider`, `/login` | `client` — a panel, an overlay, a list |
| **Terminal only** | `/vim`, `/quit`, `/exit` | `unavailable` — refused, with what the command is and why a browser is not where it runs |

Where the two surfaces differ, the reason is the same one: **a GUI chat is its session file.**
`/clear` rotates that file, and `/resume` picks another — so in a browser they are a new chat
and the task list, not commands against the agent. `/rewind` truncates it, which is why the
GUI's is the only command that has to tell the page its transcript changed underneath it (the
`transcript_reset` frame — see `docs/gui-protocol.md`).

### What the agent may run itself

The `run_command` tool lets the model invoke these commands. Two gates apply, in order:

1. `SlashCommand::agent_runnable` — the same on every surface. It refuses the interactive
   pickers without their argument (`/effort` alone; `/effort high` is fine), the
   session-ending and destructive commands (`/quit`, `/clear`, `/rewind`, `/resume`), and the
   ones that reach outside the session to set the tool up (`/provider`, `/login`, `/publish`,
   `/evolve`, `/server`).
2. The surface's dispatch set — every command on the TUI, the `server` ones the executor
   implements on the GUI, **none at all** headless (nothing there would drain the queue, so
   the tool refuses rather than report a success that never happens).

A command that fails either gate is refused **in the tool result**, which is the only thing
the model reads before the turn ends. It is never silently dropped.

## Custom slash commands

Two ways to put reusable text in front of the model: `/commands` you define as markdown files, and `@path` tokens that inline file contents. Both work identically in the TUI and in headless `-p` runs: one shared preprocessing pipeline (`src/commands.rs`) handles them.

A custom command is a markdown file whose body is a prompt template:

- `~/.wizard/commands/*.md`: global, available in every project
- `<project>/.wizard/commands/*.md`: per project, shadows a global command with the same name

The file stem is the command name: `review.md` defines `/review`. An optional frontmatter block (the same `---`-fenced convention as skills) carries a `description` shown in the TUI suggestion popup:

```markdown
---
description: review a file against the project conventions
---
Review $1 carefully. Check it against the conventions in @WIZARD.md.
Focus on: $ARGUMENTS
```

### Placeholders

| Placeholder | Expands to |
|-------------|------------|
| `$ARGUMENTS` | everything typed after the command name |
| `$1` … `$9` | the whitespace-split positional arguments (missing ones expand to the empty string) |

Expansion is a single pass: `$`-like text inside the arguments themselves is never re-expanded.

### Invocation

- **TUI:** type `/review src/app.rs`. Custom commands show up in the same suggestion popup as builtins (builtins win a name collision). The transcript shows what you typed; the model sees the expanded template.
- **Headless:** `wizard -p "/review src/app.rs"` expands exactly the same way.
- A `/word` that matches no builtin and no custom command is passed to the model as a normal prompt.
- `/reload` picks up new and edited command files without a restart.

## @file references

Any whitespace-delimited token of the form `@path` whose path resolves to an existing file expands to a fenced code block with the file's contents:

```
explain @src/main.rs and how it relates to @docs/architecture.md
```

- Paths resolve relative to the project root; absolute paths and `~/` work too.
- Contents are capped at 50KB per file, with a truncation note when cut.
- Image files (`.png .jpg .jpeg .gif .webp`) expand to a short `[image: name]` placeholder and are attached for vision-capable models (xAI Grok, OpenAI, Anthropic, OpenRouter, Ollama vision models). You can also paste an image path or a `data:image/...;base64,...` URL into the composer.
- **Paste an image from the clipboard** (a screenshot, a copied picture) and it attaches directly, shown in the composer as `[Image #1]`, `[Image #2]`, … like Claude Code. If your terminal doesn't forward the paste, press **Ctrl-V** to pull the image off the clipboard. Reading the clipboard uses `wl-paste`/`xclip` on Linux, `pngpaste` or AppleScript on macOS, and PowerShell on Windows.
- A token that does not resolve to a file is left untouched, so email addresses (`user@host`) and decorators pass through. `@@path` escapes a literal `@path`.
- **TUI:** Tab completes the path under the cursor from its directory listing after you type `@`.

The expansion happens before the prompt reaches the agent, so the file contents land in the conversation history (and survive into the session file) like any other user text.
