# Scheduler: `wizard schedule` and `wizard scheduler`

Cron-style scheduled runs. Entries live in `~/.wizard/schedule.toml`; the
`wizard scheduler` daemon fires each one as a headless wizard child process
at its cron times. Like `bench` and `doctor`, the schedule commands are
self-contained: they never load `~/.wizard/config.toml`, never trigger
onboarding, and need no LLM in this process — the spawned jobs load their
own config exactly like a user-invoked `wizard --mode sovereign -p "..."`.

## schedule.toml

```toml
[[entries]]
name = "nightly-cleanup"        # unique key, [a-zA-Z0-9_-]+
cron = "0 3 * * *"              # standard 5-field cron, local time
prompt = "tidy the repo, run tests, commit"
cwd = "/home/user/proj"         # required: where the run executes
mode = "sovereign"              # optional, default "sovereign"; or "continuous"
max_hours = 2.0                 # optional wall-clock cap for the spawned run
enabled = true                  # optional, default true
```

Cron expressions are strict 5-field (`minute hour day month weekday`) —
no seconds or year fields — and are validated on `add` and on every load.
Aliases like `MON-FRI` and `@daily` work.

## CLI

```bash
# Add an entry (validates the cron, the mode, and that --cwd exists;
# prints the next fire time):
wizard schedule add nightly-cleanup \
    --cron "0 3 * * *" \
    --prompt "tidy the repo, run tests, commit" \
    --cwd ~/proj --max-hours 2

# Table of name, cron, enabled, next fire (absolute + relative), cwd:
wizard schedule list

# Remove by name (error if absent):
wizard schedule remove nightly-cleanup

# Toggle an entry without removing it (enabled = true/false in the file;
# the daemon picks the change up on its next pass):
wizard schedule disable nightly-cleanup
wizard schedule enable nightly-cleanup

# Run one entry's job right now, in the foreground, with inherited stdio —
# the same child command the daemon would spawn. Exits with the child's
# exit code (0 completed, 2 max-steps, 3 circuit breaker, 4 time limit):
wizard schedule run nightly-cleanup
```

You can also edit `~/.wizard/schedule.toml` by hand; the daemon picks up
changes on its next pass. `enabled = false` keeps an entry in the file
without firing it (same effect as `wizard schedule disable`).

## The daemon

```bash
wizard scheduler
```

A long-running foreground process. Exactly one instance runs per user: the
daemon takes an exclusive lock on `~/.wizard/scheduler.lock` at startup, and
a second instance exits immediately with an error naming the lock (the lock
is released by the kernel on any exit, including SIGKILL, so it can never go
stale). Each pass (at least once a minute) it:

1. Reaps finished jobs and kills any past `max_hours` (plus a short grace —
   the child also receives `--max-hours`, so the normal path is a graceful
   self-stop; the kill is the backstop).
2. Reloads `schedule.toml`, so edits apply without a restart.
3. Fires every due entry: `current_exe()` spawned as
   `wizard --mode sovereign -p "<prompt>" --cwd <cwd> [--max-hours H]`
   (`--continuous` for `mode = "continuous"`). Entries due at the same time
   all spawn concurrently — runs are never serialized.
4. Sleeps until the next fire, capped at 60 s.

Semantics worth knowing:

- **No backfill.** Each entry's clock starts when the daemon starts (or at
  its last fire within this daemon's lifetime). Occurrences that passed
  while the daemon was down are skipped, and several occurrences missed
  during one sleep collapse into a single fire.
- **Logging.** One timestamped line per fire / finish / timeout to
  `~/.wizard/logs/scheduler.log` (rotated to `.log.old` past ~5 MB) and to
  stdout. Each job's stdout/stderr is captured to
  `~/.wizard/logs/jobs/<name>-<timestamp>.log` (the "finished" line names
  the file), so a job that dies before writing any of its own state still
  leaves evidence. The newest 10 logs per entry are kept; older ones are
  pruned automatically.
- **Shutdown.** Ctrl-C (SIGINT) kills running jobs and exits 0.

## Running under systemd

The daemon stays in the foreground on purpose; let your init system own it.
A user unit (`~/.config/systemd/user/wizard-scheduler.service`):

```ini
[Unit]
Description=wizard scheduler

[Service]
ExecStart=%h/.local/bin/wizard scheduler
Restart=on-failure

[Install]
WantedBy=default.target
```

```bash
systemctl --user enable --now wizard-scheduler
journalctl --user -u wizard-scheduler -f   # same lines as scheduler.log
```
