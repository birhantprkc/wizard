# `wizard sync`: move state between machines

`wizard sync` packs Wizard's portable state — config, skills, custom commands, subagents, and scripted tools — into a signed bundle that another machine can verify and apply.

| Command | What it does |
|---------|--------------|
| `wizard sync pack [--out <path>] [--include-credentials]` | Write a signed `.tar.gz` bundle (default `wizard-sync-<YYYYMMDD>.tar.gz` in the current directory); prints the path, file count, size, and signing-key fingerprint |
| `wizard sync pull <source> [--dry-run]` | Verify a bundle (file path or http(s) URL, downloaded if needed), print a new / changed / identical summary, then apply it; `--dry-run` stops after the summary. With no `<source>`, uses `source` from `[sync]` in `~/.wizard/config.toml` |
| `wizard sync key` | Print this machine's sync public key (base64) and its fingerprint (`SHA256:…`, OpenSSH-style), generating the keypair on first use |

## Quickstart

On the machine that has the state (a laptop, say):

```bash
wizard sync pack
scp wizard-sync-20260709.tar.gz workbox:
```

On the other machine:

```bash
wizard sync pull wizard-sync-20260709.tar.gz
```

One command each side. Pull verifies the bundle, prints what is new, changed, and identical, backs up anything it overwrites, and applies. The first pull pins the sender's signing key; compare the fingerprint it prints against `wizard sync key` on the source machine (see [Trust model](#trust-model)). To preview without changing anything, add `--dry-run`: it runs the full verification and prints the same summary, then stops.

## What's in a bundle

Paths are relative to `~/.wizard/`. Pieces missing on the packing machine are skipped silently.

| Contents | Bundled |
|----------|---------|
| `config.toml`, `mcp.toml`, the system prompt file | always |
| `skills/`, `commands/`, `subagents/`, `tools/` | always |
| `credentials.toml`, `xai_oauth.json` (API keys, OAuth tokens) | only with `--include-credentials` |
| `sessions/`, `logs/`, `models/`, `memory/`, `sync/`, the evolution log, caches | never |

`--include-credentials` puts your API keys and OAuth tokens in the bundle. The bundle file is then written with mode 0600 and pack prints a warning: the bundle is signed but **not encrypted**, so anyone who obtains it can read the keys. Transfer such a bundle privately (`scp`), never over a public URL.

## Trust model

Bundles are signed with ed25519. Each machine generates a signing keypair on its first `pack`; the seed lives at `~/.wizard/sync/key` (mode 0600). The bundle embeds the sender's public key, and `manifest.sig` signs the manifest, which lists the sha256 of every file.

Verification on `pull` is all-or-nothing: the signature, then the trust check, then every file hash. Nothing is written to `~/.wizard/` until everything passes.

Trust is on first use, like SSH:

- The first `pull` on a machine pins the sender's public key into `~/.wizard/sync/trusted_keys` and prints its fingerprint. Compare it against `wizard sync key` on the source machine.
- Later pulls require the bundle's key to already be in `trusted_keys`; a bundle signed by an unknown key is rejected.
- To trust an additional machine, add its public key (`wizard sync key` there) as a line in `trusted_keys`.

## Pulling from a URL

`<source>` can be an http(s) URL; the bundle is downloaded and then verified exactly like a local file. To make the pull side a single command, set the source in `~/.wizard/config.toml`:

```toml
[sync]
source = "https://example.com/wizard-sync.tar.gz"  # or a file path; used when `wizard sync pull` gets no argument
```

Then `wizard sync pull` with no argument pulls from there. Host the bundle somewhere private, and never publish a bundle made with `--include-credentials`: it is signed, not encrypted.

## Backups

Existing files a pull would overwrite are first copied to `~/.wizard/sync/backups/<timestamp>/`; restore by copying them back. Pull is additive: it overwrites and adds, never deletes, so local files absent from the bundle are left alone. There are no interactive prompts, consistent with the rest of Wizard.

## Limitations

- Not continuous sync. A bundle is a snapshot; last write wins, and there is no merge or conflict resolution.
- Pull never deletes, so removing a skill on one machine won't remove it elsewhere. Delete it on each machine by hand.
- Bundles are signed, not encrypted.
