//! Command-line argument parsing (`clap` derive).
//!
//! Flags per `docs/architecture.md` (CLI section) and `docs/modes.md`.

use std::path::PathBuf;

use clap::Parser;

use crate::config::Mode;

/// Wizard — your sovereign agent. Self-extending. Bring any model.
#[derive(Debug, Clone, Parser)]
#[command(name = "wizard", version = crate::update::display_version(), about, long_about = None)]
pub struct Cli {
    /// Personality mode: genie (interactive TUI) or sovereign (autonomous).
    #[arg(long, value_enum)]
    pub mode: Option<Mode>,

    /// Initial task. Pre-fills the first message in genie mode; the task to
    /// complete in sovereign / evolve mode.
    #[arg(short, long)]
    pub prompt: Option<String>,

    /// Self-extension mode: run the /evolve pipeline from the CLI.
    #[arg(long)]
    pub evolve: bool,

    /// Deep evolve (tier 2): rebuild Wizard's own source. Implies --evolve.
    #[arg(long, requires = "evolve")]
    pub deep: bool,

    /// Fork Wizard to your GitHub and print a one-line installer for your
    /// variant. Requires `gh` authenticated (`gh auth login`).
    #[arg(long)]
    pub publish: bool,

    /// Start in plan mode: the agent investigates with read-only tools and
    /// presents a plan via the exit_plan tool before executing. The TUI asks
    /// for approval; headless runs and the gateway auto-approve, giving a
    /// natural plan-then-execute turn.
    #[arg(long)]
    pub plan: bool,

    /// Start in omakase (chef's-choice) mode: plan mode where the agent
    /// explores read-only, decides the approach itself, and auto-approves its
    /// own plan — no interview, no review gate. Implies `--plan`.
    #[arg(long)]
    pub omakase: bool,

    /// Time limit in hours for a sovereign-mode run.
    #[arg(long)]
    pub max_hours: Option<f64>,

    /// Max outer loop iterations for a sovereign-mode run.
    #[arg(long = "loop", value_name = "N")]
    pub loop_limit: Option<u32>,

    /// Run sovereign mode perpetually: keep working toward the goal,
    /// self-directing and self-improving, until stopped (loop-control
    /// `stop` or --max-hours). Implies --mode sovereign.
    #[arg(long)]
    pub continuous: bool,

    /// Internal: this headless run was dispatched from `/dashboard`, so it
    /// registers in the session registry and persists its terminal state for
    /// the dashboard to display.
    #[arg(long, hide = true)]
    pub bg: bool,

    /// Output format for headless (sovereign `-p`) runs: `text` streams
    /// human-readable output (default), `json` emits one final JSON summary
    /// object, `stream-json` emits one JSON object per line as events
    /// arrive. Ignored by the TUI and the gateway.
    #[arg(long, value_enum, default_value_t)]
    pub output_format: crate::output::OutputFormat,

    /// Project root override (defaults to the current directory).
    #[arg(long)]
    pub cwd: Option<PathBuf>,

    /// Resume the most recent session instead of starting fresh.
    #[arg(long)]
    pub resume: bool,

    /// Re-run the first-run onboarding wizard even if a config already exists.
    #[arg(long)]
    pub onboard: bool,

    /// Run the messaging gateway (e.g. Telegram) instead of the TUI. Reads the
    /// `[gateway]` section of config.toml; a long-running headless process.
    #[arg(long)]
    pub gateway: bool,

    /// Sign in to a provider account instead of starting the TUI: `xai`
    /// (SuperGrok) or `chatgpt` (Plus/Pro/Team). OAuth in the browser; tokens
    /// are stored under ~/.wizard/.
    #[arg(long, value_name = "PROVIDER")]
    pub login: Option<String>,

    /// Harness bundle directory (sets `$WIZARD_HARNESS_DIR`): per-component
    /// overrides for the compiled harness defaults — system_prompt.md,
    /// tool_descriptions/, skills/, subagents/. Missing files fall back to
    /// the baked defaults. Produce a bundle with `wizard harness export`.
    #[arg(long, value_name = "DIR")]
    pub harness_dir: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

impl Cli {
    /// Names of top-level flags set on this invocation that a self-contained
    /// subcommand (bench, doctor, schedule, scheduler, fleet, usage, evolve,
    /// update, sync) would silently ignore. `--cwd` is honored everywhere and
    /// excluded.
    /// [`crate::run`] turns a non-empty result into a hard error rather than
    /// silently dropping the flags.
    pub fn ignored_top_level_flags(&self) -> Vec<&'static str> {
        let mut ignored = Vec::new();
        if self.mode.is_some() {
            ignored.push("--mode");
        }
        if self.prompt.is_some() {
            ignored.push("--prompt");
        }
        if self.evolve {
            ignored.push("--evolve");
        }
        if self.deep {
            ignored.push("--deep");
        }
        if self.publish {
            ignored.push("--publish");
        }
        if self.plan {
            ignored.push("--plan");
        }
        if self.omakase {
            ignored.push("--omakase");
        }
        if self.max_hours.is_some() {
            ignored.push("--max-hours");
        }
        if self.loop_limit.is_some() {
            ignored.push("--loop");
        }
        if self.continuous {
            ignored.push("--continuous");
        }
        if self.bg {
            ignored.push("--bg");
        }
        if self.output_format != crate::output::OutputFormat::Text {
            ignored.push("--output-format");
        }
        if self.resume {
            ignored.push("--resume");
        }
        if self.onboard {
            ignored.push("--onboard");
        }
        if self.gateway {
            ignored.push("--gateway");
        }
        if self.login.is_some() {
            ignored.push("--login");
        }
        ignored
    }
}

/// Top-level subcommands. Absent for the classic flag-driven modes.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum Command {
    /// Benchmark harness: record real tasks, replay them against any agent
    /// CLI, compare pass rates.
    Bench {
        #[command(subcommand)]
        cmd: BenchCmd,
    },

    /// Diagnose the environment: config, providers, MCP servers, tools,
    /// hooks, writable state dirs, checkpoints. Exits 0 when no check
    /// failed.
    Doctor,

    /// Manage scheduled runs (~/.wizard/schedule.toml): cron entries the
    /// `wizard scheduler` daemon fires as headless wizard runs.
    Schedule {
        #[command(subcommand)]
        cmd: ScheduleCmd,
    },

    /// Run the scheduler daemon in the foreground: reload
    /// ~/.wizard/schedule.toml each pass and fire due entries as headless
    /// wizard child processes. Daemonize externally (e.g. systemd); see
    /// docs/scheduler.md.
    Scheduler,

    /// Fleet mode: decompose a mission into independent tasks and run them
    /// as parallel headless workers, each in its own git worktree, then
    /// merge the fleet branches back. See docs/fleet.md.
    Fleet {
        #[command(subcommand)]
        cmd: FleetCmd,
    },

    /// Open the agent dashboard: every running Wizard session on the machine.
    /// Dispatch background sessions, watch their state, peek their output, and
    /// stop them. Same view as `/dashboard` inside a session.
    Agents,

    /// Harness bundle tooling: export the compiled harness defaults
    /// (system prompt, tool descriptions, skills, subagents) as an editable
    /// bundle for external harness-evolution loops. Load one with
    /// `--harness-dir` / `$WIZARD_HARNESS_DIR`.
    Harness {
        #[command(subcommand)]
        cmd: HarnessCmd,
    },

    /// Serve the browser GUI: a local web app (task sidebar, streaming
    /// conversation, git panel) over the same agent core as the TUI. Binds
    /// 127.0.0.1 only; agents are built lazily per task, so the server
    /// starts fine without a reachable provider.
    Gui {
        /// Port to bind on 127.0.0.1. Fails when the port is taken.
        #[arg(long, default_value_t = 4680)]
        port: u16,

        /// Do not open the browser after binding.
        #[arg(long)]
        no_open: bool,

        /// Serve GUI assets from this directory instead of the embedded
        /// copies (dev mode: edit gui/assets/ and reload).
        #[arg(long, value_name = "DIR")]
        assets: Option<PathBuf>,
    },

    /// Roll up ~/.wizard/usage.jsonl: turns, tokens, and estimated cost per
    /// project and per provider. Self-contained; never loads config.
    Usage {
        /// Only include turns from the last N days (e.g. `--since 7d`).
        #[arg(long, value_name = "DAYS")]
        since: Option<String>,
    },

    /// Inspect and roll back self-extensions recorded in
    /// ~/.wizard/evolution.jsonl.
    Evolve {
        #[command(subcommand)]
        cmd: EvolveCmd,
    },

    /// Update Wizard in place: download the latest release binary from GitHub,
    /// verify its checksum against `checksums.txt`, and swap it in atomically
    /// (the previous binary is kept as `<name>.bak`). Self-contained: never
    /// loads config or triggers onboarding.
    Update {
        /// Report whether a newer release exists without installing anything.
        #[arg(long)]
        check: bool,

        /// Install this exact tag (e.g. `v0.5.0`) instead of the latest.
        #[arg(long, value_name = "TAG")]
        to: Option<String>,

        /// Reinstall even when the running version is already up to date.
        #[arg(long)]
        force: bool,

        /// Restore the previous binary from the pre-update `<name>.bak` backup.
        #[arg(long)]
        rollback: bool,
    },

    /// Sync config and skills across machines: pack the portable parts of
    /// ~/.wizard into a signed bundle, pull and verify one from a file or
    /// URL. Self-contained: never loads config or triggers onboarding.
    Sync {
        #[command(subcommand)]
        cmd: SyncCmd,
    },
}

/// `wizard sync` subcommands. Self-contained like update: no config load
/// (pull reads `[sync].source` from config.toml directly), no onboarding,
/// no LLM. Bundles are ed25519-signed; trust is pinned on first use in
/// `~/.wizard/sync/trusted_keys` (compare fingerprints via `wizard sync key`).
#[derive(Debug, Clone, clap::Subcommand)]
pub enum SyncCmd {
    /// Create a signed bundle of portable ~/.wizard state: config.toml,
    /// mcp.toml, system_prompt.md, and the skills/, commands/, subagents/,
    /// and tools/ directories. Credentials stay out unless explicitly
    /// included.
    Pack {
        /// Output path (default: `wizard-sync-<YYYYMMDD>.tar.gz` in the
        /// current directory).
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,

        /// Also pack credentials.toml and xai_oauth.json. The bundle then
        /// contains API keys — transfer it privately. The bundle file is
        /// written with 0600 permissions.
        #[arg(long)]
        include_credentials: bool,
    },

    /// Fetch, verify, and apply a bundle (file path or http(s) URL). Falls
    /// back to `[sync].source` in config.toml when no source is given.
    /// Additive only: replaced files are backed up under
    /// ~/.wizard/sync/backups/, nothing is deleted.
    Pull {
        /// Bundle to pull: a local file path (`~` expands) or an http(s) URL.
        source: Option<String>,

        /// Verify the bundle and show what would change without writing
        /// anything.
        #[arg(long)]
        dry_run: bool,
    },

    /// Print this machine's sync public key and fingerprint (generating the
    /// keypair on first use). Compare the fingerprint against what `pull`
    /// reports on the other machine.
    Key,
}

/// `wizard evolve` subcommands. Self-contained: they read
/// `~/.wizard/evolution.jsonl` and touch the recorded artifacts directly —
/// no config load, no LLM.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum EvolveCmd {
    /// List recorded evolutions, most recent first (#1 is the newest).
    List,

    /// Undo evolution #N from `wizard evolve list`: delete the files a
    /// runtime evolution created, or restore the `.prev` binary for a deep
    /// one. Refuses when the artifacts are already gone.
    Undo {
        /// Entry number as shown by `wizard evolve list`.
        n: usize,
    },
}

/// `wizard harness` subcommands. Self-contained like bench: no config load,
/// no onboarding, no LLM.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum HarnessCmd {
    /// Write the compiled harness defaults into `dir` as a bundle:
    /// `system_prompt.md`, `tool_descriptions/<tool>.md`,
    /// `skills/<name>/SKILL.md`, `subagents/<name>.toml`, plus a generated
    /// `HARNESS.md` describing each component.
    Export {
        /// Target directory (created if missing; existing files overwritten).
        dir: PathBuf,
    },
}

/// `wizard fleet` subcommands. `run` loads config (the coordinator drives a
/// real agent for planning and synthesis); `status` and `stop` only touch
/// the project's `.wizard/fleet/` directory.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum FleetCmd {
    /// Plan the mission, spawn up to N parallel workers over git worktrees,
    /// supervise them, then synthesize (merge the fleet branches).
    Run {
        /// Number of parallel workers.
        #[arg(short = 'n', long = "workers", value_name = "N")]
        n: usize,

        /// Mission prompt, decomposed into independent tasks by a planning
        /// turn.
        #[arg(short, long)]
        prompt: String,
    },

    /// Show the fleet state: mission, status, and a per-task table.
    Status,

    /// Ask a running fleet to wind down (writes the stop sentinel; the
    /// coordinator kills its workers on the next supervision tick).
    Stop,
}

/// `wizard schedule` subcommands. Like bench, these are self-contained:
/// they edit `~/.wizard/schedule.toml` directly and never load
/// `~/.wizard/config.toml` or trigger onboarding.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum ScheduleCmd {
    /// Add an entry; validates the cron expression and prints the next
    /// fire time.
    Add {
        /// Unique entry name; `[a-zA-Z0-9_-]+` only.
        name: String,

        /// Standard 5-field cron expression (minute hour day month weekday),
        /// evaluated in local time.
        #[arg(long)]
        cron: String,

        /// Task prompt handed to the spawned headless wizard run.
        #[arg(long)]
        prompt: String,

        /// Directory the run executes in (must exist).
        #[arg(long)]
        cwd: PathBuf,

        /// Wall-clock cap in hours for the spawned run.
        #[arg(long)]
        max_hours: Option<f64>,

        /// Run mode for the job: `sovereign` (default) or `continuous`.
        #[arg(long, default_value = "sovereign")]
        mode: String,
    },

    /// List entries with their next fire times.
    List,

    /// Remove an entry by name.
    Remove {
        /// Entry name as shown by `wizard schedule list`.
        name: String,
    },

    /// Enable a disabled entry (the daemon fires it again).
    Enable {
        /// Entry name as shown by `wizard schedule list`.
        name: String,
    },

    /// Disable an entry without removing it (kept in the file, never fired).
    Disable {
        /// Entry name as shown by `wizard schedule list`.
        name: String,
    },

    /// Run one entry's job immediately in the foreground (same child
    /// command the daemon would spawn); exits with the child's exit code.
    Run {
        /// Entry name as shown by `wizard schedule list`.
        name: String,
    },
}

/// `wizard bench` subcommands. Self-contained: no flag here depends on the
/// top-level flags (which are not global), and none of them load
/// `~/.wizard/config.toml` or trigger onboarding.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum BenchCmd {
    /// Create a benchmark case by hand.
    Add {
        /// Case id; `[a-zA-Z0-9_-]+` only (it becomes a file and worktree name).
        #[arg(long)]
        id: String,

        /// Task prompt handed to the harness command.
        #[arg(long)]
        prompt: String,

        /// Shell command run in the replayed worktree; exit 0 means pass.
        #[arg(long)]
        check: String,

        /// Git ref to replay from; resolved to a full commit sha at add time.
        #[arg(long, default_value = "HEAD")]
        git_ref: String,

        /// Harness timeout in seconds.
        #[arg(long, default_value_t = 900)]
        timeout: u64,

        /// Check-command timeout in seconds.
        #[arg(long, default_value_t = 300)]
        check_timeout: u64,

        /// Tag for grouping cases (repeatable).
        #[arg(long = "tag")]
        tag: Vec<String>,

        /// Free-form notes stored with the case.
        #[arg(long)]
        notes: Option<String>,
    },

    /// List cases (and recorded trajectories with --trajectories).
    List {
        /// Also show the last 20 recorded trajectories.
        #[arg(long)]
        trajectories: bool,

        /// Show only cases carrying this tag (repeatable).
        #[arg(long = "tag")]
        tag: Vec<String>,
    },

    /// Promote a recorded trajectory into a case by attaching a check command.
    Promote {
        /// Trajectory id or unique id-prefix (see `wizard bench list --trajectories`).
        trajectory: String,

        /// Shell command run in the replayed worktree; exit 0 means pass.
        #[arg(long)]
        check: String,

        /// Case id; defaults to the trajectory id.
        #[arg(long)]
        id: Option<String>,

        /// Harness timeout in seconds.
        #[arg(long, default_value_t = 900)]
        timeout: u64,

        /// Check-command timeout in seconds.
        #[arg(long, default_value_t = 300)]
        check_timeout: u64,

        /// Tag for grouping cases (repeatable).
        #[arg(long = "tag")]
        tag: Vec<String>,

        /// Free-form notes stored with the case.
        #[arg(long)]
        notes: Option<String>,
    },

    /// Replay cases against a harness command in isolated git worktrees.
    Run {
        /// Harness command template; `{prompt}` is replaced with the
        /// shell-quoted case prompt. Defaults to this binary in sovereign mode.
        #[arg(long)]
        runner: Option<String>,

        /// Label stored in the result file name and summary.
        #[arg(long, default_value = "run")]
        label: String,

        /// Run only this case id (repeatable; default: all cases).
        #[arg(long = "case")]
        case: Vec<String>,

        /// Run only cases carrying this tag (repeatable; combines with
        /// --case as a union).
        #[arg(long = "tag")]
        tag: Vec<String>,

        /// Keep the per-case worktrees for inspection instead of removing them.
        #[arg(long)]
        keep_worktrees: bool,
    },

    /// Remove a case by id.
    Remove {
        /// Case id as shown by `wizard bench list`.
        id: String,
    },

    /// Compare two result files case-by-case.
    Compare {
        /// First result JSON (baseline).
        a: PathBuf,

        /// Second result JSON.
        b: PathBuf,
    },
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};

    use super::*;
    use crate::config::Mode;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("wizard").chain(args.iter().copied()))
    }

    #[test]
    fn clap_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn defaults_when_no_args() {
        let cli = parse(&[]).expect("bare invocation parses");
        assert_eq!(cli.mode, None);
        assert_eq!(cli.prompt, None);
        assert!(!cli.evolve);
        assert!(!cli.deep);
        assert!(!cli.plan);
        assert_eq!(cli.max_hours, None);
        assert_eq!(cli.loop_limit, None);
        assert!(!cli.continuous);
        assert_eq!(cli.output_format, crate::output::OutputFormat::Text);
        assert_eq!(cli.cwd, None);
        assert!(!cli.resume);
        assert!(!cli.onboard);
        assert!(!cli.gateway);
        assert_eq!(cli.login, None);
        assert!(cli.command.is_none(), "bare wizard has no subcommand");
    }

    #[test]
    fn login_flag_takes_a_provider() {
        let cli = parse(&["--login", "xai"]).expect("--login xai parses");
        assert_eq!(cli.login.as_deref(), Some("xai"));

        let err = parse(&["--login"]).expect_err("--login without a provider is rejected");
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);
    }

    #[test]
    fn parses_all_documented_flags() {
        let cli = parse(&[
            "--mode",
            "sovereign",
            "-p",
            "add tests",
            "--plan",
            "--max-hours",
            "1.5",
            "--loop",
            "10",
            "--continuous",
            "--cwd",
            "/tmp/project",
            "--resume",
            "--onboard",
            "--gateway",
        ])
        .expect("full flag set parses");
        assert_eq!(cli.mode, Some(Mode::Sovereign));
        assert_eq!(cli.prompt.as_deref(), Some("add tests"));
        assert!(cli.plan);
        assert_eq!(cli.max_hours, Some(1.5));
        assert_eq!(cli.loop_limit, Some(10));
        assert!(cli.continuous);
        assert_eq!(
            cli.cwd.as_deref(),
            Some(std::path::Path::new("/tmp/project"))
        );
        assert!(cli.resume);
        assert!(cli.onboard);
        assert!(cli.gateway);
    }

    #[test]
    fn long_prompt_flag_works() {
        let cli = parse(&["--prompt", "task"]).expect("long form parses");
        assert_eq!(cli.prompt.as_deref(), Some("task"));
    }

    #[test]
    fn evolve_flags() {
        let cli = parse(&["--evolve", "-p", "add a skill"]).expect("evolve parses");
        assert!(cli.evolve);
        assert!(!cli.deep);

        let cli = parse(&["--evolve", "--deep", "-p", "new panel"]).expect("deep evolve parses");
        assert!(cli.evolve);
        assert!(cli.deep);
    }

    #[test]
    fn deep_requires_evolve() {
        let err = parse(&["--deep"]).expect_err("--deep alone must be rejected");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn output_format_parses_all_values() {
        use crate::output::OutputFormat;
        for (raw, expected) in [
            ("text", OutputFormat::Text),
            ("json", OutputFormat::Json),
            ("stream-json", OutputFormat::StreamJson),
        ] {
            let cli = parse(&["--output-format", raw]).expect("format parses");
            assert_eq!(cli.output_format, expected);
        }
        let err = parse(&["--output-format", "yaml"]).expect_err("unknown format rejected");
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);
    }

    #[test]
    fn rejects_unknown_mode() {
        let err = parse(&["--mode", "warlock"]).expect_err("unknown mode must be rejected");
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);
    }

    #[test]
    fn doctor_parses_as_a_subcommand() {
        let cli = parse(&["doctor"]).expect("doctor parses");
        assert!(matches!(cli.command, Some(Command::Doctor)));
    }

    #[test]
    fn update_parses_as_a_subcommand() {
        let cli = parse(&["update"]).expect("update parses");
        assert!(matches!(
            cli.command,
            Some(Command::Update {
                check: false,
                to: None,
                force: false,
                rollback: false,
            })
        ));

        let cli = parse(&[
            "update",
            "--check",
            "--to",
            "v0.5.0",
            "--force",
            "--rollback",
        ])
        .expect("update flags parse");
        let Some(Command::Update {
            check,
            to,
            force,
            rollback,
        }) = cli.command
        else {
            panic!("expected update");
        };
        assert!(check);
        assert_eq!(to.as_deref(), Some("v0.5.0"));
        assert!(force);
        assert!(rollback);
    }

    #[test]
    fn sync_subcommands_parse() {
        let cli = parse(&["sync", "pack"]).expect("sync pack parses");
        assert!(matches!(
            cli.command,
            Some(Command::Sync {
                cmd: SyncCmd::Pack {
                    out: None,
                    include_credentials: false,
                }
            })
        ));

        let cli = parse(&[
            "sync",
            "pack",
            "--out",
            "/tmp/b.tar.gz",
            "--include-credentials",
        ])
        .expect("sync pack flags parse");
        let Some(Command::Sync {
            cmd:
                SyncCmd::Pack {
                    out,
                    include_credentials,
                },
        }) = cli.command
        else {
            panic!("expected sync pack");
        };
        assert_eq!(out, Some(PathBuf::from("/tmp/b.tar.gz")));
        assert!(include_credentials);

        let cli = parse(&["sync", "pull"]).expect("sync pull without a source parses");
        assert!(matches!(
            cli.command,
            Some(Command::Sync {
                cmd: SyncCmd::Pull {
                    source: None,
                    dry_run: false,
                }
            })
        ));

        let cli = parse(&["sync", "pull", "~/b.tar.gz", "--dry-run"])
            .expect("sync pull with a source parses");
        let Some(Command::Sync {
            cmd: SyncCmd::Pull { source, dry_run },
        }) = cli.command
        else {
            panic!("expected sync pull");
        };
        assert_eq!(source.as_deref(), Some("~/b.tar.gz"));
        assert!(dry_run);

        let cli = parse(&["sync", "key"]).expect("sync key parses");
        assert!(matches!(
            cli.command,
            Some(Command::Sync { cmd: SyncCmd::Key })
        ));

        let err = parse(&["sync"]).expect_err("bare sync requires a subcommand");
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand,
            "got: {err}"
        );
    }

    #[test]
    fn scheduler_parses_as_a_subcommand() {
        let cli = parse(&["scheduler"]).expect("scheduler parses");
        assert!(matches!(cli.command, Some(Command::Scheduler)));
    }

    #[test]
    fn schedule_add_parses_with_defaults() {
        let cli = parse(&[
            "schedule",
            "add",
            "nightly",
            "--cron",
            "0 3 * * *",
            "--prompt",
            "tidy up",
            "--cwd",
            "/tmp/proj",
        ])
        .expect("schedule add parses");
        let Some(Command::Schedule {
            cmd:
                ScheduleCmd::Add {
                    name,
                    cron,
                    prompt,
                    cwd,
                    max_hours,
                    mode,
                },
        }) = cli.command
        else {
            panic!("expected schedule add");
        };
        assert_eq!(name, "nightly");
        assert_eq!(cron, "0 3 * * *");
        assert_eq!(prompt, "tidy up");
        assert_eq!(cwd, PathBuf::from("/tmp/proj"));
        assert_eq!(max_hours, None);
        assert_eq!(mode, "sovereign");
    }

    #[test]
    fn schedule_add_requires_cron_prompt_and_cwd() {
        let err = parse(&["schedule", "add", "nightly"]).expect_err("missing args rejected");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn schedule_list_remove_and_run_parse() {
        let cli = parse(&["schedule", "list"]).expect("schedule list parses");
        assert!(matches!(
            cli.command,
            Some(Command::Schedule {
                cmd: ScheduleCmd::List
            })
        ));

        let cli = parse(&["schedule", "remove", "nightly"]).expect("schedule remove parses");
        let Some(Command::Schedule {
            cmd: ScheduleCmd::Remove { name },
        }) = cli.command
        else {
            panic!("expected schedule remove");
        };
        assert_eq!(name, "nightly");

        let cli = parse(&["schedule", "run", "nightly"]).expect("schedule run parses");
        let Some(Command::Schedule {
            cmd: ScheduleCmd::Run { name },
        }) = cli.command
        else {
            panic!("expected schedule run");
        };
        assert_eq!(name, "nightly");
    }

    #[test]
    fn fleet_run_parses_workers_and_prompt() {
        let cli = parse(&["fleet", "run", "-n", "3", "-p", "improve coverage"])
            .expect("fleet run parses");
        let Some(Command::Fleet {
            cmd: FleetCmd::Run { n, prompt },
        }) = cli.command
        else {
            panic!("expected fleet run");
        };
        assert_eq!(n, 3);
        assert_eq!(prompt, "improve coverage");

        let cli =
            parse(&["fleet", "run", "--workers", "2", "--prompt", "x"]).expect("long forms parse");
        assert!(matches!(
            cli.command,
            Some(Command::Fleet {
                cmd: FleetCmd::Run { n: 2, .. }
            })
        ));
    }

    #[test]
    fn fleet_run_requires_workers_and_prompt() {
        let err = parse(&["fleet", "run", "-p", "x"]).expect_err("missing -n rejected");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
        let err = parse(&["fleet", "run", "-n", "2"]).expect_err("missing -p rejected");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn fleet_status_and_stop_parse() {
        let cli = parse(&["fleet", "status"]).expect("fleet status parses");
        assert!(matches!(
            cli.command,
            Some(Command::Fleet {
                cmd: FleetCmd::Status
            })
        ));
        let cli = parse(&["fleet", "stop"]).expect("fleet stop parses");
        assert!(matches!(
            cli.command,
            Some(Command::Fleet {
                cmd: FleetCmd::Stop
            })
        ));
    }

    #[test]
    fn bench_add_parses_with_defaults() {
        let cli = parse(&["bench", "add", "--id", "x", "--prompt", "y", "--check", "z"])
            .expect("bench add parses");
        let Some(Command::Bench {
            cmd:
                BenchCmd::Add {
                    id,
                    prompt,
                    check,
                    git_ref,
                    timeout,
                    check_timeout,
                    tag,
                    notes,
                },
        }) = cli.command
        else {
            panic!("expected bench add");
        };
        assert_eq!(id, "x");
        assert_eq!(prompt, "y");
        assert_eq!(check, "z");
        assert_eq!(git_ref, "HEAD");
        assert_eq!(timeout, 900);
        assert_eq!(check_timeout, 300);
        assert!(tag.is_empty());
        assert_eq!(notes, None);
    }

    #[test]
    fn bench_add_requires_id_prompt_and_check() {
        let err = parse(&["bench", "add", "--id", "x"]).expect_err("missing args rejected");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn bench_run_parses_repeatable_case_and_tag_filters() {
        let cli = parse(&[
            "bench", "run", "--runner", "true", "--case", "a", "--case", "b", "--tag", "rust",
        ])
        .expect("bench run parses");
        let Some(Command::Bench {
            cmd:
                BenchCmd::Run {
                    runner,
                    label,
                    case,
                    tag,
                    keep_worktrees,
                },
        }) = cli.command
        else {
            panic!("expected bench run");
        };
        assert_eq!(runner.as_deref(), Some("true"));
        assert_eq!(label, "run");
        assert_eq!(case, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(tag, vec!["rust".to_string()]);
        assert!(!keep_worktrees);
    }

    #[test]
    fn bench_list_and_remove_parse() {
        let cli = parse(&["bench", "list", "--tag", "smoke"]).expect("bench list parses");
        let Some(Command::Bench {
            cmd: BenchCmd::List { trajectories, tag },
        }) = cli.command
        else {
            panic!("expected bench list");
        };
        assert!(!trajectories);
        assert_eq!(tag, vec!["smoke".to_string()]);

        let cli = parse(&["bench", "remove", "touch-case"]).expect("bench remove parses");
        let Some(Command::Bench {
            cmd: BenchCmd::Remove { id },
        }) = cli.command
        else {
            panic!("expected bench remove");
        };
        assert_eq!(id, "touch-case");
    }

    #[test]
    fn schedule_enable_and_disable_parse() {
        let cli = parse(&["schedule", "enable", "nightly"]).expect("schedule enable parses");
        let Some(Command::Schedule {
            cmd: ScheduleCmd::Enable { name },
        }) = cli.command
        else {
            panic!("expected schedule enable");
        };
        assert_eq!(name, "nightly");

        let cli = parse(&["schedule", "disable", "nightly"]).expect("schedule disable parses");
        let Some(Command::Schedule {
            cmd: ScheduleCmd::Disable { name },
        }) = cli.command
        else {
            panic!("expected schedule disable");
        };
        assert_eq!(name, "nightly");
    }

    #[test]
    fn usage_parses_with_optional_since() {
        let cli = parse(&["usage"]).expect("usage parses");
        assert!(matches!(cli.command, Some(Command::Usage { since: None })));

        let cli = parse(&["usage", "--since", "7d"]).expect("usage --since parses");
        let Some(Command::Usage { since }) = cli.command else {
            panic!("expected usage");
        };
        assert_eq!(since.as_deref(), Some("7d"));
    }

    #[test]
    fn evolve_list_and_undo_parse() {
        let cli = parse(&["evolve", "list"]).expect("evolve list parses");
        assert!(matches!(
            cli.command,
            Some(Command::Evolve {
                cmd: EvolveCmd::List
            })
        ));

        let cli = parse(&["evolve", "undo", "2"]).expect("evolve undo parses");
        assert!(matches!(
            cli.command,
            Some(Command::Evolve {
                cmd: EvolveCmd::Undo { n: 2 }
            })
        ));
    }

    #[test]
    fn ignored_top_level_flags_names_everything_but_cwd() {
        let cli = parse(&["--cwd", "/tmp", "doctor"]).expect("parses");
        assert!(cli.ignored_top_level_flags().is_empty(), "--cwd is honored");

        let cli = parse(&["--plan", "--max-hours", "2", "fleet", "status"]).expect("parses");
        assert_eq!(cli.ignored_top_level_flags(), vec!["--plan", "--max-hours"]);

        let cli = parse(&["--output-format", "json", "bench", "list"]).expect("parses");
        assert_eq!(cli.ignored_top_level_flags(), vec!["--output-format"]);
    }

    #[test]
    fn bench_compare_parses_positional_paths() {
        let cli = parse(&["bench", "compare", "a.json", "b.json"]).expect("bench compare parses");
        let Some(Command::Bench {
            cmd: BenchCmd::Compare { a, b },
        }) = cli.command
        else {
            panic!("expected bench compare");
        };
        assert_eq!(a, PathBuf::from("a.json"));
        assert_eq!(b, PathBuf::from("b.json"));
    }
}
