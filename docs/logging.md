# Logs

Wizard writes diagnostics to a file and never to your terminal. Each process gets one JSONL log under `~/.wizard/logs/`, filtered by `WIZARD_LOG`.

```bash
ls -t ~/.wizard/logs/
tail -f ~/.wizard/logs/2026-08-01T10-14-22-48213.jsonl
```

## Where it goes

`~/.wizard/logs/<timestamp>-<pid>.jsonl`, one file per process. The pid is in the name so two Wizards started in the same second do not interleave into one file.

The file is created lazily, on the first record that passes the filter. A run that logs nothing (`wizard usage`, a shell completion probe, a clean session at the default filter) leaves no file behind at all, so the directory holds runs that had something to say.

That first record is also what brings `~/.wizard` and `~/.wizard/logs` into existence on a fresh install, and it creates them **mode 0700 on unix**, tightening them if an older release left them loose. This matters because most subcommands dispatch before the config is loaded and never build the state tree themselves — `mcp-serve`, `usage`, `harness`, `doctor`, `evolve`, `update`, `sync`, `schedule`, `scheduler`, `gateway <cmd>`, `fleet`, `skills`, `peers`, and `--login` — and a session log carries prompts, tool output, and error detail: created at the process umask it would be world-readable on a stock distro. On a filesystem that cannot express unix modes (exFAT, WSL DrvFs, a CIFS/NFS mount without POSIX modes) the chmod fails, a warning naming the directory is logged, and the log is still written: losing the diagnostics is the worse failure. `wizard doctor`'s **secret storage** check is what reports a loose tree afterwards.

Two limits keep the directory bounded:

- **20 session logs.** When a new session first writes, older `*.jsonl` files in the directory are deleted oldest-first until 20 remain (with room reserved for the one about to be created, so a session never prunes itself).
- **8 MiB per log.** Past that the log records one warning — `session log passed its 8388608 byte budget; later events dropped` — and drops the rest. A long `--continuous` run under `WIZARD_LOG=trace` cannot fill the disk.

The scheduler's own `scheduler.log` and `jobs/` live in the same directory ([scheduler.md](scheduler.md)) and are not touched by either limit.

## What lands there

One JSON object per line, shaped like the `tracing` events it carries:

```json
{"timestamp":"2026-08-01T10:14:22.481233Z","level":"WARN","fields":{"message":"provider 'openai' has no API key"},"target":"wizard::llm"}
```

`WIZARD_LOG` selects what is recorded, in the same directive syntax as `RUST_LOG`:

| `WIZARD_LOG` | Effect |
|--------------|--------|
| unset (default) | `off,wizard=warn`: Wizard's own warnings and errors, nothing from dependencies |
| `debug` | everything at debug and above, including dependencies (hyper, reqwest, …) |
| `wizard=debug` | Wizard at debug, dependencies still silent |
| `wizard::agent=trace,wizard=info` | per-module, most specific directive wins |
| `off` | nothing except panics |

The default disables every target that is not `wizard`, so a chatty dependency can never fill the log. An empty, blank, or unparseable value falls back to the default rather than failing: a typo in an environment variable must not stop Wizard from starting.

`RUST_LOG` is **not** consulted. Only `WIZARD_LOG`.

## Panics

A panic is appended to the same file by the panic hook, whatever `WIZARD_LOG` says and even past the 8 MiB budget, with the message, the source location, and a forced backtrace (no `RUST_BACKTRACE` needed). It is target `wizard::panic` at level `ERROR`. On the TUI's alternate screen, or inside `wizard acp` where the editor swallows the process's stderr, that record is often the only surviving evidence of the crash.

A process where logging could not be set up at all (no resolvable home directory) records nothing, including panics; that is deliberate, since there is nowhere safe to report the failure from and a missing log must never stop Wizard from starting.

## Never stdout, never stderr

Nothing in this subsystem prints. `wizard acp` and `wizard mcp-serve` speak JSON-RPC over stdout, and the TUI owns the terminal through the alternate screen, so a subscriber that printed would corrupt the protocol in the first two cases and the frame in the third. The only sink is the file, and the logging layer's own fallback for a failed write (an `eprintln!` in the underlying library) is turned off rather than relied on. If a write fails, the event is lost silently, which is the cheaper failure.

This is also why `WIZARD_LOG=debug` does not put anything on your screen. To watch it live, `tail -f` the file in another terminal.

## Sharing a log

`wizard doctor --bundle` collects the five newest logs (plus config, the latest transcript, and the usage and evolution logs) into one redacted directory for a bug report. See [doctor.md](doctor.md#bug-report-bundles). Read the bundle before you send it: redaction catches credential shapes, not your own file paths and prose.
