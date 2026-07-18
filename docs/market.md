# Fork and distribute

Wizard is a self-owning agent: when it modifies its own source (a deep evolve, tier 2), you can publish that variant as a GitHub fork under your account and hand anyone a one-line command that installs your Wizard.

---

## The flow

Deep evolve → publish:

1. `/evolve --deep` proposes and builds a change to Wizard's own Rust source. The source lives at `~/.wizard/src` (cloned from the repo on first use; committed by each deep evolve). Full walkthrough in [docs/evolve.md](evolve.md).

2. After a successful deep evolve, or any time you want to share the version currently at `~/.wizard/src`, run `/publish` in the TUI, call the `publish` tool in a prompt, or run `wizard --publish` from the shell.

3. Wizard forks `teddytennant/wizard` to your GitHub account (or reuses an existing fork), pushes the committed source from `~/.wizard/src` to a branch on the fork (default branch: `main`), and prints the install one-liner for your fork.

4. Anyone who runs that one-liner gets your Wizard, built from your source, installed as the `wizard` binary.

---

## Prerequisites

Publish requires the GitHub CLI (`gh`) installed and authenticated:

```bash
gh auth login
```

Wizard checks `gh auth status` before doing anything and tells you what to fix if authentication is missing. It never invents or stores credentials.

---

## The install one-liner

```
curl -fsSL https://raw.githubusercontent.com/<owner>/wizard/<ref>/install.sh | WIZARD_REPO=<owner>/wizard WIZARD_REF=<ref> WIZARD_BUILD_FROM_SOURCE=1 bash
```

`/publish` prints this line with `<owner>` and `<ref>` filled in.

| Env var | Default | Meaning |
|---------|---------|---------|
| `WIZARD_REPO` | `teddytennant/wizard` | GitHub repo to install from, as `owner/repo`. |
| `WIZARD_REF` | `main` | Branch or tag to clone and build. |
| `WIZARD_BUILD_FROM_SOURCE` | `0` | Set to `1` to build the binary from source instead of downloading a release asset. Fork installers always set this to `1`, since forks don't ship prebuilt release binaries unless you cut releases yourself. |

The installer clones your fork at `WIZARD_REF`, ensures a Rust toolchain (installs via `rustup --profile minimal` if `cargo` is absent), runs `cargo build --release`, and places the resulting binary. It works on any machine with internet access and a supported OS (Linux and macOS, x86_64 and aarch64). Build time is a few minutes the first time.

---

## What the recipient installs

Running your one-liner installs:

- **Your source code**: the Rust that came out of your deep evolve, committed at `~/.wizard/src`.
- **Your WIZARD.md charter**: the behavioral charter ([WIZARD.md](../WIZARD.md) at the repo root) that governs how Wizard behaves. It is compiled into the binary and injected into every system prompt, so your fork ships your copy of it.
- **Your defaults**: any configuration baked into the source.

Tier-1 evolutions (skills, MCP server registrations, scripted tools, subagents) live under `~/.wizard/` on your machine and are not pushed by `/publish`. Publish is for source changes only.

---

## Gated and logged

Publish is logged like deep evolve, and both run `/publish` directly with no approval gate. Genie narrates the fork target, branch, and source commit as it proceeds; sovereign publishes as part of its unattended flow.

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

See [WIZARD.md](../WIZARD.md) for the current charter.

---

## When to publish

Skills, MCP servers, scripted tools, and subagents are runtime additions that do not touch Wizard's source, so `/publish` will not do anything useful with them alone. To share a skill or MCP setup, document the configuration instead.

Reach for `/publish` when the change is in `~/.wizard/src`: a new built-in tool, a protocol change, a TUI feature, an amended charter.
