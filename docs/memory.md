# Memory

Wizard remembers things about a project across sessions. Each memory is a markdown file
the agent writes with the native `memory` tool; the index of them is injected into the
system prompt at the start of every session, so a fact learned today is in front of the
model tomorrow without anyone re-explaining it.

Memory is per project, keyed on the project root, and lives entirely under
`~/.wizard/memory/` — nothing is written into your repo.

## On disk

```
~/.wizard/memory/<project-slug>/
  MEMORY.md              # the index, regenerated from the entry files
  prefers-rust.md        # one memory per file
  release-flow.md
```

The slug is the project root with every non-alphanumeric character replaced by `-`
(`/home/you/app` → `-home-you-app`). An entry file:

```markdown
---
name: release-flow
description: tags drive releases; CI publishes, nobody publishes by hand
metadata:
  type: project
---

Releases go out from a tag on `main`. Cutting the tag is the whole release: CI
builds and publishes. See [[ci-gates]] for what has to be green first.
```

`MEMORY.md` is derived, never appended to — every save and delete regenerates it from
the files, so it cannot drift from what is actually stored:

```
- [ci-gates](ci-gates.md) [project] — what CI blocks a merge on
- [release-flow](release-flow.md) [project] — tags drive releases; CI publishes, nobody publishes by hand
```

## Types

Every memory is classified, and the type is what makes recall selective — the model
reads the index, sees the types, and knows which entries bear on the turn.

| Type | What belongs in it |
|------|--------------------|
| `user` | Who the user is: role, expertise, standing preferences |
| `feedback` | How the agent should work: corrections *and* confirmed approaches, always with the why |
| `project` | Ongoing work, goals, and constraints not derivable from the code or git history; relative dates written as absolute ones |
| `reference` | A pointer to an external resource: a URL, a dashboard, a ticket |

## Links

A memory's body can point at related memories by name, `[[wiki-style]]`. A `read`
resolves them and reports which are saved, so a recall can follow the trail. A link to a
memory that does not exist yet is not an error — it marks something worth writing later.
There is no link database; resolution is a file lookup in the memory directory.

## What not to save

A memory has to earn its place, or the store becomes a junk drawer nobody trusts. The
rules are in the tool description and the system prompt, so the model applies them
before every save:

- Never save what the repo already records: code structure, past fixes, anything in the
  git history.
- Never save what only matters to the current conversation.
- Before saving, look for a memory that already covers the same ground and update it
  (save over its name) instead of creating a near-duplicate.
- Delete a memory that turns out to be wrong.

## Seeing and managing them

`/memory` in the TUI (and in the browser GUI) is the human's window onto the store:

| Command | What it does |
|---------|--------------|
| `/memory` | List the saved memories: name, type, description |
| `/memory read <name>` | Show one memory's full content |
| `/memory forget <name>` | Delete one memory |

The agent may run these itself through `run_command` — every one of them is something
the `memory` tool already lets it do.

## Compatibility

Memory files written before types existed have no `type` in their frontmatter. They
still load, as `project` memories, and keep their description in the regenerated index.
