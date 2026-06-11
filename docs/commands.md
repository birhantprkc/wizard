# Custom commands and @file references

Two ways to put reusable text in front of the model: `/commands` you define as markdown files, and `@path` tokens that inline file contents. Both work identically in the TUI and in headless `-p` runs — one shared preprocessing pipeline (`src/commands.rs`) handles them.

## Custom slash commands

A custom command is a markdown file whose body is a prompt template:

- `~/.wizard/commands/*.md` — global, available in every project
- `<project>/.wizard/commands/*.md` — per project, shadows a global command with the same name

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

- **TUI:** type `/review src/app.rs` — custom commands show up in the same suggestion popup as builtins (builtins win a name collision). The transcript shows what you typed; the model sees the expanded template.
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
- Image files (`.png .jpg .jpeg .gif .webp`) are replaced by a note — this build has no vision path to attach them to.
- A token that does not resolve to a file is left untouched, so email addresses (`user@host`) and decorators pass through. `@@path` escapes a literal `@path`.
- **TUI:** Tab completes the path under the cursor from its directory listing after you type `@`.

The expansion happens before the prompt reaches the agent, so the file contents land in the conversation history (and survive into the session file) like any other user text.
