# Contributing

Thanks for wanting to improve Wizard. Short guide so PRs land cleanly.

## Setup

Rust stable (edition 2024). Clone and build:

```bash
git clone https://github.com/teddytennant/wizard
cd wizard
cargo build --release
./target/release/wizard
```

Or `nix develop` for a shell with the Rust toolchain and `llama-cpp`.

## Before you open a PR

Match what CI runs (see `.github/workflows/ci.yml`):

```bash
contrib/check-file-size.sh
cargo machete
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --release --locked
```

Optional, if you touch the desktop shell (`wizard app`):

```bash
cargo check --locked --features desktop
```

Optional supply-chain check:

```bash
cargo deny --locked --all-features check
```

Keep `Cargo.lock` in sync (`--locked` fails on drift). Prefer small, focused diffs that match existing style. No `todo!()` / bare `unwrap()` on fallible paths.

## What to send

- Bug fixes and tests
- Docs that match the code
- Focused features that fit the single-binary design

Open an issue first for large or architectural changes. Security-sensitive reports: see [SECURITY.md](SECURITY.md).

## Behavior and docs

- [WIZARD.md](WIZARD.md) is the agent charter forks inherit; change it only when the behavior change is intentional.
- User-facing docs live under `docs/`. Update them when you change commands, flags, or flows.
- Read [SECURITY.md](SECURITY.md) before changing tools, hooks, MCP, install, or trust boundaries.

## License

By contributing, you agree your work is licensed under the MIT license ([LICENSE](LICENSE)).
