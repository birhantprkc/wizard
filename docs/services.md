# Running Wizard as a background service

Some Wizard surfaces never finish on their own: the [messaging gateway](gateway.md) waits for chat messages, and the [scheduler](scheduler.md) waits for the clock. Both used to mean a terminal you could not close, a `tmux` session, or a unit file copied out of a doc and edited by hand.

They install themselves instead:

```bash
cd ~/your/project
wizard gateway install     # writes the unit, enables it, starts it, gives you the prompt back
```

The same seven subcommands exist on every supervised surface:

| Command | What it does |
|---|---|
| `install` | Write the service definition for this machine, enable it, start it, return to the prompt. Idempotent. |
| `start` / `stop` / `restart` | The obvious things, to an already-installed service. |
| `status` | Installed or not, running or not, since when, and the last thing that went wrong. Exits 0 when running, 1 otherwise. |
| `logs` | Tail the log. `-f` follows, `-n N` sets how much history. |
| `uninstall` | Stop it, disable it, remove the definition. Removing what is not installed is not an error. |

Surfaces with a service form today:

- `wizard gateway …` — the chat gateway (`wizard --gateway`)
- `wizard scheduler …` — the cron daemon (`wizard scheduler` with no subcommand still runs it in the foreground)

## What gets installed, per platform

### Linux: a systemd **user** unit

`~/.config/systemd/user/wizard-gateway.service` (or under `$XDG_CONFIG_HOME`). A user unit, not a system unit, so **no root is involved** — the same choice as `~/.local/bin` for the binary itself, and it means the agent runs as you rather than as root.

```ini
# Written by `wizard gateway install`. Edit at your own risk: a reinstall overwrites it.
[Unit]
Description=Wizard messaging gateway
Documentation=https://github.com/teddytennant/wizard/blob/main/docs/services.md
Wants=network-online.target
After=network-online.target

[Service]
Type=simple
WorkingDirectory=/home/you/your/project
ExecStart=/home/you/.local/bin/wizard --gateway
Restart=always
RestartSec=5
Environment="PATH=/home/you/.local/bin:/usr/bin:/bin"
Environment="LANG=en_US.UTF-8"
Environment="RUST_LOG=info"

[Install]
WantedBy=default.target
```

Three details are resolved at install time rather than guessed:

- **`ExecStart` is an absolute path**, from `current_exe()` with symlinks resolved. A service manager does not share your shell's `PATH`; a unit that says `wizard` works on the machine that wrote it and nowhere else. Because the path is *resolved*, a `~/.local/bin/wizard` that is a symlink puts the target in the unit, not the symlink.
- **`WorkingDirectory` is the directory you ran `install` from — for the gateway.** A gateway turn edits files in a project, so this is the one thing you have to get right, and it is why you `cd` first. Reinstall from elsewhere to move it. The scheduler is the deliberate exception: its unit pins your **home directory**, because every `schedule.toml` entry carries its own `cwd` and the daemon `chdir`s each child into it, so capturing wherever you happened to be standing would only pin a directory that could later be deleted out from under the service.
- **The environment is captured, by name.** See below. Which `Environment=` lines actually appear depends on what is set in your shell; the block above is a typical Linux desktop.

**Lingering.** A systemd *user* manager is torn down when your last session ends, so without lingering the service dies at logout and never starts at boot. `install` checks and tells you, in words:

```
note: lingering is off for you, so this service stops when you log out and
does not start at boot. Turn it on once:
  sudo loginctl enable-linger you
```

### macOS: a launchd LaunchAgent

`~/Library/LaunchAgents/com.teddytennant.wizard.gateway.plist`, loaded with `launchctl bootstrap gui/<uid>` and removed with `bootout`. `KeepAlive` is the launchd spelling of `Restart=always` and `ThrottleInterval` of `RestartSec`. LaunchAgents load at login, so there is no lingering to enable.

launchd has no journal, so the plist points `StandardOutPath` and `StandardErrorPath` at `~/.wizard/logs/wizard-gateway.log`, and `wizard gateway logs` tails that file.

### Termux, and Linux without systemd

Neither gets a file written into a void. `install` refuses and names the alternative.

On **Termux** there is no systemd at all; Termux ships runit under `termux-services`:

```bash
pkg install termux-services && . $PREFIX/etc/profile.d/start-services.sh
mkdir -p $PREFIX/var/service/wizard-gateway
printf '#!/data/data/com.termux/files/usr/bin/sh\nexec wizard --gateway 2>&1\n' \
  > $PREFIX/var/service/wizard-gateway/run
chmod +x $PREFIX/var/service/wizard-gateway/run
sv-enable wizard-gateway
```

Also hold a wakelock (`termux-wake-lock`), or Android suspends the process.

On **OpenRC / runit / s6 / a container with no init**, point your own supervisor at the command; the whole service is one long-running process in one directory.

The refusal is not limited to `install`. Supervisor detection runs first for *every* subcommand, so on such a host `wizard gateway status` and `wizard gateway logs` also exit non-zero with the same message naming what to use instead. That is honest rather than broken — Wizard never reports a state it has no supervisor to ask about — but it does mean these subcommands are not usable as a probe there.

Windows is not supported.

## Credentials: how the token reaches the service

This section is about the **gateway**. The scheduler has no credential to arrange: its daemon spawns `wizard` children that load their own config, so a scheduler service that starts is a scheduler service that works.

**No secret is ever written into a unit file.** `~/.config/systemd/user/*.service` is world-readable by default and `systemctl --user cat` prints it back, so a bot token in there is a bot token published to every local user of the machine. Wizard refuses to render a unit carrying an environment variable whose name looks like a secret at all: the check is a case-insensitive substring match for `token`, `secret`, `key`, `password`, `passwd`, `credential`, `auth`, or `cookie`, and it fails the render rather than dropping the variable quietly.

That check is on the variable's *name*, so it is a guard against the obvious mistake, not a scanner. A secret smuggled inside the *value* of a carried variable — a token pasted into `PATH`, say — would still be written.

Secrets reach the service the way they already reach a cron job: through **`~/.wizard/credentials.toml`, mode 0600**, which the service reads because it runs as you.

That is also why `wizard gateway install` does a little work up front. If your bot token is only in the installing shell's environment (`export WIZARD_TELEGRAM_TOKEN=…`), the service would never see it — a supervisor hands the process no environment — so install copies it, once, into `credentials.toml` and says so:

```
copied the bot token from $WIZARD_TELEGRAM_TOKEN into /home/you/.wizard/credentials.toml
(mode 0600) — a background service inherits no environment, and a token in a unit
file would be world-readable
```

If there is no token anywhere, install refuses rather than leaving you with a crash loop and a nice message in a log nobody opens.

### What *is* carried into the unit

Only these variables, by name, and only when they are set to a non-empty value with no control characters in it:

| Variable | Why |
|---|---|
| `PATH` | A gateway turn runs `git`, `cargo`, `rg`. systemd's user manager supplies a minimal `PATH` that usually lacks `~/.local/bin`, so the agent would find a different toolchain than you do. |
| `WIZARD_HOME` | Relocates all Wizard state. A service reading a different `~/.wizard` has no credentials and no config. |
| `WIZARD_LOG`, `RUST_LOG` | Diagnostics. `RUST_LOG=info` is set when you have not set it, so the journal is not empty when something goes wrong. |
| `LANG`, `TZ` | Text handling, and the local time the scheduler evaluates cron expressions in. |

`PATH` is a snapshot of the shell you installed from. If you install from inside a `nix develop` or a virtualenv, that is the `PATH` the service keeps — reinstall from a normal shell if that is not what you wanted.

`SSH_AUTH_SOCK` is deliberately **not** carried, even though it would let a gateway turn push to a git remote: its value names a socket belonging to one login session, so capturing it would bake in a path that is dead the next time you log in. If you want the agent to push, give it a deploy key with no passphrase, or add the variable to the unit yourself with `systemctl --user edit wizard-gateway` (a drop-in survives a reinstall; the unit does not).

## After replacing the binary

When the new binary is put in place by a rename over the old path — which is what `wizard update` will do once it can, and what `cargo install`-style flows do — a running service keeps executing the image it already opened, so **it survives the swap and keeps running the old version**. The next start picks up the new one:

```bash
wizard gateway restart
```

Two caveats on the rename. It only holds where the install directory is writable by you; in a root-owned prefix like `/usr/local/bin` the update path escalates to `sudo install`, which writes the target in place rather than replacing the inode, and on Linux that can fail outright while the old binary is executing. And a build that writes straight over the path (rather than to a temporary file and renaming) has the same problem — stop the service first if you are unsure.

The unit itself does not need rewriting: it points at the path, and the path is what got replaced. Reinstalling is only necessary when you want to change the working directory or the captured environment.

## Idempotence

The definition path is a function of the service name, so installing twice replaces one file rather than accumulating two. Install ends with a `restart`, not a `start`, so what is running is always what was just written — a reinstall never leaves an older copy of the process alive. Uninstalling something that is not installed prints "nothing to remove" and exits 0.

## Diagnosing

```bash
wizard gateway status
wizard gateway logs -f
```

`status` reads the definition file first, so "not installed" is answered without asking the supervisor at all. When it can ask, it reports systemd's own post-mortem (`Result`, the last exit status, the restart count), which survives a restart — a service that crashed four times and is currently up still tells you it crashed four times.

If the supervisor cannot be reached (no session bus in a container), the state is reported as `unknown` rather than guessed at as `stopped`.
