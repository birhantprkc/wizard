# The default loadout

A fresh Wizard install comes equipped already. Besides the binary, the installer lays down a default loadout: a browser (Playwright MCP) and a roster of four subagents. The binary itself is unchanged; the loadout is *configuration*, plain TOML files under `~/.wizard/` that you can edit, extend, or delete. Everything in [Self-extension](evolve.md), [Modes](modes.md), and [Fork and distribute](market.md) works exactly as documented.

The loadout is installed in every flavor except `WIZARD_MINIMAL=1`, which skips it. Each file is written only if it does not already exist, so nothing under `~/.wizard/` that you already have is ever overwritten: re-running the installer on an existing setup adds only what is missing.

> No release is signed yet, so `install.sh` refuses to fetch a binary and never reaches the loadout step. Until then it lands only via `WIZARD_BUILD_FROM_SOURCE=1`, or by copying `loadout/` into `~/.wizard/` yourself.

> Historical note: this loadout used to ship as a separate distribution called Wizard Arsenal. It has been absorbed into Wizard's default install; there is nothing extra to add on.

---

## A browser (Playwright MCP)

`~/.wizard/mcp.toml` declares the [Playwright MCP](https://github.com/microsoft/playwright-mcp) server:

```toml
[[server]]
name = "playwright"
transport = "stdio"
command = "npx"
args = ["-y", "@playwright/mcp@latest"]
```

This is the same browser recipe from [WIZARD.md §2](../WIZARD.md), shipped ready instead of acquired via `/evolve`. When Wizard starts (or after `/reload`), the server's navigate / click / type / snapshot tools merge into the registry, and the agent can read pages, fill forms, and do computer-use style tasks.

It requires **Node and `npx`** on your PATH. If Node is missing, the server fails to start, is skipped, and the rest of Wizard works normally; install Node, then `/reload`. The warning goes to the log file (`~/.wizard/logs/`), not the screen, so a browser that quietly is not there looks the same as one you never configured — `wizard doctor` reports MCP servers that did not connect.

## A roster of subagents

`~/.wizard/subagents/` ships four specialists the parent model can delegate to with the `spawn_subagent` tool. Each runs in an isolated context with its own tool scope and returns a single summary to the parent. None of them sets `max_steps`, so all four take the subagent default of 50 steps; add `max_steps` to a file to change that (`0` means no ceiling).

| Subagent | What it does | Tools it can use |
|----------|--------------|------------------|
| `reviewer` | Reviews a diff or files for bugs, security issues, and style; read-only. | read / search / git only |
| `researcher` | Web research via the Playwright browser; gathers facts and reports sources. | full set (incl. browser MCP) |
| `tester` | Runs the test suite, diagnoses failures, fixes code or tests. | read / write / edit / search / execute |
| `documenter` | Writes and updates docs and comments to match the code. | read / write / edit / search |

These are plain TOML files. Edit them, add your own, or delete the ones you don't want; they follow the same format as any [user-defined subagent](evolve.md). Changes take effect on the next run or `/reload`.

```
> use the reviewer subagent to review my staged changes
> have the researcher subagent find the latest Tokio release notes
```

---

## Where it lives in the repo

The canonical source for these files is the repo's [`loadout/`](../loadout) directory (`loadout/mcp.toml`, `loadout/subagents/*.toml`). `install.sh` embeds copies so the one-liner works without a repo checkout. They are identical, and a test asserts it: `install_sh_ships_the_same_subagents_as_the_loadout` reads all four TOMLs and fails if a heredoc has drifted from its file. They had drifted once — the repo's `documenter.toml` carried a `Voice:` block the installer's did not — which is why the test exists. `loadout/` is the source of truth. `loadout/config.toml.template` is a fuller annotated config reference used by external config distributions; the installer writes its own, simpler `config.toml`.

## See also

- [Getting started](getting-started.md): install flavors, tiers, first run
- [Self-extension](evolve.md): `/evolve`, subagents, MCP servers
- [WIZARD.md](../WIZARD.md): the behavioral charter (browser recipe in §2)
