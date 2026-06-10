# Fork and distribute — self-ownership

Wizard is a self-owning coding agent. "Self-owning" means this: when Wizard modifies its own source (a deep evolve, tier 2), you can publish that exact variant as your own GitHub fork and hand anyone a one-line command that installs *your* Wizard. The upstream project is one possible Wizard; yours is another.

---

## The flow

Deep evolve → publish:

1. `/evolve --deep` proposes and builds a change to Wizard's own Rust source. The source lives at `~/.wizard/src` (cloned from the repo on first use; committed by each deep evolve). Full walkthrough in [docs/evolve.md](evolve.md).

2. After a successful deep evolve — or any time you want to share the version currently at `~/.wizard/src` — run `/publish` in the TUI, call the `publish` tool in a prompt, or run `wizard --publish` from the shell.

3. Wizard forks `teddytennant/wizard` to your GitHub account (or reuses an existing fork), pushes the committed source from `~/.wizard/src` to a branch on the fork (default branch: `main`), and prints the install one-liner for your fork.

4. Anyone who runs that one-liner gets your Wizard, built from your source, installed as the `wizard` binary.

---

## Prerequisites

Publish requires the GitHub CLI (`gh`) installed and authenticated:

```bash
gh auth login
```

Wizard checks `gh auth status` before doing anything and tells you exactly what to fix if authentication is missing. It never invents or stores credentials.

---

## The install one-liner

```
curl -fsSL https://raw.githubusercontent.com/<owner>/wizard/<ref>/install.sh | WIZARD_REPO=<owner>/wizard WIZARD_REF=<ref> WIZARD_BUILD_FROM_SOURCE=1 bash
```

`/publish` prints this line with `<owner>` and `<ref>` filled in. Here is what each piece does:

| Env var | Default | Meaning |
|---------|---------|---------|
| `WIZARD_REPO` | `teddytennant/wizard` | GitHub repo to install from, as `owner/repo`. |
| `WIZARD_REF` | `main` | Branch or tag to clone and build. |
| `WIZARD_BUILD_FROM_SOURCE` | `0` | Set to `1` to build the binary from source rather than downloading a release asset. Fork installers always set this to `1` because forks do not ship prebuilt release binaries unless you cut releases yourself. |

The installer clones your fork at `WIZARD_REF`, ensures a Rust toolchain (installs via `rustup --profile minimal` if `cargo` is absent), runs `cargo build --release`, and places the resulting binary. It works on any machine with internet access and a supported OS (currently Linux x86_64/aarch64). Build time is a few minutes the first time.

---

## What the recipient installs

Running your one-liner installs:

- **Your source code** — the Rust that came out of your deep evolve, committed at `~/.wizard/src`.
- **Your WIZARD.md charter** — the behavioral charter ([WIZARD.md](../WIZARD.md) at the repo root) that governs how Wizard behaves. It is compiled into the binary and injected into every system prompt. Because it lives in the source, your fork inherits your copy of it. Edit `WIZARD.md` before publishing and whoever installs your fork gets your rules.
- **Your defaults** — any configuration baked into the source.

Tier-1 evolutions (skills, MCP server registrations, scripted tools, subagents) live under `~/.wizard/` on your machine and are **not** pushed by `/publish`. Publish is for source changes only.

---

## Gated and logged

Like deep evolve, publish is gated:

- In **genie mode**: `/publish` shows you the fork target, branch, and source commit before proceeding. You confirm before anything is pushed.
- In **sovereign mode**: publish auto-approves (same standing consent as all sovereign actions).

Every publication is appended to `~/.wizard/evolution.jsonl` alongside deep-evolve records, with the fork repo, branch, and the short commit SHA that was pushed.

---

## Amending your charter

`WIZARD.md` at the repo root is Wizard's operating charter. A fork inherits the upstream charter and may amend it:

```bash
# Edit the charter in your source checkout
$EDITOR ~/.wizard/src/WIZARD.md
```

Then rebuild and push:

```
> /evolve --deep rebuild with the updated charter
> /publish
```

Whoever installs your fork gets a Wizard with your rules. See [WIZARD.md](../WIZARD.md) for the current charter.

---

## Publish is not for Tier-1 evolutions

Skills, MCP servers, scripted tools, and subagents are runtime additions that live under `~/.wizard/` and do not touch Wizard's source. `/publish` will not do anything useful with them alone. If you want to share a skill or MCP setup, the right path is to document the configuration — not to publish a fork.

Reach for `/publish` when the change is in `~/.wizard/src`: a new built-in tool, a protocol change, a TUI feature, an amended charter. That is what makes a fork meaningfully different from the upstream.
