//! Quality gates: the commands a sovereign run must pass before it is allowed
//! to call itself finished.
//!
//! Sovereign mode already has brakes. The circuit breakers stop a run that is
//! failing the same way over and over, `--max-hours` and `max_steps` stop one
//! that is taking too long, and `max_consecutive_failures` stops a perpetual
//! run whose setup is broken. Every one of those answers the question "should
//! this run keep going?". None of them answers "was the work any good?". A
//! model that writes a plausible patch, never runs the suite, and says "done"
//! ends the run with [`DoneReason::Completed`] and exit code 0, which is
//! exactly the shape of a successful run to whatever script is reading it.
//!
//! A gate is the missing answer: a command line that has to exit zero before a
//! run is allowed to finish. The model does not run it and cannot see it
//! coming; the loop runs it after the model says it is done, and a failing gate
//! is fed back as another turn rather than accepted.
//!
//! # Why a failed gate is not re-run on an unchanged workspace
//!
//! The naive loop is: run the gate, feed the failure back, run it again. That
//! is fine while the model is fixing things and catastrophic when it is not. A
//! model that cannot make `cargo test` pass will keep saying "fixed it" without
//! touching a file, and each of those turns costs a full test-suite run plus
//! the tokens to describe it. A four-hour budget disappears into re-running one
//! failing command with identical inputs.
//!
//! So every failure is recorded against a fingerprint of the workspace at the
//! moment it happened, and a gate whose fingerprint has not moved is not run
//! again: the model is told, in the prompt, that nothing changed and that the
//! result would be identical. See [`workspace_fingerprint`] for what "changed"
//! means and why it is defined the way it is.
//!
//! Only *failures* are cached this way. A passing gate is always re-run, even
//! on an unchanged workspace, because the two mistakes are not symmetric: a
//! stale skip of a failure costs one wasted turn and still reports failure,
//! while a stale skip of a pass would let a run declare success without
//! evidence, which is the entire thing gates exist to prevent.

use std::collections::HashMap;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

use crate::agent::{AgentEvent, DoneReason};
use crate::cli::Cli;
use crate::config::{Config, Mode};
use crate::tools::truncate_output;

/// Process exit code for a run that ended with a gate still failing.
///
/// Its own code rather than the [`DoneReason`]'s, because the two questions a
/// scripted caller asks are different. "Why did the run stop?" is answered by
/// [`crate::output::exit_code`] (2 = step budget, 3 = breaker, 4 = time limit).
/// "Is the work verified?" has exactly one bad answer and it must not be
/// confusable with a run that merely ran long: `wizard --gate 'cargo test' -p
/// '…' && deploy` has to refuse to deploy on a failing gate whether the run
/// ended on its deadline, on its loop bound, or on the model giving up.
pub const EXIT_GATES_FAILED: i32 = 5;

/// Bytes of a failing gate's output quoted back to the model.
///
/// A failing suite prints megabytes: every passing test, every warning, a
/// backtrace per failure. The prompt this lands in is prepended to a
/// conversation that already has a context budget, and the part that names the
/// failure is at the end, which is why [`truncate_output`] keeps a small head
/// and a large tail rather than a prefix.
const MAX_GATE_OUTPUT_BYTES: usize = 6_000;

/// Files stat'ed before the non-git fingerprint gives up.
///
/// A tree this large without a git repo in it is a tree where the fingerprint
/// would cost more than the gate it is protecting, so the walk stops and
/// reports "unknown" instead, which the caller reads as "assume it changed".
const MAX_WALK_ENTRIES: usize = 20_000;

/// Directories the non-git fingerprint never descends into.
///
/// Every one of these is a build or cache directory, and skipping them is
/// load-bearing rather than an optimization: see [`workspace_fingerprint`].
const WALK_SKIP: &[&str] = &[
    ".git",
    ".wizard",
    "target",
    "node_modules",
    "dist",
    "build",
    ".venv",
    "venv",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    ".next",
    ".cargo",
];

/// A project-local gate list, read from `<project>/.wizard/gates.toml`.
///
/// Gates belong to the project, not to the machine: "this repo is not done
/// until `cargo clippy -D warnings` passes" is a fact about the repo, and a
/// user who has to remember it on the command line will forget it. The global
/// `gates` key in `~/.wizard/config.toml` covers the other case (a habit the
/// user wants everywhere), and the two are merged.
#[derive(Debug, Default, serde::Deserialize)]
struct ProjectGates {
    #[serde(default)]
    gates: Vec<String>,
}

/// What one gate did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateOutcome {
    /// The command line, as configured.
    pub command: String,
    /// Exit code zero, within its timeout.
    pub passed: bool,
    /// Exit code, or `None` when the command was killed (timeout or signal).
    pub code: Option<i32>,
    /// Set when the gate was killed at its timeout.
    pub timed_out: bool,
    /// Combined output, already bounded to [`MAX_GATE_OUTPUT_BYTES`].
    pub output: String,
    /// False when this outcome was replayed from a previous failure because
    /// the workspace has not changed since (see the module docs).
    pub rerun: bool,
}

/// The gate state of a run, as of the last check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateVerdict {
    /// No gate has been checked yet: the run never reached a point where it
    /// claimed to be finished, so nothing was verified either way.
    Unverified,
    /// Every gate exited zero at the last check.
    Passed,
    /// A gate was failing at the last check.
    Failing(GateOutcome),
}

/// What the run loop should do after a gate check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateDecision {
    /// Every gate passed; the run may finish (or, in continuous mode, move on).
    Finish,
    /// A gate failed and there is budget for another attempt: take one more
    /// turn with this prompt.
    Retry(String),
    /// A gate failed and there is no budget left. The run ends unverified.
    GiveUp,
}

/// The gate set of one run, plus the memory that keeps it from re-running a
/// failure against an unchanged workspace.
pub struct Gates {
    /// Gate command lines, in the order they run.
    commands: Vec<String>,
    /// Where gates run, and what the fingerprint covers.
    root: PathBuf,
    /// Per-gate wall clock, also clamped to the run's own deadline.
    timeout_secs: u64,
    /// Consecutive failed checks tolerated before the run gives up. `0` is
    /// unlimited, bounded then only by `--max-hours` and `.wizard/loop-control`.
    max_attempts: u32,
    /// Consecutive failed checks so far. Cleared by a check that passes, for
    /// the same reason `max_consecutive_failures` counts consecutively: a run
    /// that got the gates green once and broke them again later is making
    /// progress, not stuck.
    attempts: u32,
    /// Workspace fingerprint at the moment each gate last failed, with the
    /// outcome to replay.
    last_failure: HashMap<String, (String, GateOutcome)>,
    /// State of the last check, which decides the process exit code.
    verdict: GateVerdict,
}

impl Gates {
    /// The gate set for this run, or `None` when there is nothing to check.
    ///
    /// `None` rather than an empty set so the caller's hot path is a single
    /// `Option` test: a run without gates must cost nothing, not one
    /// fingerprint of the tree per finished turn.
    pub fn for_run(config: &Config, cli: &Cli, project_root: &Path) -> Option<Self> {
        let commands = gate_commands(config, cli, project_root);
        if commands.is_empty() {
            return None;
        }
        Some(Self {
            commands,
            root: project_root.to_path_buf(),
            timeout_secs: config.gate_timeout_secs,
            max_attempts: config.gate_max_attempts,
            attempts: 0,
            last_failure: HashMap::new(),
            verdict: GateVerdict::Unverified,
        })
    }

    /// Gate command lines, in run order.
    pub fn commands(&self) -> &[String] {
        &self.commands
    }

    /// State of the last check.
    pub fn verdict(&self) -> &GateVerdict {
        &self.verdict
    }

    /// Run the gates and say what the loop should do about the result.
    ///
    /// Gates run in order and the check stops at the first failure. The order
    /// is the author's, and an author puts the cheap gate first: re-running a
    /// twenty-minute suite because the formatter is unhappy spends the run's
    /// wall clock on an answer that was already known.
    pub async fn check(
        &mut self,
        deadline: Option<Instant>,
        events: &mpsc::Sender<AgentEvent>,
    ) -> GateDecision {
        let fingerprint = workspace_fingerprint(&self.root).await;
        let total = self.commands.len();
        for (index, command) in self.commands.clone().into_iter().enumerate() {
            let position = format!("gate {}/{total}", index + 1);

            // A failure this workspace has already produced. Not re-run: see
            // the module docs.
            if let Some(fingerprint) = fingerprint.as_deref()
                && let Some((failed_at, outcome)) = self.last_failure.get(&command)
                && failed_at == fingerprint
            {
                let mut outcome = outcome.clone();
                outcome.rerun = false;
                notify(
                    events,
                    format!(
                        "{position} `{command}` still failing, not re-run: the workspace has not \
                         changed since it failed"
                    ),
                )
                .await;
                return self.fail(outcome);
            }

            let Some(budget) = self.budget(deadline) else {
                // The run's wall clock ran out before the gate could start.
                // Nothing was verified, and reaching a limit is not success.
                notify(
                    events,
                    format!("{position} `{command}` not run: the run's time limit has passed"),
                )
                .await;
                return GateDecision::GiveUp;
            };
            notify(events, format!("{position}: {command}")).await;
            let outcome = self.run_one(&command, budget).await;
            if outcome.passed {
                notify(events, format!("{position} passed: {command}")).await;
                // A gate that passes forgets its old failure, so the next
                // failure is compared against the workspace that produced it
                // rather than an older one.
                self.last_failure.remove(&command);
                continue;
            }
            notify(
                events,
                format!("{position} FAILED ({}): {command}", exit_note(&outcome)),
            )
            .await;
            if let Some(fingerprint) = fingerprint.clone() {
                self.last_failure
                    .insert(command.clone(), (fingerprint, outcome.clone()));
            }
            return self.fail(outcome);
        }
        self.attempts = 0;
        self.verdict = GateVerdict::Passed;
        notify(
            events,
            format!("all {total} gate(s) passed; the run may finish"),
        )
        .await;
        GateDecision::Finish
    }

    /// Record a failing check and decide whether another turn is affordable.
    fn fail(&mut self, outcome: GateOutcome) -> GateDecision {
        self.attempts = self.attempts.saturating_add(1);
        let prompt = failure_prompt(&outcome);
        self.verdict = GateVerdict::Failing(outcome);
        if self.max_attempts != 0 && self.attempts >= self.max_attempts {
            GateDecision::GiveUp
        } else {
            GateDecision::Retry(prompt)
        }
    }

    /// Run one gate through the platform shell in the project root.
    async fn run_one(&self, command: &str, budget: Duration) -> GateOutcome {
        let mut process = crate::platform::shell::tokio_command(command);
        process.current_dir(&self.root);
        match crate::tools::shell::run_command("gate", process, budget).await {
            Ok(result) => {
                let timed_out = result.timed_out.is_some();
                GateOutcome {
                    command: command.to_string(),
                    passed: !timed_out && result.code == Some(0),
                    code: result.code,
                    timed_out,
                    output: combined_output(&result.stdout, &result.stderr),
                    rerun: true,
                }
            }
            // A gate that cannot be spawned is a failing gate, not a crashed
            // run: the command line is the user's and a typo in it must be
            // reported, not thrown.
            Err(err) => GateOutcome {
                command: command.to_string(),
                passed: false,
                code: None,
                timed_out: false,
                output: format!("the gate command could not be started: {err}"),
                rerun: true,
            },
        }
    }

    /// How long one gate may run: its configured budget, never past the run's
    /// own deadline. `None` when the deadline has already passed, which means
    /// the gate must not start at all.
    fn budget(&self, deadline: Option<Instant>) -> Option<Duration> {
        let configured = Duration::from_secs(self.timeout_secs.max(1));
        match deadline {
            Some(deadline) => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    None
                } else {
                    Some(configured.min(left))
                }
            }
            None => Some(configured),
        }
    }

    /// The end-of-run line, or `None` when there is nothing worth saying.
    ///
    /// Only the failing case is loud. A run whose gates passed already says so
    /// through its exit code and its ordinary summary, and a line repeating it
    /// is the kind of padding this project's output does not have.
    pub fn summary(&self, reason: DoneReason) -> Option<String> {
        match &self.verdict {
            GateVerdict::Passed => None,
            GateVerdict::Unverified => Some(format!(
                "gates were never checked: the run ended ({}) before it claimed to be finished",
                crate::output::reason_str(reason)
            )),
            GateVerdict::Failing(outcome) => Some(format!(
                "GATE FAILING at the end of the run ({}): `{}` {} (exit {EXIT_GATES_FAILED})",
                crate::output::reason_str(reason),
                outcome.command,
                exit_note(outcome),
            )),
        }
    }
}

/// The process exit code for a finished run, gates included.
///
/// A failing gate outranks the reason the loop stopped. Everything else falls
/// through to [`crate::output::exit_code`], including [`GateVerdict::Unverified`]:
/// a run stopped by its operator before it ever claimed to be done has not
/// failed its gates, it just never reached them, and reporting that as a gate
/// failure would make the code mean two different things.
pub fn exit_code(reason: DoneReason, gates: Option<&Gates>) -> i32 {
    match gates.map(Gates::verdict) {
        Some(GateVerdict::Failing(_)) => EXIT_GATES_FAILED,
        _ => crate::output::exit_code(reason),
    }
}

/// The gate command lines for this run: config, then the project's own, then
/// `--gate` flags, in that order, with duplicates and blanks dropped.
///
/// Genie mode gets none of them, whatever is configured. A gate is a
/// substitute for a human looking at the result, and in genie mode there is a
/// human looking at the result: they can see the tests run, they can see the
/// claim, and they will say so. Running the suite behind their back at the end
/// of every turn would be a slow surprise, not a safeguard.
pub fn gate_commands(config: &Config, cli: &Cli, project_root: &Path) -> Vec<String> {
    if config.mode != Mode::Sovereign {
        return Vec::new();
    }
    let project = project_gates(project_root);
    let mut commands: Vec<String> = Vec::new();
    for command in config.gates.iter().chain(&project).chain(&cli.gate) {
        let command = command.trim();
        if command.is_empty() || commands.iter().any(|seen| seen == command) {
            continue;
        }
        commands.push(command.to_string());
    }
    commands
}

/// Gates declared by the project in `<project>/.wizard/gates.toml`. A missing
/// or unparseable file is an empty list with a log line: a broken gate file
/// must not stop a run from starting, and a run that silently loses its gates
/// is why the log line exists.
fn project_gates(project_root: &Path) -> Vec<String> {
    let path = project_root.join(".wizard").join("gates.toml");
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    match toml::from_str::<ProjectGates>(&raw) {
        Ok(parsed) => parsed.gates,
        Err(err) => {
            tracing::warn!("ignoring {}: {err}", path.display());
            Vec::new()
        }
    }
}

/// `exit 101` / `timed out after 900s` / `killed by a signal`, for one line of
/// output.
fn exit_note(outcome: &GateOutcome) -> String {
    if outcome.timed_out {
        return "timed out".to_string();
    }
    match outcome.code {
        Some(code) => format!("exit {code}"),
        None => "killed by a signal".to_string(),
    }
}

/// One bounded block carrying both of a gate's streams. Labelled even when one
/// is empty, because "stderr was empty" is information when a build tool is
/// supposed to have complained on it.
fn combined_output(stdout: &str, stderr: &str) -> String {
    let combined = format!(
        "stdout:\n{}\n\nstderr:\n{}",
        stdout.trim_end(),
        stderr.trim_end()
    );
    truncate_output(combined, MAX_GATE_OUTPUT_BYTES)
}

/// The turn a failing gate opens.
///
/// Two shapes, and the difference matters. A gate that just ran gets its
/// output quoted back. A gate that was *not* run, because the workspace has
/// not changed since it failed, gets told exactly that, because otherwise a model
/// that has run out of ideas reads the identical failure text as a fresh
/// result and tries the identical non-fix again.
fn failure_prompt(outcome: &GateOutcome) -> String {
    let head = if outcome.rerun {
        format!(
            "A quality gate failed. This run is not finished until every gate exits zero.\n\n\
             gate: `{}`\nresult: {}\n\n{}",
            outcome.command,
            exit_note(outcome),
            outcome.output
        )
    } else {
        format!(
            "A quality gate is still failing, and it was NOT re-run: nothing in the workspace has \
             changed since it failed (no edited file, no new file, no commit), so the result \
             would be identical.\n\n\
             gate: `{}`\nresult: {}\n\n{}",
            outcome.command,
            exit_note(outcome),
            outcome.output
        )
    };
    format!(
        "{head}\n\n\
         Fix the cause. Do not edit, weaken, or work around the gate command itself, and do not \
         claim the task is done until the gate passes. If you genuinely cannot make it pass, say \
         plainly what blocks you and what you tried. The run will end reporting the gate as \
         failing, which is the honest outcome."
    )
}

/// Send a gate notice to whichever sink the run is using. Best-effort: a full
/// or closed channel costs a line of commentary, never the check.
async fn notify(events: &mpsc::Sender<AgentEvent>, message: String) {
    let _ = events
        .send(AgentEvent::Notice(format!("gate: {message}")))
        .await;
}

/// A hash of everything about the workspace a gate could plausibly depend on,
/// or `None` when it cannot be determined (which callers must read as "assume
/// it changed").
///
/// # What counts as a change
///
/// In a git repo: `HEAD`, the porcelain status, and the size plus modification
/// time of every tracked and every untracked-but-not-ignored file. That is
/// three cheap `git` calls and one `stat` per source file.
///
/// Outside a repo: the same size-and-mtime stamp over a bounded walk of the
/// tree, skipping [`WALK_SKIP`].
///
/// # Why ignored files are excluded, which is the whole trick
///
/// The obvious definition, "did anything under the project root change?",
/// does not work, and fails in the exact direction that makes the feature
/// useless. A gate of `cargo test` writes into `target/` every time it runs.
/// If build output counted, then running the gate would itself change the
/// workspace, every re-check would see a "changed" tree, and the loop would
/// re-run the failing suite until the budget was gone. Same story for
/// `node_modules/.cache`, `__pycache__`, and `.pytest_cache`.
///
/// So the fingerprint covers *source*: what git tracks, plus what git would
/// track if you added it. `.gitignore` is already the project's own statement
/// about which files are inputs and which are byproducts, and reusing it means
/// a project that adds a new build directory does not have to teach Wizard
/// about it too.
///
/// # Where it is imprecise, and in which direction
///
/// Size and mtime, not content: an edit that changes neither (a same-length
/// rewrite inside one timestamp tick) is invisible. The consequence is a gate
/// that is reported as still failing instead of being re-run, which costs one
/// turn and never produces a false pass: only failures are cached against a
/// fingerprint, so an imprecise fingerprint can never let a run claim success
/// it did not earn.
pub(crate) async fn workspace_fingerprint(root: &Path) -> Option<String> {
    match git_fingerprint(root).await {
        Some(fingerprint) => Some(fingerprint),
        None => walk_fingerprint(root),
    }
}

/// Run `git` in `root` and return its stdout, or `None` on any failure,
/// including "not a repository", which is the common case this has to survive.
async fn git(root: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .await
        .ok()?;
    output.status.success().then_some(output.stdout)
}

/// [`workspace_fingerprint`] for a git repo.
async fn git_fingerprint(root: &Path) -> Option<String> {
    let status = git(root, &["status", "--porcelain", "-z"]).await?;
    let tracked = git(root, &["ls-files", "-z"]).await?;
    let untracked = git(root, &["ls-files", "-z", "--others", "--exclude-standard"])
        .await
        .unwrap_or_default();
    // A repo with no commit yet has no HEAD, and that is a workspace worth
    // fingerprinting like any other.
    let head = git(root, &["rev-parse", "HEAD"]).await.unwrap_or_default();

    let mut hasher = Sha256::new();
    hasher.update(b"git\0");
    hasher.update(&head);
    hasher.update([0]);
    hasher.update(&status);
    hasher.update([0]);
    for path in split_nul(&tracked).chain(split_nul(&untracked)) {
        let absolute = root.join(std::ffi::OsStr::from_bytes(path));
        stamp(&mut hasher, &absolute, path);
    }
    Some(format!("{:x}", hasher.finalize()))
}

/// NUL-separated paths as git's `-z` output writes them.
fn split_nul(raw: &[u8]) -> impl Iterator<Item = &[u8]> {
    raw.split(|byte| *byte == 0).filter(|path| !path.is_empty())
}

/// [`workspace_fingerprint`] for a directory that is not a git repo: a
/// bounded, deterministic walk. `None` once the tree is bigger than
/// [`MAX_WALK_ENTRIES`], which the caller reads as "assume it changed".
fn walk_fingerprint(root: &Path) -> Option<String> {
    let mut hasher = Sha256::new();
    hasher.update(b"walk\0");
    let mut budget = MAX_WALK_ENTRIES;
    walk(root, root, &mut hasher, &mut budget)?;
    Some(format!("{:x}", hasher.finalize()))
}

/// One directory of [`walk_fingerprint`]. Entries are sorted so the hash does
/// not depend on `readdir` order, which is not stable across filesystems and
/// would make every fingerprint differ from the last.
fn walk(root: &Path, dir: &Path, hasher: &mut Sha256, budget: &mut usize) -> Option<()> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .collect();
    entries.sort();
    for path in entries {
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        if WALK_SKIP.contains(&name.as_str()) {
            continue;
        }
        if *budget == 0 {
            return None;
        }
        *budget -= 1;
        let relative = path.strip_prefix(root).unwrap_or(&path).as_os_str();
        if path.is_dir() {
            hasher.update(b"dir\0");
            hasher.update(relative.as_bytes());
            walk(root, &path, hasher, budget)?;
        } else {
            stamp(hasher, &path, relative.as_bytes());
        }
    }
    Some(())
}

/// Fold one file's identity into the hash: its path, and its size and
/// modification time, or a marker when it is not there. `symlink_metadata` so
/// a symlink is stamped as itself rather than as whatever it points at, which
/// may be outside the workspace entirely.
fn stamp(hasher: &mut Sha256, path: &Path, relative: &[u8]) {
    hasher.update(relative);
    hasher.update([0]);
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            hasher.update(meta.len().to_le_bytes());
            let nanos = meta
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |since| since.as_nanos());
            hasher.update(nanos.to_le_bytes());
        }
        Err(_) => hasher.update(b"absent"),
    }
    hasher.update([0]);
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    fn cli(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("wizard").chain(args.iter().copied()))
            .expect("valid args")
    }

    fn sovereign() -> Config {
        Config {
            mode: Mode::Sovereign,
            ..Config::default()
        }
    }

    /// A `Gates` over `root` with everything but the command lines defaulted.
    fn runner(root: &Path, commands: &[&str]) -> Gates {
        Gates::for_run(
            &sovereign(),
            &cli(&commands
                .iter()
                .flat_map(|command| ["--gate", command])
                .collect::<Vec<_>>()),
            root,
        )
        .expect("gates were configured")
    }

    /// A channel whose receiver is kept alive, so `notify` never fails and the
    /// notices can be read back.
    fn channel() -> (mpsc::Sender<AgentEvent>, mpsc::Receiver<AgentEvent>) {
        mpsc::channel(64)
    }

    fn notices(rx: &mut mpsc::Receiver<AgentEvent>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(AgentEvent::Notice(message)) = rx.try_recv() {
            out.push(message);
        }
        out
    }

    fn git(repo: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap_or_else(|err| panic!("git {args:?}: {err}"));
        assert!(status.status.success(), "git {args:?} failed");
    }

    /// An initialized repo with one committed file, which is the shape a
    /// sovereign run works in.
    fn repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().expect("tempdir");
        git(tmp.path(), &["init", "-q", "-b", "main"]);
        git(tmp.path(), &["config", "user.email", "t@example.com"]);
        git(tmp.path(), &["config", "user.name", "Test"]);
        std::fs::write(tmp.path().join("src.txt"), "one\n").expect("write");
        git(tmp.path(), &["add", "-A"]);
        git(tmp.path(), &["commit", "-qm", "init"]);
        tmp
    }

    #[tokio::test]
    async fn a_passing_gate_lets_the_run_finish() {
        let project = repo();
        let mut gates = runner(project.path(), &["true", "test -f src.txt"]);
        let (tx, mut rx) = channel();
        assert_eq!(gates.check(None, &tx).await, GateDecision::Finish);
        assert_eq!(gates.verdict(), &GateVerdict::Passed);
        assert_eq!(exit_code(DoneReason::Completed, Some(&gates)), 0);
        assert!(gates.summary(DoneReason::Completed).is_none());
        assert!(
            notices(&mut rx)
                .iter()
                .any(|line| line.contains("2 gate(s) passed")),
            "the run says what it verified"
        );
    }

    #[tokio::test]
    async fn a_failing_gate_produces_another_turn() {
        let project = repo();
        let mut gates = runner(
            project.path(),
            &["true", "echo boom >&2; echo done; exit 3", "true"],
        );
        let (tx, mut rx) = channel();
        let GateDecision::Retry(prompt) = gates.check(None, &tx).await else {
            panic!("a failing gate with attempts left is another turn, not the end");
        };
        assert!(prompt.contains("A quality gate failed"), "{prompt}");
        assert!(prompt.contains("exit 3"), "{prompt}");
        assert!(prompt.contains("boom"), "the failure output is quoted back");
        assert!(
            prompt.contains("do not claim the task is done"),
            "the model is told what finishing now means"
        );
        assert!(matches!(gates.verdict(), GateVerdict::Failing(_)));
        // A failing gate is not success, whatever the loop's own reason says.
        assert_eq!(
            exit_code(DoneReason::Completed, Some(&gates)),
            EXIT_GATES_FAILED
        );
        let notices = notices(&mut rx);
        assert!(
            notices.iter().any(|line| line.contains("FAILED (exit 3)")),
            "{notices:?}"
        );
        assert!(
            !notices.iter().any(|line| line.contains("gate 3/3")),
            "the gates after a failure are not run: {notices:?}"
        );
    }

    #[tokio::test]
    async fn an_unchanged_workspace_does_not_rerun_a_failed_gate() {
        let project = repo();
        // The tally lives outside the workspace on purpose: a gate that wrote
        // into the project would change the fingerprint it is being measured
        // against, which is the bug this test would then hide.
        let tally = tempfile::tempdir().expect("tempdir");
        let tally = tally.path().join("runs");
        let command = format!("echo run >> {}; exit 1", tally.display());
        // Unlimited attempts: this test is about what the loop re-runs, not
        // about where it gives up.
        let config = Config {
            gate_max_attempts: 0,
            ..sovereign()
        };
        let mut gates =
            Gates::for_run(&config, &cli(&["--gate", command.as_str()]), project.path())
                .expect("a gate was configured");
        let (tx, mut rx) = channel();

        assert!(matches!(
            gates.check(None, &tx).await,
            GateDecision::Retry(_)
        ));
        assert_eq!(
            std::fs::read_to_string(&tally)
                .expect("ran once")
                .lines()
                .count(),
            1
        );

        // The model changed nothing. Re-running would produce the identical
        // failure and cost the identical minutes.
        let _ = notices(&mut rx);
        let GateDecision::Retry(prompt) = gates.check(None, &tx).await else {
            panic!("still failing, still has attempts");
        };
        assert_eq!(
            std::fs::read_to_string(&tally)
                .expect("still one run")
                .lines()
                .count(),
            1,
            "an unchanged workspace must not pay for the gate a second time"
        );
        assert!(prompt.contains("NOT re-run"), "{prompt}");
        assert!(
            prompt.contains("nothing in the workspace has changed"),
            "{prompt}"
        );
        assert!(
            notices(&mut rx)
                .iter()
                .any(|line| line.contains("not re-run")),
            "the operator is told why nothing happened"
        );

        // An edit to a tracked file is a change, so the gate runs again.
        std::fs::write(project.path().join("src.txt"), "two\n").expect("write");
        assert!(matches!(
            gates.check(None, &tx).await,
            GateDecision::Retry(_)
        ));
        assert_eq!(
            std::fs::read_to_string(&tally)
                .expect("ran twice")
                .lines()
                .count(),
            2,
            "a changed workspace deserves a fresh answer"
        );
    }

    #[tokio::test]
    async fn hitting_a_limit_with_a_failing_gate_reports_failure_and_a_non_zero_exit() {
        let project = repo();
        let config = Config {
            gate_max_attempts: 2,
            ..sovereign()
        };
        let mut gates = Gates::for_run(&config, &cli(&["--gate", "false"]), project.path())
            .expect("a gate was configured");
        let (tx, _rx) = channel();

        assert!(matches!(
            gates.check(None, &tx).await,
            GateDecision::Retry(_)
        ));
        assert_eq!(
            gates.check(None, &tx).await,
            GateDecision::GiveUp,
            "the second failed check spends the last attempt"
        );
        assert_eq!(
            exit_code(DoneReason::Completed, Some(&gates)),
            EXIT_GATES_FAILED,
            "a run that gave up on its gates must not exit 0"
        );
        let summary = gates
            .summary(DoneReason::Completed)
            .expect("a failing run says so");
        assert!(summary.contains("GATE FAILING"), "{summary}");
        assert!(summary.contains("`false`"), "{summary}");

        // The wall clock is the other limit, and it is not success either.
        let mut gates = runner(project.path(), &["false"]);
        let past = Instant::now() - Duration::from_secs(1);
        assert_eq!(
            gates.check(Some(past), &tx).await,
            GateDecision::GiveUp,
            "a gate must not start after the run's deadline"
        );
        assert_eq!(
            exit_code(DoneReason::TimeLimit, Some(&gates)),
            crate::output::exit_code(DoneReason::TimeLimit),
            "nothing was verified, so the time limit is still the reason"
        );
    }

    #[test]
    fn genie_mode_ignores_gates_entirely() {
        let project = repo();
        std::fs::create_dir_all(project.path().join(".wizard")).expect("mkdir");
        std::fs::write(
            project.path().join(".wizard").join("gates.toml"),
            "gates = [\"cargo test\"]\n",
        )
        .expect("write");
        let genie = Config {
            mode: Mode::Genie,
            gates: vec!["cargo fmt --check".to_string()],
            ..Config::default()
        };
        let flags = cli(&["--gate", "cargo clippy"]);
        assert!(
            gate_commands(&genie, &flags, project.path()).is_empty(),
            "a human is watching; the suite must not run behind their back"
        );
        assert!(Gates::for_run(&genie, &flags, project.path()).is_none());

        // The same three sources, in sovereign mode, all arrive.
        let sovereign = Config {
            mode: Mode::Sovereign,
            ..genie
        };
        assert_eq!(
            gate_commands(&sovereign, &flags, project.path()),
            vec!["cargo fmt --check", "cargo test", "cargo clippy"],
            "config, then the project's own, then the flags"
        );
    }

    #[test]
    fn a_repeated_gate_is_configured_once() {
        let project = repo();
        let config = Config {
            gates: vec!["cargo test".to_string(), "  ".to_string()],
            ..sovereign()
        };
        assert_eq!(
            gate_commands(
                &config,
                &cli(&["--gate", "cargo test", "--gate", " "]),
                project.path()
            ),
            vec!["cargo test"],
            "a gate named twice runs once, and a blank one is not a gate"
        );
    }

    #[tokio::test]
    async fn a_gate_that_hangs_is_killed_at_its_budget() {
        let project = repo();
        let config = Config {
            gate_timeout_secs: 1,
            ..sovereign()
        };
        let mut gates = Gates::for_run(&config, &cli(&["--gate", "sleep 30"]), project.path())
            .expect("a gate was configured");
        let (tx, _rx) = channel();
        let started = Instant::now();
        assert!(matches!(
            gates.check(None, &tx).await,
            GateDecision::Retry(_)
        ));
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "a hung gate must not outlive its budget"
        );
        let GateVerdict::Failing(outcome) = gates.verdict() else {
            panic!("a gate killed at its timeout has not passed");
        };
        assert!(outcome.timed_out);
    }

    #[tokio::test]
    async fn a_failing_gates_output_cannot_swamp_the_context() {
        let project = repo();
        let mut gates = runner(
            project.path(),
            &["head -c 4000000 /dev/zero | tr '\\0' 'x'; exit 1"],
        );
        let (tx, _rx) = channel();
        let GateDecision::Retry(prompt) = gates.check(None, &tx).await else {
            panic!("a failing gate is another turn");
        };
        assert!(
            prompt.len() < MAX_GATE_OUTPUT_BYTES + 2_000,
            "prompt was {} bytes",
            prompt.len()
        );
        assert!(prompt.contains("[output truncated]"), "{prompt}");
    }

    #[tokio::test]
    async fn the_fingerprint_moves_for_source_changes_and_not_for_build_output() {
        let project = repo();
        std::fs::write(project.path().join(".gitignore"), "target/\n").expect("write");
        git(project.path(), &["add", "-A"]);
        git(project.path(), &["commit", "-qm", "ignore"]);
        let before = workspace_fingerprint(project.path())
            .await
            .expect("a repo has a fingerprint");

        // What a gate leaves behind when it runs. If this counted, every
        // re-check would look like progress and the loop would re-run a
        // failing suite until the budget was gone.
        std::fs::create_dir_all(project.path().join("target/debug")).expect("mkdir");
        std::fs::write(project.path().join("target/debug/bin"), "elf").expect("write");
        assert_eq!(
            workspace_fingerprint(project.path()).await.as_deref(),
            Some(before.as_str()),
            "ignored build output is not a workspace change"
        );

        // A new source file is.
        std::fs::write(project.path().join("new.txt"), "hello\n").expect("write");
        let after = workspace_fingerprint(project.path())
            .await
            .expect("a repo has a fingerprint");
        assert_ne!(after, before, "an untracked source file is a change");

        // So is a commit, even though it leaves the working tree identical.
        git(project.path(), &["add", "-A"]);
        git(project.path(), &["commit", "-qm", "new"]);
        assert_ne!(
            workspace_fingerprint(project.path()).await.as_deref(),
            Some(after.as_str()),
            "a commit is a change to the workspace a gate may care about"
        );
    }

    #[tokio::test]
    async fn a_directory_without_git_still_has_a_fingerprint() {
        let plain = tempfile::tempdir().expect("tempdir");
        std::fs::write(plain.path().join("a.txt"), "one\n").expect("write");
        let before = workspace_fingerprint(plain.path())
            .await
            .expect("the walk covers a non-repo");
        std::fs::create_dir_all(plain.path().join("node_modules/pkg")).expect("mkdir");
        std::fs::write(plain.path().join("node_modules/pkg/index.js"), "x").expect("write");
        assert_eq!(
            workspace_fingerprint(plain.path()).await.as_deref(),
            Some(before.as_str()),
            "the skip list plays git's ignore rules outside a repo"
        );
        std::fs::write(plain.path().join("a.txt"), "two!\n").expect("write");
        assert_ne!(
            workspace_fingerprint(plain.path()).await.as_deref(),
            Some(before.as_str())
        );
    }

    #[tokio::test]
    async fn a_gate_that_cannot_be_started_fails_rather_than_ending_the_run() {
        let project = repo();
        let mut gates = runner(project.path(), &["definitely-not-a-real-command-xyz"]);
        let (tx, _rx) = channel();
        assert!(matches!(
            gates.check(None, &tx).await,
            GateDecision::Retry(_)
        ));
        assert!(matches!(gates.verdict(), GateVerdict::Failing(_)));
    }

    #[tokio::test]
    async fn a_gate_that_starts_passing_clears_the_streak() {
        let project = repo();
        let flag = project.path().join("fixed");
        let command = format!("test -f {}", flag.display());
        let config = Config {
            gate_max_attempts: 2,
            ..sovereign()
        };
        let mut gates =
            Gates::for_run(&config, &cli(&["--gate", command.as_str()]), project.path())
                .expect("a gate was configured");
        let (tx, _rx) = channel();

        assert!(matches!(
            gates.check(None, &tx).await,
            GateDecision::Retry(_)
        ));
        std::fs::write(&flag, "").expect("write");
        assert_eq!(gates.check(None, &tx).await, GateDecision::Finish);
        // One attempt was spent before; a run that got green once is not one
        // failure away from giving up.
        std::fs::remove_file(&flag).expect("remove");
        assert!(
            matches!(gates.check(None, &tx).await, GateDecision::Retry(_)),
            "the attempt counter is consecutive failures, not lifetime ones"
        );
    }
}
