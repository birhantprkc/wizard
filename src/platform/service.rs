//! Installing Wizard's long-running surfaces as background services.
//!
//! Three things Wizard does never end on their own: the messaging gateway
//! (`wizard --gateway`), the cron daemon (`wizard scheduler`), and — once it
//! has a headless spelling — the mesh listener. Until now every one of them
//! was documented as "run it and keep the terminal open", with a unit file
//! pasted into `docs/` for the reader to copy, edit, and get subtly wrong: the
//! shipped `contrib/wizard-gateway.service` pointed at `%h/.local/bin/wizard`
//! whether or not the binary was there, defaulted `WorkingDirectory` to the
//! home directory rather than the project, and said nothing about lingering,
//! so the service a reader installed died at logout.
//!
//! This module owns that instead. A caller describes *what* to supervise with
//! a [`ServiceSpec`] and asks for an action ([`ServiceCmd`]); the platform
//! decides *how*, which on Linux means a systemd **user** unit under
//! `~/.config/systemd/user` and on macOS a launchd **LaunchAgent** under
//! `~/Library/LaunchAgents`. Both are per-user by construction: everything
//! else Wizard installs lands in `~/.local/bin` or `~/.wizard`, and a daemon
//! that needs root to install is a daemon that runs the agent as root.
//!
//! ## What the platform is not allowed to guess
//!
//! - **The binary.** `ExecStart` is [`std::env::current_exe`] resolved through
//!   its symlinks at install time, never the bare word `wizard`. A service
//!   manager does not share the shell's `PATH`, and "works when I type it,
//!   fails as a service" is the single most common way a hand-written unit is
//!   wrong.
//! - **The working directory.** A gateway turn runs *in a project*. The
//!   directory is captured at install time and written into the unit.
//! - **The environment.** A systemd user unit inherits almost nothing, so the
//!   parts of the caller's shell environment that a run genuinely needs are
//!   captured too — but only the ones on [`CARRIED_ENV`], by name. Everything
//!   else is dropped, and [`ServiceSpec::validate`] refuses outright to carry
//!   a variable whose *name* looks like a secret ([`looks_secret`]). Unit
//!   files are world-readable by default; secrets reach the service the way
//!   they already reach a cron job, through `~/.wizard/credentials.toml` at
//!   0600, which the service reads because it runs as the same user.
//!
//! ## Refusing beats writing a file into a void
//!
//! [`Installer::detect`] is the only constructor that consults the host, and
//! nothing here can write a unit without an [`Installer`]. On Termux (no
//! systemd, no launchd — it has runit via `termux-services`) and on a
//! non-systemd Linux (OpenRC, s6, a container with no init) detection fails
//! with a message naming what to do instead, before any path is computed.
//! That ordering is a type-level property rather than a convention: see
//! `an_unsupported_host_cannot_even_name_a_unit_path` in the tests below.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

/// Seconds a supervisor waits before restarting a crashed service.
const RESTART_SECS: u32 = 5;

/// Environment variables carried from the installing shell into the unit, by
/// name. Nothing else is carried, and a name that [`looks_secret`] may not be
/// added here (the test below enforces it).
///
/// - `PATH`: a gateway turn runs `git`, `cargo`, `rg`. systemd's user manager
///   supplies a minimal `PATH` that usually lacks `~/.local/bin` and anything
///   a version manager put in front, so the agent's shell tool would find a
///   different toolchain than the operator's.
/// - `WIZARD_HOME`: relocates all of Wizard's state. A service that reads a
///   different `~/.wizard` than the operator has no credentials and no config.
/// - `WIZARD_LOG` / `RUST_LOG`: diagnostics, so a verbose shell stays verbose
///   as a service.
/// - `LANG`: byte-vs-UTF-8 behaviour of the tools a turn shells out to.
/// - `TZ`: the scheduler evaluates cron expressions in local time.
///
/// `SSH_AUTH_SOCK` is deliberately *not* here even though it would let a
/// gateway turn push to a remote: its value names a socket belonging to one
/// login session, so capturing it bakes in a path that is dead the next time
/// the operator logs in — a service that worked yesterday and silently cannot
/// push today. `docs/services.md` says how to do it deliberately.
pub const CARRIED_ENV: &[&str] = &[
    "PATH",
    "WIZARD_HOME",
    "WIZARD_LOG",
    "RUST_LOG",
    "LANG",
    "TZ",
];

/// Substrings that make a variable name look like it holds a secret. Matched
/// case-insensitively by [`looks_secret`].
const SECRET_MARKERS: &[&str] = &[
    "token",
    "secret",
    "key",
    "password",
    "passwd",
    "credential",
    "auth",
    "cookie",
];

/// Whether `name` looks like it names a secret, and so may never be written
/// into a unit file.
///
/// A guard against the *next* caller, not against [`CARRIED_ENV`] as it
/// stands: the obvious way to make a gateway service find its bot token is to
/// pass `WIZARD_TELEGRAM_TOKEN` through, and the obvious way is wrong, because
/// `~/.config/systemd/user/*.service` is 0644 and `systemctl --user cat` will
/// print it back to anyone who asks.
pub fn looks_secret(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    SECRET_MARKERS.iter().any(|marker| lower.contains(marker))
}

// ---------------------------------------------------------------------------
// What to supervise
// ---------------------------------------------------------------------------

/// One supervised Wizard surface, resolved for *this* machine at install time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceSpec {
    /// Service name: lowercase, `[a-z0-9-]`, e.g. `wizard-gateway`. Becomes
    /// `wizard-gateway.service` under systemd and
    /// `com.teddytennant.wizard.gateway` under launchd.
    pub name: String,
    /// One-line human description for `[Unit] Description=`.
    pub description: String,
    /// Documentation URL for `[Unit] Documentation=`.
    pub documentation: String,
    /// The CLI prefix that manages this service (`wizard gateway`), used only
    /// for the hints printed after an action.
    pub cli: String,
    /// Absolute, symlink-resolved path of the binary to run.
    pub exe: PathBuf,
    /// Arguments after the binary (`["--gateway"]`).
    pub args: Vec<String>,
    /// Absolute working directory, captured at install time.
    pub working_dir: PathBuf,
    /// Environment carried into the service. See [`CARRIED_ENV`].
    pub environment: Vec<(String, String)>,
    /// Seconds to wait before restarting after a failure.
    pub restart_secs: u32,
}

impl ServiceSpec {
    /// Describe a surface of *this* binary: the running executable, the given
    /// arguments, `working_dir` (defaulting to the current directory) and the
    /// carried environment.
    pub fn for_surface(
        name: &str,
        description: &str,
        documentation: &str,
        cli: &str,
        args: &[&str],
        working_dir: Option<PathBuf>,
    ) -> Result<Self> {
        let exe = std::env::current_exe().context("locating the running wizard binary")?;
        // Resolved, because the unit outlives this process: `~/.local/bin/wizard`
        // may be a symlink today and a different symlink tomorrow, and
        // `wizard update` renames over the *real* path.
        let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
        let working_dir = match working_dir {
            Some(dir) => dir,
            None => std::env::current_dir().context("determining the working directory")?,
        };
        let working_dir = std::fs::canonicalize(&working_dir).unwrap_or(working_dir);
        let spec = Self {
            name: name.to_string(),
            description: description.to_string(),
            documentation: documentation.to_string(),
            cli: cli.to_string(),
            exe,
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
            working_dir,
            environment: inherited_env(),
            restart_secs: RESTART_SECS,
        };
        spec.validate()?;
        Ok(spec)
    }

    /// Everything that must be true before a unit is rendered from this spec.
    ///
    /// The secret check is the load-bearing one and is why this is a separate
    /// function rather than a few asserts inside [`for_surface`]: a caller
    /// that builds a spec by hand (a test, a future surface) goes through the
    /// same gate.
    pub fn validate(&self) -> Result<()> {
        if self.name.is_empty()
            || !self
                .name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            bail!(
                "service name {:?} must be lowercase letters, digits and dashes",
                self.name
            );
        }
        if !self.exe.is_absolute() {
            bail!(
                "service binary {} must be an absolute path",
                self.exe.display()
            );
        }
        if !self.working_dir.is_absolute() {
            bail!(
                "service working directory {} must be an absolute path",
                self.working_dir.display()
            );
        }
        for (name, value) in &self.environment {
            if looks_secret(name) {
                bail!(
                    "refusing to write {name} into a service definition: unit files are \
                     world-readable, and a name like this holds a secret. Store it in \
                     ~/.wizard/credentials.toml (mode 0600) instead — the service runs as \
                     you and reads the same file"
                );
            }
            if name.is_empty() || name.contains('=') || has_control(name) || has_control(value) {
                bail!("environment variable {name:?} cannot be written into a service definition");
            }
        }
        for arg in &self.args {
            if has_control(arg) {
                bail!("service argument {arg:?} cannot be written into a service definition");
            }
        }
        Ok(())
    }

    /// The full command line, for messages that tell an operator what the
    /// service actually runs.
    pub fn command_line(&self) -> String {
        let mut line = self.exe.display().to_string();
        for arg in &self.args {
            line.push(' ');
            line.push_str(arg);
        }
        line
    }
}

/// True when `text` holds a newline, carriage return or NUL — none of which
/// survive a unit file or a plist intact.
fn has_control(text: &str) -> bool {
    text.contains(['\n', '\r', '\0'])
}

/// The subset of this process's environment that a service is given. See
/// [`CARRIED_ENV`] for why each name is on the list.
///
/// `RUST_LOG` gets a default of `info` when unset, matching what the
/// hand-written unit in `docs/gateway.md` always carried: a service with no
/// log level at all is a service whose journal is empty when it misbehaves.
pub fn inherited_env() -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = CARRIED_ENV
        .iter()
        .filter(|name| !looks_secret(name))
        .filter_map(|name| {
            std::env::var(name)
                .ok()
                .filter(|value| !value.is_empty() && !has_control(value))
                .map(|value| ((*name).to_string(), value))
        })
        .collect();
    if !env.iter().any(|(name, _)| name == "RUST_LOG") {
        env.push(("RUST_LOG".to_string(), "info".to_string()));
    }
    env
}

// ---------------------------------------------------------------------------
// Which supervisor this host has
// ---------------------------------------------------------------------------

/// The service manager Wizard knows how to drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Manager {
    /// systemd, as a **user** manager (`systemctl --user`). No root needed.
    Systemd,
    /// launchd, as a per-user LaunchAgent (`launchctl bootstrap gui/<uid>`).
    Launchd,
}

impl Manager {
    /// Human name, for messages.
    pub fn label(self) -> &'static str {
        match self {
            Manager::Systemd => "systemd (user)",
            Manager::Launchd => "launchd (LaunchAgent)",
        }
    }
}

/// What [`Installer::detect`] looked at. Separated from the decision so the
/// decision can be tested for hosts this machine is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Probe {
    /// `std::env::consts::OS`.
    pub os: &'static str,
    /// Running under Termux on Android.
    pub termux: bool,
    /// systemd is pid 1 here (`/run/systemd/system` exists).
    pub systemd: bool,
}

impl Probe {
    /// Look at the host this process is running on.
    pub fn current() -> Self {
        Self {
            os: std::env::consts::OS,
            termux: super::host::is_termux(),
            systemd: Path::new("/run/systemd/system").is_dir(),
        }
    }
}

/// Choose a manager, or explain what to do on a host that has neither.
///
/// Termux is checked before the OS: it reports `linux`, and a `/run/systemd`
/// probe there is simply absent, so without this arm an Android user would get
/// the generic non-systemd message instead of the one naming the tool they
/// actually have.
fn manager_for(probe: &Probe) -> Result<Manager> {
    if probe.termux {
        bail!(
            "Termux has no systemd, so there is no user unit to install.\n\
             Use termux-services (runit), which Termux ships for exactly this:\n\
             \x20 pkg install termux-services && . $PREFIX/etc/profile.d/start-services.sh\n\
             \x20 mkdir -p $PREFIX/var/service/wizard-gateway\n\
             \x20 printf '#!/data/data/com.termux/files/usr/bin/sh\\nexec wizard --gateway 2>&1\\n' \\\n\
             \x20   > $PREFIX/var/service/wizard-gateway/run\n\
             \x20 chmod +x $PREFIX/var/service/wizard-gateway/run\n\
             \x20 sv-enable wizard-gateway\n\
             Also acquire a wakelock (`termux-wake-lock`), or Android stops the process. \
             See docs/services.md."
        );
    }
    match probe.os {
        "macos" => Ok(Manager::Launchd),
        "linux" if probe.systemd => Ok(Manager::Systemd),
        "linux" => bail!(
            "no systemd on this host (/run/systemd/system is absent), and Wizard only \
             installs systemd user units or macOS LaunchAgents.\n\
             Point your own supervisor at the binary instead — with OpenRC, runit, s6 or \
             supervisord, the whole service is one long-running command in the project \
             directory. Every `service` subcommand answers with this same message on \
             such a host, `status` included: detection runs before the action, so there \
             is no supervisor to ask whether anything is installed. See docs/services.md."
        ),
        other => bail!(
            "installing a background service is not supported on {other}; \
             run the surface under your own supervisor. See docs/services.md."
        ),
    }
}

// ---------------------------------------------------------------------------
// The installer
// ---------------------------------------------------------------------------

/// What [`Installer::control`] does to an already-installed service.
///
/// An enum rather than the systemd verb as a string, because launchd has no
/// `restart` at all and spells the other two `bootstrap` / `bootout`: a
/// stringly-typed action silently becomes a `kickstart` on the arm that
/// matched nothing. It also carries the past tense, so the line an operator
/// reads is not "stoped".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Start,
    Stop,
    Restart,
}

impl Action {
    /// The systemd verb, and the word used in "nothing to …".
    fn verb(self) -> &'static str {
        match self {
            Action::Start => "start",
            Action::Stop => "stop",
            Action::Restart => "restart",
        }
    }

    /// Past tense, for the confirmation line.
    fn done(self) -> &'static str {
        match self {
            Action::Start => "started",
            Action::Stop => "stopped",
            Action::Restart => "restarted",
        }
    }
}

/// A supervisor plus the directory its per-user definitions live in.
///
/// The only way to obtain one for the real machine is [`Installer::detect`],
/// which fails on an unsupported host. Every function that writes, removes or
/// inspects a unit hangs off this type, so "refuse before writing anything" is
/// enforced by construction rather than by remembering to check.
#[derive(Debug, Clone)]
pub struct Installer {
    manager: Manager,
    root: PathBuf,
}

impl Installer {
    /// Detect this host's supervisor and where its user definitions live.
    pub fn detect() -> Result<Self> {
        let manager = manager_for(&Probe::current())?;
        let root = default_root(manager)?;
        Ok(Self::at(manager, root))
    }

    /// An installer for an explicit manager and definition directory. Used by
    /// [`Installer::detect`] and by tests, which install into a temporary
    /// directory rather than the operator's real home.
    pub fn at(manager: Manager, root: impl Into<PathBuf>) -> Self {
        Self {
            manager,
            root: root.into(),
        }
    }

    /// Which supervisor this installer drives.
    pub fn manager(&self) -> Manager {
        self.manager
    }

    /// The supervisor's name for the service: `wizard-gateway.service` or
    /// `com.teddytennant.wizard.gateway`.
    pub fn unit_name(&self, name: &str) -> String {
        match self.manager {
            Manager::Systemd => format!("{name}.service"),
            Manager::Launchd => format!("com.teddytennant.{}", name.replace('-', ".")),
        }
    }

    /// Absolute path of the definition file. Deterministic in `name`, which is
    /// what makes a second install overwrite rather than accumulate.
    pub fn unit_path(&self, name: &str) -> PathBuf {
        match self.manager {
            Manager::Systemd => self.root.join(self.unit_name(name)),
            Manager::Launchd => self.root.join(format!("{}.plist", self.unit_name(name))),
        }
    }

    /// Whether a definition for `name` exists.
    pub fn is_installed(&self, name: &str) -> bool {
        self.unit_path(name).is_file()
    }

    /// Render the definition for `spec` without writing it.
    pub fn render(&self, spec: &ServiceSpec) -> Result<String> {
        spec.validate()?;
        Ok(match self.manager {
            Manager::Systemd => systemd_unit(spec),
            Manager::Launchd => launchd_plist(spec, &self.unit_name(&spec.name)),
        })
    }

    /// Write the definition, creating the directory. Returns the path and
    /// whether the bytes changed — a second install of an unchanged spec
    /// rewrites nothing and, since the path is a function of the name, can
    /// never produce a second unit.
    pub fn write_unit(&self, spec: &ServiceSpec) -> Result<(PathBuf, bool)> {
        let rendered = self.render(spec)?;
        let path = self.unit_path(&spec.name);
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("creating {}", self.root.display()))?;
        if std::fs::read_to_string(&path).is_ok_and(|existing| existing == rendered) {
            return Ok((path, false));
        }
        std::fs::write(&path, rendered).with_context(|| format!("writing {}", path.display()))?;
        Ok((path, true))
    }

    /// Delete the definition. `false` means there was nothing to delete, which
    /// is not an error: uninstalling twice must be as quiet as uninstalling
    /// something that was never installed.
    pub fn remove_unit(&self, name: &str) -> Result<bool> {
        let path = self.unit_path(name);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => {
                Err(anyhow::Error::new(err).context(format!("removing {}", path.display())))
            }
        }
    }
}

/// Where the supervisor reads per-user definitions from.
fn default_root(manager: Manager) -> Result<PathBuf> {
    let home = dirs::home_dir().context("could not determine the home directory")?;
    Ok(match manager {
        // systemd honours XDG_CONFIG_HOME for user units, so honour it too:
        // writing to ~/.config on a host that reads elsewhere installs nothing.
        Manager::Systemd => match std::env::var_os("XDG_CONFIG_HOME") {
            Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("systemd/user"),
            _ => home.join(".config/systemd/user"),
        },
        Manager::Launchd => home.join("Library/LaunchAgents"),
    })
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// A systemd user unit.
///
/// `%` is doubled everywhere a caller's string is interpolated: systemd reads
/// `%h`, `%i`, `%n` and friends as specifiers, so a project directory with a
/// literal percent in its name would otherwise expand to something else
/// entirely (or refuse to load).
fn systemd_unit(spec: &ServiceSpec) -> String {
    let mut unit = String::new();
    unit.push_str("# Written by `");
    unit.push_str(&spec.cli);
    unit.push_str(" install`. Edit at your own risk: a reinstall overwrites it.\n");
    unit.push_str("[Unit]\n");
    unit.push_str(&format!(
        "Description={}\n",
        escape_systemd(&spec.description)
    ));
    unit.push_str(&format!(
        "Documentation={}\n",
        escape_systemd(&spec.documentation)
    ));
    // Both, deliberately: `Wants=` pulls the target in, `After=` orders
    // against it. A gateway that starts before the network is up spends its
    // first restart interval failing to reach the platform.
    unit.push_str("Wants=network-online.target\n");
    unit.push_str("After=network-online.target\n\n");

    unit.push_str("[Service]\n");
    unit.push_str("Type=simple\n");
    unit.push_str(&format!(
        "WorkingDirectory={}\n",
        escape_systemd(&spec.working_dir.display().to_string())
    ));
    unit.push_str(&format!("ExecStart={}\n", systemd_exec_start(spec)));
    // `always`, not `on-failure`: the gateway exits 0 on SIGINT, and a
    // supervised long-running surface that stops cleanly for any reason other
    // than an operator asking should come back. `systemctl --user stop` still
    // wins — it is not a failure, it is a stop.
    unit.push_str("Restart=always\n");
    unit.push_str(&format!("RestartSec={}\n", spec.restart_secs));
    for (name, value) in &spec.environment {
        unit.push_str(&format!(
            "Environment=\"{}={}\"\n",
            escape_systemd(name),
            escape_systemd_quoted(value)
        ));
    }
    unit.push('\n');

    unit.push_str("[Install]\n");
    unit.push_str("WantedBy=default.target\n");
    unit
}

/// `ExecStart=` with the binary quoted when it needs it. systemd splits the
/// line on whitespace, so an unquoted path with a space in it becomes two
/// arguments and the unit fails to start with a file-not-found.
fn systemd_exec_start(spec: &ServiceSpec) -> String {
    let mut line = systemd_exec_word(&spec.exe.display().to_string());
    for arg in &spec.args {
        line.push(' ');
        line.push_str(&systemd_exec_word(arg));
    }
    line
}

fn systemd_exec_word(word: &str) -> String {
    if word.contains([' ', '\t', '"', '\'', '\\']) {
        format!("\"{}\"", escape_systemd_quoted(word))
    } else {
        escape_systemd(word)
    }
}

/// Escape a value for a unit file: only `%` needs it outside quotes.
fn escape_systemd(value: &str) -> String {
    value.replace('%', "%%")
}

/// Escape a value that will sit inside double quotes in a unit file.
fn escape_systemd_quoted(value: &str) -> String {
    escape_systemd(&value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// A launchd LaunchAgent property list.
fn launchd_plist(spec: &ServiceSpec, label: &str) -> String {
    let mut plist = String::new();
    plist.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    plist.push_str(
        "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
    );
    plist.push_str("<plist version=\"1.0\">\n<dict>\n");
    plist.push_str(&format!(
        "  <!-- Written by `{} install`. A reinstall overwrites it. -->\n",
        xml_escape(&spec.cli)
    ));
    plist.push_str(&format!(
        "  <key>Label</key>\n  <string>{}</string>\n",
        xml_escape(label)
    ));
    plist.push_str("  <key>ProgramArguments</key>\n  <array>\n");
    plist.push_str(&format!(
        "    <string>{}</string>\n",
        xml_escape(&spec.exe.display().to_string())
    ));
    for arg in &spec.args {
        plist.push_str(&format!("    <string>{}</string>\n", xml_escape(arg)));
    }
    plist.push_str("  </array>\n");
    plist.push_str(&format!(
        "  <key>WorkingDirectory</key>\n  <string>{}</string>\n",
        xml_escape(&spec.working_dir.display().to_string())
    ));
    plist.push_str("  <key>RunAtLoad</key>\n  <true/>\n");
    // launchd's throttle is per-label and global; `ThrottleInterval` is the
    // launchd spelling of RestartSec, and KeepAlive without qualification is
    // "restart it whenever it is not running", which is what Restart=always is.
    plist.push_str("  <key>KeepAlive</key>\n  <true/>\n");
    plist.push_str(&format!(
        "  <key>ThrottleInterval</key>\n  <integer>{}</integer>\n",
        spec.restart_secs
    ));
    plist.push_str("  <key>ProcessType</key>\n  <string>Background</string>\n");
    if !spec.environment.is_empty() {
        plist.push_str("  <key>EnvironmentVariables</key>\n  <dict>\n");
        for (name, value) in &spec.environment {
            plist.push_str(&format!(
                "    <key>{}</key>\n    <string>{}</string>\n",
                xml_escape(name),
                xml_escape(value)
            ));
        }
        plist.push_str("  </dict>\n");
    }
    // launchd has no journal, so the agent's own output has to go somewhere a
    // `logs` subcommand can tail.
    let log = log_file_display(&spec.name);
    plist.push_str(&format!(
        "  <key>StandardOutPath</key>\n  <string>{}</string>\n",
        xml_escape(&log)
    ));
    plist.push_str(&format!(
        "  <key>StandardErrorPath</key>\n  <string>{}</string>\n",
        xml_escape(&log)
    ));
    plist.push_str("</dict>\n</plist>\n");
    plist
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// `~/.wizard/logs/<name>.log`: where a launchd agent's stdout and stderr go,
/// and what `logs` tails on macOS. Falls back to a path under the home
/// directory when the state directory cannot be resolved at all, because a
/// plist has to name *something*.
fn log_file(name: &str) -> PathBuf {
    match super::paths::logs_dir() {
        Ok(dir) => dir.join(format!("{name}.log")),
        Err(_) => dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(format!(".wizard/logs/{name}.log")),
    }
}

fn log_file_display(name: &str) -> String {
    log_file(name).display().to_string()
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

/// What a service is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// No definition on disk.
    NotInstalled,
    /// Installed, not running.
    Stopped,
    /// Installed and running.
    Running,
    /// Installed, and the supervisor gave up or the process died badly.
    Failed,
    /// Installed, but the supervisor could not be asked (no session bus, no
    /// `systemctl` on `PATH`). Reported as such rather than guessed at.
    Unknown,
}

/// A service's state, as the supervisor reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub name: String,
    pub unit: PathBuf,
    pub state: State,
    /// Whether the supervisor will start it at login, when it can say.
    pub enabled: Option<bool>,
    /// When it entered its current state, in the supervisor's own words.
    pub since: Option<String>,
    /// The last thing that went wrong, when there is one.
    pub detail: Option<String>,
}

impl Status {
    /// A service that is not installed, which is the answer whenever there is
    /// no definition on disk — no supervisor is consulted for that.
    fn absent(name: &str, unit: PathBuf) -> Self {
        Self {
            name: name.to_string(),
            unit,
            state: State::NotInstalled,
            enabled: None,
            since: None,
            detail: None,
        }
    }

    /// Multi-line human summary.
    pub fn summary(&self) -> String {
        let mut out = match self.state {
            State::NotInstalled => format!("{}: not installed", self.name),
            State::Stopped => format!("{}: installed, stopped", self.name),
            State::Running => format!("{}: running", self.name),
            State::Failed => format!("{}: failed", self.name),
            State::Unknown => format!("{}: installed, state unknown", self.name),
        };
        if self.state != State::NotInstalled {
            out.push_str(&format!("\n  unit:    {}", self.unit.display()));
        }
        if let Some(enabled) = self.enabled {
            out.push_str(&format!(
                "\n  at boot: {}",
                if enabled { "enabled" } else { "disabled" }
            ));
        }
        if let Some(since) = &self.since {
            out.push_str(&format!("\n  since:   {since}"));
        }
        if let Some(detail) = &self.detail {
            out.push_str(&format!("\n  last:    {detail}"));
        }
        out
    }
}

/// Parse `systemctl --user show` output (`Key=Value` per line) into a status.
///
/// Pure, so the mapping from systemd's vocabulary to [`State`] is testable on
/// a host with no systemd at all — which is most CI containers.
fn status_from_systemd_show(name: &str, unit: PathBuf, text: &str) -> Status {
    let field = |key: &str| -> Option<&str> {
        text.lines()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };
    let state = match field("ActiveState") {
        Some("active") | Some("activating") | Some("reloading") => State::Running,
        Some("failed") => State::Failed,
        Some("inactive") | Some("deactivating") => State::Stopped,
        _ => State::Unknown,
    };
    let since = field("ActiveEnterTimestamp").map(str::to_string);
    // `Result` is systemd's post-mortem: `success` until something goes wrong,
    // then `exit-code`, `signal`, `timeout`, `start-limit-hit`. It survives a
    // restart, so it is the "last error" an operator wants after the service
    // has flapped back into `active`.
    let detail = match field("Result") {
        Some("success") | None => None,
        Some(result) => {
            let mut detail = result.to_string();
            if let Some(code) = field("ExecMainStatus").filter(|code| *code != "0") {
                detail.push_str(&format!(" (exit status {code})"));
            }
            if let Some(restarts) = field("NRestarts").filter(|n| *n != "0") {
                detail.push_str(&format!(", {restarts} restarts"));
            }
            Some(detail)
        }
    };
    Status {
        name: name.to_string(),
        unit,
        state,
        enabled: field("UnitFileState").map(|value| value == "enabled"),
        since,
        detail,
    }
}

/// Parse `launchctl list <label>` output. The dictionary launchd prints holds
/// `"PID" = <n>;` while the job is running and `"LastExitStatus" = <n>;`
/// after it has stopped.
fn status_from_launchctl(name: &str, unit: PathBuf, text: &str, listed: bool) -> Status {
    if !listed {
        // The plist is on disk but launchd does not know the label: installed
        // and not loaded, which is exactly "stopped".
        return Status {
            name: name.to_string(),
            unit,
            state: State::Stopped,
            enabled: Some(false),
            since: None,
            detail: None,
        };
    }
    let number = |key: &str| -> Option<i64> {
        text.lines()
            .find(|line| line.trim_start().starts_with(&format!("\"{key}\"")))
            .and_then(|line| line.split('=').nth(1))
            .map(|value| value.trim().trim_end_matches(';').trim())
            .and_then(|value| value.parse::<i64>().ok())
    };
    let pid = number("PID");
    let last_exit = number("LastExitStatus").filter(|code| *code != 0);
    Status {
        name: name.to_string(),
        unit,
        state: if pid.is_some() {
            State::Running
        } else if last_exit.is_some() {
            State::Failed
        } else {
            State::Stopped
        },
        enabled: Some(true),
        since: pid.map(|pid| format!("pid {pid}")),
        detail: last_exit.map(|code| format!("exit status {code}")),
    }
}

// ---------------------------------------------------------------------------
// Talking to the supervisor
// ---------------------------------------------------------------------------

/// Run a supervisor command and capture its output. A missing binary is an
/// error naming it, not a panic: `systemctl` is absent from plenty of
/// containers, and this is how that surfaces.
fn capture(program: &str, args: &[&str]) -> Result<std::process::Output> {
    Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running {program} {}", args.join(" ")))
}

/// Run a supervisor command, failing with its stderr when it exits non-zero.
fn run_checked(program: &str, args: &[&str]) -> Result<()> {
    let output = capture(program, args)?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let message = [stderr.trim(), stdout.trim()]
        .iter()
        .find(|text| !text.is_empty())
        .map(|text| (*text).to_string())
        .unwrap_or_else(|| format!("exit status {}", output.status));
    bail!("{program} {} failed: {message}", args.join(" "))
}

/// This user's numeric id, which launchd needs to name the per-user domain
/// (`gui/501`).
fn current_uid() -> u32 {
    #[cfg(unix)]
    {
        // SAFETY: getuid is always safe; it takes no arguments, reads no
        // memory, and cannot fail.
        unsafe { libc::getuid() }
    }
    #[cfg(not(unix))]
    {
        0
    }
}

impl Installer {
    /// Install and start `spec`. Idempotent: the definition path is a function
    /// of the name, `enable` is a no-op the second time, and the final
    /// `restart` guarantees that what is running is what was just written
    /// rather than a copy started from an older unit.
    pub fn install(&self, spec: &ServiceSpec) -> Result<i32> {
        let (path, changed) = self.write_unit(spec)?;
        let unit = self.unit_name(&spec.name);
        println!(
            "{} {}",
            if changed { "wrote" } else { "unchanged" },
            path.display()
        );
        match self.manager {
            Manager::Systemd => {
                run_checked("systemctl", &["--user", "daemon-reload"])?;
                run_checked("systemctl", &["--user", "enable", &unit])?;
                // `restart` rather than `start`: starting an already-running
                // service is a no-op, which would leave the old unit's process
                // in place after a reinstall.
                run_checked("systemctl", &["--user", "restart", &unit])?;
            }
            Manager::Launchd => {
                let domain = format!("gui/{}", current_uid());
                let target = format!("{domain}/{unit}");
                // Bootout first so a reinstall replaces the loaded job rather
                // than colliding with it; failure means it was not loaded.
                let _ = capture("launchctl", &["bootout", &target]);
                run_checked(
                    "launchctl",
                    &["bootstrap", &domain, &path.display().to_string()],
                )?;
                let _ = capture("launchctl", &["enable", &target]);
                run_checked("launchctl", &["kickstart", "-k", &target])?;
            }
        }
        println!("{} is running under {}", unit, self.manager.label());
        println!("  runs: {}", spec.command_line());
        println!("  in:   {}", spec.working_dir.display());
        println!();
        println!("  {} status     — is it up?", spec.cli);
        println!("  {} logs -f    — what is it doing?", spec.cli);
        println!("  {} restart    — pick up a new binary or config", spec.cli);
        println!("  {} uninstall  — stop it and remove the unit", spec.cli);
        if let Some(advice) = self.linger_advice() {
            println!();
            println!("{advice}");
        }
        Ok(0)
    }

    /// Stop, disable and delete. Removing what is not there is not an error.
    pub fn uninstall(&self, name: &str) -> Result<i32> {
        let unit = self.unit_name(name);
        if !self.is_installed(name) {
            println!("nothing to remove — {name} is not installed as a service.");
            return Ok(0);
        }
        match self.manager {
            Manager::Systemd => {
                // Best effort: a unit that is already stopped, already
                // disabled, or that systemd never loaded must not turn
                // `uninstall` into a failure that leaves the file behind.
                let _ = capture("systemctl", &["--user", "disable", "--now", &unit]);
            }
            Manager::Launchd => {
                let target = format!("gui/{}/{unit}", current_uid());
                let _ = capture("launchctl", &["bootout", &target]);
            }
        }
        let removed = self.remove_unit(name)?;
        if removed {
            println!("removed {}", self.unit_path(name).display());
        }
        if self.manager == Manager::Systemd {
            let _ = capture("systemctl", &["--user", "daemon-reload"]);
        }
        Ok(0)
    }

    /// Start, stop or restart an installed service.
    pub fn control(&self, action: Action, name: &str) -> Result<i32> {
        let unit = self.unit_name(name);
        if !self.is_installed(name) {
            bail!(
                "{name} is not installed as a service — nothing to {}. \
                 Install it first, or run it in the foreground.",
                action.verb()
            );
        }
        match self.manager {
            Manager::Systemd => run_checked("systemctl", &["--user", action.verb(), &unit])?,
            Manager::Launchd => {
                let domain = format!("gui/{}", current_uid());
                let target = format!("{domain}/{unit}");
                match action {
                    Action::Stop => run_checked("launchctl", &["bootout", &target])?,
                    Action::Start => run_checked(
                        "launchctl",
                        &[
                            "bootstrap",
                            &domain,
                            &self.unit_path(name).display().to_string(),
                        ],
                    )?,
                    // launchd has no restart verb: kickstart -k kills the
                    // running copy and starts it again in one call.
                    Action::Restart => run_checked("launchctl", &["kickstart", "-k", &target])?,
                }
            }
        }
        println!("{unit} {}", action.done());
        Ok(0)
    }

    /// Ask the supervisor what the service is doing.
    pub fn status(&self, name: &str) -> Result<Status> {
        let unit_path = self.unit_path(name);
        if !unit_path.is_file() {
            return Ok(Status::absent(name, unit_path));
        }
        let unit = self.unit_name(name);
        Ok(match self.manager {
            Manager::Systemd => {
                let properties = "ActiveState,SubState,UnitFileState,Result,ExecMainStatus,\
                                  NRestarts,ActiveEnterTimestamp";
                match capture(
                    "systemctl",
                    &["--user", "show", &unit, "--property", properties],
                ) {
                    Ok(output) => status_from_systemd_show(
                        name,
                        unit_path,
                        &String::from_utf8_lossy(&output.stdout),
                    ),
                    Err(err) => Status {
                        name: name.to_string(),
                        unit: unit_path,
                        state: State::Unknown,
                        enabled: None,
                        since: None,
                        detail: Some(format!("could not ask systemd: {err}")),
                    },
                }
            }
            Manager::Launchd => match capture("launchctl", &["list", &unit]) {
                Ok(output) => status_from_launchctl(
                    name,
                    unit_path,
                    &String::from_utf8_lossy(&output.stdout),
                    output.status.success(),
                ),
                Err(err) => Status {
                    name: name.to_string(),
                    unit: unit_path,
                    state: State::Unknown,
                    enabled: None,
                    since: None,
                    detail: Some(format!("could not ask launchd: {err}")),
                },
            },
        })
    }

    /// Tail the service's log: the journal under systemd, the agent's own log
    /// file under launchd. Replaces this process's stdio with the tool's, so
    /// `-f` behaves exactly as `journalctl -f` does.
    pub fn logs(&self, name: &str, follow: bool, lines: u32) -> Result<i32> {
        let unit = self.unit_name(name);
        let lines = lines.to_string();
        let mut command = match self.manager {
            Manager::Systemd => {
                let mut command = Command::new("journalctl");
                command.args(["--user", "-u", &unit, "-n", &lines]);
                if follow {
                    command.arg("-f");
                }
                command
            }
            Manager::Launchd => {
                let log = log_file(name);
                if !log.exists() {
                    println!(
                        "no log yet at {} — the service has not started, or has written nothing.",
                        log.display()
                    );
                    return Ok(0);
                }
                let mut command = Command::new("tail");
                command.args(["-n", &lines]);
                if follow {
                    command.arg("-f");
                }
                command.arg(&log);
                command
            }
        };
        let status = command
            .status()
            .with_context(|| format!("running the log reader for {unit}"))?;
        Ok(status.code().unwrap_or(0))
    }

    /// What to tell the operator about lingering, or `None` when there is
    /// nothing to say.
    ///
    /// A systemd **user** manager is torn down when the last session of that
    /// user ends, so without `enable-linger` the service dies at logout and
    /// never starts at boot. That is precisely the failure this whole module
    /// exists to avoid, so it is checked and stated in words rather than left
    /// in a doc.
    pub fn linger_advice(&self) -> Option<String> {
        if self.manager != Manager::Systemd {
            // launchd agents load at login and are not affected.
            return None;
        }
        let user = std::env::var("USER").unwrap_or_else(|_| "$USER".to_string());
        let observed = capture("loginctl", &["show-user", &user, "--property=Linger"])
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned());
        linger_advice_from(observed.as_deref(), &user)
    }
}

/// The linger message, given `loginctl`'s output (or `None` when it could not
/// be run). Pure so the words an operator reads are under test.
fn linger_advice_from(output: Option<&str>, user: &str) -> Option<String> {
    let lingering = output.map(|text| text.lines().any(|line| line.trim() == "Linger=yes"));
    match lingering {
        Some(true) => None,
        Some(false) => Some(format!(
            "note: lingering is off for {user}, so this service stops when you log out and \
             does not start at boot. Turn it on once:\n\
             \x20 sudo loginctl enable-linger {user}"
        )),
        None => Some(format!(
            "note: could not check whether lingering is on for {user} (no loginctl?). \
             Without it a user service stops at logout. If in doubt, run:\n\
             \x20 sudo loginctl enable-linger {user}"
        )),
    }
}

// ---------------------------------------------------------------------------
// The CLI surface
// ---------------------------------------------------------------------------

/// Service management subcommands, shared by every surface that has a
/// long-running form. Defined here rather than in `cli.rs` so a new surface
/// gets the identical vocabulary by naming this type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::Subcommand)]
pub enum ServiceCmd {
    /// Write the service definition for this machine, enable it, start it,
    /// and return to the prompt. Idempotent: installing twice replaces the
    /// definition and restarts the one copy.
    Install,
    /// Start an installed service.
    Start,
    /// Stop it, leaving it installed.
    Stop,
    /// Restart it — what to run after `wizard update` so the service picks up
    /// the new binary.
    Restart,
    /// Report whether it is installed, running, since when, and the last
    /// thing that went wrong.
    Status,
    /// Tail its log (the journal under systemd, ~/.wizard/logs under launchd).
    Logs {
        /// Follow the log instead of printing the tail and exiting.
        #[arg(short, long)]
        follow: bool,

        /// How many lines of history to print.
        #[arg(short = 'n', long, default_value_t = 50, value_name = "N")]
        lines: u32,
    },
    /// Stop it, disable it, and remove the definition. Removing something that
    /// is not installed is not an error.
    Uninstall,
}

/// Run one [`ServiceCmd`] against `spec` on this host.
///
/// The exit code is the shell's answer to "is it up?": `status` exits 0 when
/// the service is running and 1 otherwise, so `wizard gateway status &&` works
/// the way `systemctl is-active` does. Every other action exits 0 on success.
pub fn dispatch(spec: &ServiceSpec, cmd: ServiceCmd) -> Result<i32> {
    // Detection first, for every action: on a host with neither supervisor the
    // answer is the same refusal whether the operator asked to install or to
    // check the status, and it names what to do instead.
    let installer = Installer::detect()?;
    match cmd {
        ServiceCmd::Install => installer.install(spec),
        ServiceCmd::Start => installer.control(Action::Start, &spec.name),
        ServiceCmd::Stop => installer.control(Action::Stop, &spec.name),
        ServiceCmd::Restart => installer.control(Action::Restart, &spec.name),
        ServiceCmd::Status => {
            let status = installer.status(&spec.name)?;
            println!("{}", status.summary());
            if status.state == State::NotInstalled {
                println!("\n  install it with: {} install", spec.cli);
            }
            if let Some(advice) = installer.linger_advice()
                && status.state != State::NotInstalled
            {
                println!("\n{advice}");
            }
            Ok(i32::from(status.state != State::Running))
        }
        ServiceCmd::Logs { follow, lines } => installer.logs(&spec.name, follow, lines),
        ServiceCmd::Uninstall => installer.uninstall(&spec.name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A spec that does not touch the host: no `current_exe`, no `current_dir`.
    fn spec() -> ServiceSpec {
        ServiceSpec {
            name: "wizard-gateway".to_string(),
            description: "Wizard messaging gateway".to_string(),
            documentation: "https://example.invalid/gateway".to_string(),
            cli: "wizard gateway".to_string(),
            exe: PathBuf::from("/home/op/.local/bin/wizard"),
            args: vec!["--gateway".to_string()],
            working_dir: PathBuf::from("/home/op/projects/thing"),
            environment: vec![
                (
                    "PATH".to_string(),
                    "/home/op/.local/bin:/usr/bin".to_string(),
                ),
                ("RUST_LOG".to_string(), "info".to_string()),
            ],
            restart_secs: 5,
        }
    }

    #[test]
    fn a_systemd_unit_names_the_real_binary_the_project_and_a_restart_policy() {
        let installer = Installer::at(Manager::Systemd, "/nonexistent/systemd/user");
        let unit = installer.render(&spec()).expect("render");

        // The three things a hand-written unit gets wrong.
        assert!(
            unit.contains("ExecStart=/home/op/.local/bin/wizard --gateway"),
            "an absolute binary path, not the bare word `wizard`:\n{unit}"
        );
        assert!(
            unit.contains("WorkingDirectory=/home/op/projects/thing"),
            "the project captured at install time:\n{unit}"
        );
        assert!(unit.contains("Restart=always"), "{unit}");
        assert!(unit.contains("RestartSec=5"), "{unit}");
        assert!(unit.contains("WantedBy=default.target"), "{unit}");
        assert!(
            unit.contains("Description=Wizard messaging gateway"),
            "{unit}"
        );
        // The unit is a *user* unit: nothing in it may need root.
        assert!(!unit.contains("User="), "{unit}");
        assert_eq!(
            installer.unit_path("wizard-gateway"),
            PathBuf::from("/nonexistent/systemd/user/wizard-gateway.service")
        );
    }

    #[test]
    fn a_launchd_plist_names_the_real_binary_the_project_and_a_log() {
        let installer = Installer::at(Manager::Launchd, "/nonexistent/LaunchAgents");
        let plist = installer.render(&spec()).expect("render");

        assert!(
            plist.contains("<key>Label</key>\n  <string>com.teddytennant.wizard.gateway</string>"),
            "{plist}"
        );
        assert!(
            plist.contains("<string>/home/op/.local/bin/wizard</string>"),
            "{plist}"
        );
        assert!(plist.contains("<string>--gateway</string>"), "{plist}");
        assert!(
            plist.contains(
                "<key>WorkingDirectory</key>\n  <string>/home/op/projects/thing</string>"
            ),
            "{plist}"
        );
        assert!(plist.contains("<key>KeepAlive</key>"), "{plist}");
        assert!(plist.contains("<key>RunAtLoad</key>"), "{plist}");
        // launchd has no journal, so `logs` has nothing to tail unless the
        // plist points stdout and stderr at a file.
        assert!(plist.contains("<key>StandardOutPath</key>"), "{plist}");
        assert!(plist.contains("<key>StandardErrorPath</key>"), "{plist}");
        assert!(plist.contains("wizard-gateway.log"), "{plist}");
        assert_eq!(
            installer.unit_path("wizard-gateway"),
            PathBuf::from("/nonexistent/LaunchAgents/com.teddytennant.wizard.gateway.plist")
        );
    }

    /// Adversarial, and the reason this module exists rather than a doc: a
    /// service definition is world-readable, so a bot token in one is a bot
    /// token published to every local user and to `systemctl --user cat`.
    #[test]
    fn no_secret_can_reach_a_unit_file() {
        // 1. The carried list itself. Nothing on it may look like a secret,
        //    and the gateway's token variable is specifically not on it.
        for name in CARRIED_ENV {
            assert!(
                !looks_secret(name),
                "{name} must not be carried into a unit"
            );
        }
        assert!(
            !CARRIED_ENV.contains(&crate::config::GatewayConfig::DEFAULT_TOKEN_ENV),
            "the bot token variable is not carried into a unit"
        );
        assert!(looks_secret(
            crate::config::GatewayConfig::DEFAULT_TOKEN_ENV
        ));
        assert!(looks_secret("OPENAI_API_KEY") && looks_secret("aws_secret_access_key"));

        // 2. A spec that tries anyway is refused, before anything is rendered
        //    or written — this is the gate a future surface hits.
        let mut leaky = spec();
        leaky.environment.push((
            "WIZARD_TELEGRAM_TOKEN".to_string(),
            "7654321:AA-not-a-real-bot-token".to_string(),
        ));
        let err = leaky
            .validate()
            .expect_err("a token env var must be refused");
        assert!(
            format!("{err:#}").contains("credentials.toml"),
            "the refusal says where the secret belongs: {err:#}"
        );
        let installer = Installer::at(Manager::Systemd, "/nonexistent/systemd/user");
        assert!(
            installer.render(&leaky).is_err(),
            "render goes through validate"
        );

        // 3. And what a real render does emit carries only the named
        //    variables, so a secret cannot arrive by some other route.
        let unit = installer.render(&spec()).expect("render");
        for line in unit.lines().filter(|line| line.starts_with("Environment=")) {
            let name = line
                .trim_start_matches("Environment=\"")
                .split('=')
                .next()
                .expect("an environment line names a variable");
            assert!(
                CARRIED_ENV.contains(&name),
                "unit carries {name}, which is not on the carried list:\n{unit}"
            );
        }
        let lower = unit.to_ascii_lowercase();
        for marker in ["token", "secret", "password", "api_key"] {
            assert!(!lower.contains(marker), "unit mentions {marker}:\n{unit}");
        }
    }

    #[test]
    fn installing_twice_leaves_one_unit_and_rewrites_nothing() {
        let root = tempfile::tempdir().expect("tempdir");
        let installer = Installer::at(Manager::Systemd, root.path());
        let spec = spec();

        let (first, changed) = installer.write_unit(&spec).expect("write");
        assert!(changed, "the first install writes the unit");
        let (second, changed) = installer.write_unit(&spec).expect("write again");
        assert_eq!(first, second, "the path is a function of the name");
        assert!(!changed, "an unchanged spec rewrites nothing");

        let units: Vec<_> = std::fs::read_dir(root.path())
            .expect("read_dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(units, vec!["wizard-gateway.service".to_string()]);

        // A changed spec (the operator reinstalled from another project)
        // replaces the one unit rather than adding a second.
        let mut moved = spec.clone();
        moved.working_dir = PathBuf::from("/home/op/projects/other");
        let (path, changed) = installer.write_unit(&moved).expect("write moved");
        assert!(changed);
        assert_eq!(path, first);
        let unit = std::fs::read_to_string(&path).expect("read back");
        assert!(
            unit.contains("WorkingDirectory=/home/op/projects/other"),
            "{unit}"
        );
        assert!(!unit.contains("/home/op/projects/thing"), "{unit}");
        assert_eq!(std::fs::read_dir(root.path()).expect("read_dir").count(), 1);
    }

    #[test]
    fn removing_a_service_that_was_never_installed_is_not_an_error() {
        let root = tempfile::tempdir().expect("tempdir");
        let installer = Installer::at(Manager::Systemd, root.path());
        assert!(
            !installer
                .remove_unit("wizard-gateway")
                .expect("remove absent")
        );
        // And it is still absent, rather than having been created on the way.
        assert!(!installer.is_installed("wizard-gateway"));

        installer.write_unit(&spec()).expect("write");
        assert!(installer.remove_unit("wizard-gateway").expect("remove"));
        assert!(
            !installer
                .remove_unit("wizard-gateway")
                .expect("remove twice")
        );
    }

    #[test]
    fn starting_something_that_is_not_installed_is_answered_here_not_by_the_supervisor() {
        let root = tempfile::tempdir().expect("tempdir");
        let installer = Installer::at(Manager::Systemd, root.path());
        for (action, done) in [
            (Action::Start, "started"),
            (Action::Stop, "stopped"),
            (Action::Restart, "restarted"),
        ] {
            let err = format!(
                "{:#}",
                installer
                    .control(action, "wizard-gateway")
                    .expect_err("nothing to control")
            );
            // Ours, not `systemctl`'s "Unit not found", which does not say
            // what to do about it.
            assert!(err.contains("Install it first"), "{err}");
            assert!(err.contains(action.verb()), "{err}");
            assert_eq!(action.done(), done, "the confirmation line is a real word");
        }
    }

    /// The refusal path, for the hosts this machine is not.
    #[test]
    fn an_unsupported_host_cannot_even_name_a_unit_path() {
        // Termux: Linux, but no systemd anywhere, and the answer is a tool it
        // genuinely has.
        let termux = Probe {
            os: "linux",
            termux: true,
            systemd: false,
        };
        let err = format!(
            "{:#}",
            manager_for(&termux).expect_err("termux has no systemd")
        );
        assert!(err.contains("termux-services"), "{err}");
        assert!(err.contains("sv-enable"), "{err}");

        // Termux wins even if something else claims systemd is present.
        assert!(
            manager_for(&Probe {
                systemd: true,
                ..termux
            })
            .is_err()
        );

        // Non-systemd Linux: name the alternative rather than write a unit
        // nothing will ever read.
        let openrc = Probe {
            os: "linux",
            termux: false,
            systemd: false,
        };
        let err = format!("{:#}", manager_for(&openrc).expect_err("no systemd"));
        assert!(err.contains("OpenRC") && err.contains("runit"), "{err}");
        assert!(err.contains("docs/services.md"), "{err}");

        // An OS with neither.
        assert!(
            manager_for(&Probe {
                os: "windows",
                termux: false,
                systemd: false
            })
            .is_err()
        );

        // The supported two.
        assert_eq!(
            manager_for(&Probe {
                os: "linux",
                termux: false,
                systemd: true
            })
            .expect("systemd host"),
            Manager::Systemd
        );
        assert_eq!(
            manager_for(&Probe {
                os: "macos",
                termux: false,
                systemd: false
            })
            .expect("macos host"),
            Manager::Launchd
        );

        // Nothing above wrote a file, and nothing could have: every writing
        // function hangs off `Installer`, and the only way to build one for
        // this host is `detect`, which starts with the check just exercised.
    }

    #[test]
    fn status_is_honest_about_absent_stopped_running_and_failed() {
        let root = tempfile::tempdir().expect("tempdir");
        let installer = Installer::at(Manager::Systemd, root.path());

        // Not installed: answered from the filesystem, so no supervisor is
        // consulted and this works in a container with no systemd at all.
        let status = installer.status("wizard-gateway").expect("status");
        assert_eq!(status.state, State::NotInstalled);
        assert!(
            status.summary().contains("not installed"),
            "{}",
            status.summary()
        );

        let unit = installer.unit_path("wizard-gateway");
        // Installed but stopped.
        let stopped = status_from_systemd_show(
            "wizard-gateway",
            unit.clone(),
            "ActiveState=inactive\nSubState=dead\nUnitFileState=enabled\nResult=success\n\
             ExecMainStatus=0\nNRestarts=0\nActiveEnterTimestamp=\n",
        );
        assert_eq!(stopped.state, State::Stopped);
        assert_eq!(stopped.enabled, Some(true));
        assert!(stopped.detail.is_none());
        assert!(stopped.summary().contains("installed, stopped"));

        // Running, with a start time.
        let running = status_from_systemd_show(
            "wizard-gateway",
            unit.clone(),
            "ActiveState=active\nSubState=running\nUnitFileState=enabled\nResult=success\n\
             ExecMainStatus=0\nNRestarts=0\nActiveEnterTimestamp=Fri 2026-08-07 09:12:00 UTC\n",
        );
        assert_eq!(running.state, State::Running);
        assert_eq!(
            running.since.as_deref(),
            Some("Fri 2026-08-07 09:12:00 UTC")
        );
        assert!(running.summary().contains("running"));

        // Failed, and the last error survives into the summary — that is the
        // whole point of reading `Result` rather than `ActiveState` alone.
        let failed = status_from_systemd_show(
            "wizard-gateway",
            unit.clone(),
            "ActiveState=failed\nSubState=failed\nUnitFileState=enabled\nResult=exit-code\n\
             ExecMainStatus=1\nNRestarts=4\nActiveEnterTimestamp=\n",
        );
        assert_eq!(failed.state, State::Failed);
        let summary = failed.summary();
        assert!(summary.contains("exit-code"), "{summary}");
        assert!(summary.contains("exit status 1"), "{summary}");
        assert!(summary.contains("4 restarts"), "{summary}");

        // A flapping service that is back up still reports what went wrong.
        let flapping = status_from_systemd_show(
            "wizard-gateway",
            unit.clone(),
            "ActiveState=active\nResult=exit-code\nExecMainStatus=1\nNRestarts=9\n",
        );
        assert_eq!(flapping.state, State::Running);
        assert!(
            flapping.detail.is_some(),
            "a restarted failure is still reported"
        );

        // Nothing at all back from systemd is `unknown`, not `stopped`: a
        // container with no session bus must not report a running service as
        // down.
        let mute = status_from_systemd_show("wizard-gateway", unit, "");
        assert_eq!(mute.state, State::Unknown);
        assert!(mute.enabled.is_none());
    }

    #[test]
    fn launchd_status_reads_the_pid_and_the_last_exit() {
        let unit = PathBuf::from("/tmp/com.teddytennant.wizard.gateway.plist");
        let running = status_from_launchctl(
            "wizard-gateway",
            unit.clone(),
            "{\n\t\"LimitLoadToSessionType\" = \"Aqua\";\n\t\"Label\" = \"com.teddytennant.wizard.gateway\";\n\
             \t\"OnDemand\" = false;\n\t\"LastExitStatus\" = 0;\n\t\"PID\" = 4213;\n}\n",
            true,
        );
        assert_eq!(running.state, State::Running);
        assert_eq!(running.since.as_deref(), Some("pid 4213"));

        let failed = status_from_launchctl(
            "wizard-gateway",
            unit.clone(),
            "{\n\t\"LastExitStatus\" = 256;\n}\n",
            true,
        );
        assert_eq!(failed.state, State::Failed);
        assert_eq!(failed.detail.as_deref(), Some("exit status 256"));

        // The plist is on disk but launchd does not know the label.
        let unloaded = status_from_launchctl("wizard-gateway", unit, "", false);
        assert_eq!(unloaded.state, State::Stopped);
        assert_eq!(unloaded.enabled, Some(false));
    }

    /// Lingering off is the difference between a service that survives logout
    /// and one that does not, so the operator is told in words, with the
    /// command to run.
    #[test]
    fn linger_off_tells_the_operator_exactly_what_to_run() {
        let off = linger_advice_from(Some("Linger=no\n"), "op").expect("advice when off");
        assert!(off.contains("sudo loginctl enable-linger op"), "{off}");
        assert!(off.contains("log out"), "{off}");

        // On: nothing to say.
        assert_eq!(linger_advice_from(Some("Linger=yes\n"), "op"), None);

        // Unknown (no loginctl, or it failed): say so, and still give the
        // command — silence here reads as "all good", which it is not.
        let unknown = linger_advice_from(None, "op").expect("advice when unknown");
        assert!(unknown.contains("enable-linger op"), "{unknown}");
        assert!(unknown.contains("could not check"), "{unknown}");

        // A launchd installer never mentions lingering: LaunchAgents load at
        // login and the concept does not exist there.
        assert_eq!(
            Installer::at(Manager::Launchd, "/nonexistent").linger_advice(),
            None
        );
    }

    #[test]
    fn a_spec_is_refused_before_it_can_render_something_broken() {
        let mut bad = spec();
        bad.name = "Wizard Gateway".to_string();
        assert!(
            bad.validate().is_err(),
            "a name with spaces escapes the file name"
        );

        let mut relative = spec();
        relative.exe = PathBuf::from("wizard");
        let err = format!("{:#}", relative.validate().expect_err("relative binary"));
        assert!(err.contains("absolute"), "{err}");

        let mut relative_dir = spec();
        relative_dir.working_dir = PathBuf::from("projects/thing");
        assert!(relative_dir.validate().is_err());

        let mut newline = spec();
        newline.environment.push((
            "WIZARD_HOME".to_string(),
            "/home/op\nExecStart=/bin/sh".to_string(),
        ));
        assert!(
            newline.validate().is_err(),
            "a newline in a value would forge a directive"
        );
    }

    #[test]
    fn unit_text_escapes_what_the_formats_would_otherwise_eat() {
        // systemd: `%` is a specifier introducer, so a literal one has to be
        // doubled or the unit expands (or refuses to load).
        let mut percent = spec();
        percent.working_dir = PathBuf::from("/home/op/100% done");
        percent.exe = PathBuf::from("/home/op/my tools/wizard");
        let unit = Installer::at(Manager::Systemd, "/nonexistent")
            .render(&percent)
            .expect("render");
        assert!(
            unit.contains("WorkingDirectory=/home/op/100%% done"),
            "{unit}"
        );
        // A path with a space has to be quoted or systemd splits it in two.
        assert!(
            unit.contains("ExecStart=\"/home/op/my tools/wizard\" --gateway"),
            "{unit}"
        );

        // The plist is XML, so `&` and `<` have to be entities.
        let mut xml = spec();
        xml.working_dir = PathBuf::from("/home/op/a&b<c");
        let plist = Installer::at(Manager::Launchd, "/nonexistent")
            .render(&xml)
            .expect("render");
        assert!(plist.contains("/home/op/a&amp;b&lt;c"), "{plist}");
    }

    #[test]
    fn the_carried_environment_always_names_a_log_level() {
        // A unit with no RUST_LOG at all is a unit whose journal is empty when
        // the operator finally goes looking.
        let env = inherited_env();
        assert!(
            env.iter()
                .any(|(name, value)| name == "RUST_LOG" && !value.is_empty()),
            "{env:?}"
        );
        for (name, value) in &env {
            assert!(CARRIED_ENV.contains(&name.as_str()), "carried {name}");
            assert!(!looks_secret(name), "carried a secret-looking {name}");
            assert!(!has_control(value), "carried a control character in {name}");
        }
    }

    /// Live systemd, when this host has one. Skipped in CI containers, which
    /// have no session bus — the same shape as the root skip in `mesh::node`.
    #[test]
    fn a_live_systemd_agrees_that_an_uninstalled_service_is_not_there() {
        let Ok(installer) = Installer::detect() else {
            // No supervisor here (a container, Termux). Detection already
            // refused, which the test above covers.
            return;
        };
        if installer.manager() != Manager::Systemd
            || capture("systemctl", &["--user", "is-system-running"]).is_err()
        {
            // macOS, or no systemctl on PATH: nothing live to ask.
            return;
        }
        // A name no install ever uses. The point is that `status` answers from
        // the filesystem first, so a real `systemctl --user show` of an unknown
        // unit cannot turn "not installed" into something else.
        let status = installer
            .status("wizard-service-selftest-absent")
            .expect("status");
        assert_eq!(status.state, State::NotInstalled);
    }
}
