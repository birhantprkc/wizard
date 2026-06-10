//! Command-line argument parsing (`clap` derive).
//!
//! Flags per `docs/architecture.md` (CLI section) and `docs/modes.md`.

use std::path::PathBuf;

use clap::Parser;

use crate::config::Mode;

/// Wizard — your sovereign agent. Self-extending. Fully local.
#[derive(Debug, Clone, Parser)]
#[command(name = "wizard", version, about, long_about = None)]
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

    /// Force auto-approve for this run (already the default; useful when
    /// config has `auto_approve = false` to restore bypass behavior).
    #[arg(long)]
    pub auto: bool,

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

    #[command(subcommand)]
    pub command: Option<Command>,
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

        /// Keep the per-case worktrees for inspection instead of removing them.
        #[arg(long)]
        keep_worktrees: bool,
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
        assert!(!cli.auto);
        assert_eq!(cli.max_hours, None);
        assert_eq!(cli.loop_limit, None);
        assert!(!cli.continuous);
        assert_eq!(cli.cwd, None);
        assert!(!cli.resume);
        assert!(!cli.onboard);
        assert!(!cli.gateway);
        assert!(cli.command.is_none(), "bare wizard has no subcommand");
    }

    #[test]
    fn parses_all_documented_flags() {
        let cli = parse(&[
            "--mode",
            "sovereign",
            "-p",
            "add tests",
            "--auto",
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
        assert!(cli.auto);
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
    fn rejects_unknown_mode() {
        let err = parse(&["--mode", "warlock"]).expect_err("unknown mode must be rejected");
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);
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
    fn bench_run_parses_repeatable_case_filter() {
        let cli = parse(&[
            "bench", "run", "--runner", "true", "--case", "a", "--case", "b",
        ])
        .expect("bench run parses");
        let Some(Command::Bench {
            cmd:
                BenchCmd::Run {
                    runner,
                    label,
                    case,
                    keep_worktrees,
                },
        }) = cli.command
        else {
            panic!("expected bench run");
        };
        assert_eq!(runner.as_deref(), Some("true"));
        assert_eq!(label, "run");
        assert_eq!(case, vec!["a".to_string(), "b".to_string()]);
        assert!(!keep_worktrees);
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
