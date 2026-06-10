//! `wizard bench` — trajectory recorder + replay benchmark runner.
//!
//! Records real agent tasks as they run ([`record`]), lets the user promote
//! them into benchmark cases, and replays cases against an arbitrary harness
//! command (this binary, another wizard build, or any other CLI agent) in
//! isolated git worktrees, scoring each case with a check command. Works
//! end-to-end without an LLM, and never loads `~/.wizard/config.toml`.
//!
//! Project-local layout (rooted at the current directory):
//! - `.wizard/trajectories.jsonl` — append-only trajectory records
//! - `.wizard/bench/cases/<id>.toml` — one case per file
//! - `.wizard/bench/results/<label>-<unix_ts>.json` — run results

pub mod git;
pub mod record;
pub mod runner;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::cli::BenchCmd;

/// One recorded headless agent turn (a line in `trajectories.jsonl`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrajectoryRecord {
    /// Random uuid v4; promote refers to records by id prefix.
    pub id: String,
    /// When the turn finished.
    pub timestamp: DateTime<Utc>,
    /// The exact input handed to the turn.
    pub prompt: String,
    /// Full sha of HEAD before the turn; `None` if not in a git repo.
    pub git_ref: Option<String>,
    /// Whether `git status --porcelain` was non-empty before the turn.
    pub dirty: bool,
    /// Debug format of the turn's `DoneReason`.
    pub done_reason: String,
    /// Wall-clock turn duration in seconds.
    pub duration_secs: f64,
    /// Model name in use during the turn.
    pub model: String,
    /// "sovereign" or "continuous".
    pub mode: String,
}

/// A replayable benchmark case (`bench/cases/<id>.toml`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchCase {
    /// File / worktree-safe identifier (`[a-zA-Z0-9_-]+`).
    pub id: String,
    /// Task prompt handed to the harness command.
    pub prompt: String,
    /// Full commit sha the worktree is created from.
    pub base_ref: String,
    /// Shell command run in the worktree after the harness; exit 0 = pass.
    pub check: String,
    /// Harness timeout in seconds.
    pub timeout_secs: u64,
    /// Check-command timeout in seconds.
    pub check_timeout_secs: u64,
    /// Free-form grouping tags.
    pub tags: Vec<String>,
    /// "manual" (bench add) or "recorded" (bench promote).
    pub source: String,
    /// When the case was created.
    pub created: DateTime<Utc>,
    /// Optional free-form notes.
    pub notes: Option<String>,
}

/// Outcome of replaying one case.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaseResult {
    /// Case id.
    pub id: String,
    /// "pass" | "fail" | "timeout" | "error".
    pub status: String,
    /// True only for status "pass".
    pub passed: bool,
    /// Harness exit code, when it ran to completion.
    pub harness_exit: Option<i32>,
    /// Check exit code, when it ran to completion.
    pub check_exit: Option<i32>,
    /// Harness wall-clock seconds.
    pub harness_secs: f64,
    /// Check wall-clock seconds.
    pub check_secs: f64,
    /// Populated for status "error" (worktree / ref / spawn failures).
    pub error: Option<String>,
}

/// A full `bench run` (`bench/results/<label>-<unix_ts>.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunResults {
    /// `--label` value.
    pub label: String,
    /// The resolved runner template actually used.
    pub runner: String,
    /// When the run finished.
    pub created: DateTime<Utc>,
    /// Per-case outcomes.
    pub cases: Vec<CaseResult>,
    /// Number of cases run.
    pub total: usize,
    /// Number of cases with status "pass".
    pub passed: usize,
    /// `passed / total`, 0.0..=1.0.
    pub pass_rate: f64,
}

/// Dispatch a `wizard bench` subcommand.
pub async fn run(cmd: BenchCmd) -> Result<()> {
    match cmd {
        BenchCmd::Add {
            id,
            prompt,
            check,
            git_ref,
            timeout,
            check_timeout,
            tag,
            notes,
        } => {
            add(
                id,
                prompt,
                check,
                git_ref,
                timeout,
                check_timeout,
                tag,
                notes,
            )
            .await
        }
        BenchCmd::List { trajectories } => list(trajectories),
        BenchCmd::Promote {
            trajectory,
            check,
            id,
            timeout,
            check_timeout,
            tag,
            notes,
        } => promote(trajectory, check, id, timeout, check_timeout, tag, notes),
        BenchCmd::Run {
            runner,
            label,
            case,
            keep_worktrees,
        } => runner::run_cases(runner, label, case, keep_worktrees).await,
        BenchCmd::Compare { a, b } => compare(&a, &b),
    }
}

/// `.wizard/bench/cases` under `root`.
pub(crate) fn cases_dir(root: &Path) -> PathBuf {
    root.join(".wizard").join("bench").join("cases")
}

/// `.wizard/bench/results` under `root`.
pub(crate) fn results_dir(root: &Path) -> PathBuf {
    root.join(".wizard").join("bench").join("results")
}

/// `.wizard/trajectories.jsonl` under `root`.
pub(crate) fn trajectories_path(root: &Path) -> PathBuf {
    root.join(".wizard").join("trajectories.jsonl")
}

/// Case ids become file names and worktree directory names, so restrict them
/// to a path-safe alphabet.
pub(crate) fn validate_case_id(id: &str) -> Result<()> {
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        bail!(
            "invalid case id {id:?}: only [a-zA-Z0-9_-] is allowed \
             (case ids become file and worktree names)"
        );
    }
    Ok(())
}

/// Load all cases under `root`, sorted by id. A missing dir is an empty set.
pub(crate) fn load_cases(root: &Path) -> Result<Vec<BenchCase>> {
    let dir = cases_dir(root);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).context(format!("reading {}", dir.display())),
    };
    let mut cases = Vec::new();
    for entry in entries {
        let path = entry.context("reading case dir entry")?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("toml") {
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading case {}", path.display()))?;
        let case: BenchCase =
            toml::from_str(&text).with_context(|| format!("parsing case {}", path.display()))?;
        cases.push(case);
    }
    cases.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(cases)
}

/// Write a new case file; refuses to overwrite an existing id.
fn write_case(root: &Path, case: &BenchCase) -> Result<PathBuf> {
    validate_case_id(&case.id)?;
    let dir = cases_dir(root);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(format!("{}.toml", case.id));
    if path.exists() {
        bail!("case '{}' already exists at {}", case.id, path.display());
    }
    let text = toml::to_string_pretty(case).context("serializing case")?;
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Read all trajectory records under `root`. Malformed lines are skipped
/// (with a warning) so one corrupt write can't brick the whole log.
pub(crate) fn read_trajectories(root: &Path) -> Result<Vec<TrajectoryRecord>> {
    let path = trajectories_path(root);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).context(format!("reading {}", path.display())),
    };
    let mut records = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<TrajectoryRecord>(line) {
            Ok(record) => records.push(record),
            Err(err) => tracing::warn!("skipping malformed trajectory line: {err}"),
        }
    }
    Ok(records)
}

/// Aggregate per-case results into a [`RunResults`].
pub(crate) fn summarize(label: String, runner: String, cases: Vec<CaseResult>) -> RunResults {
    let total = cases.len();
    let passed = cases.iter().filter(|case| case.passed).count();
    let pass_rate = if total == 0 {
        0.0
    } else {
        passed as f64 / total as f64
    };
    RunResults {
        label,
        runner,
        created: Utc::now(),
        cases,
        total,
        passed,
        pass_rate,
    }
}

#[allow(clippy::too_many_arguments)]
async fn add(
    id: String,
    prompt: String,
    check: String,
    git_ref: String,
    timeout: u64,
    check_timeout: u64,
    tags: Vec<String>,
    notes: Option<String>,
) -> Result<()> {
    let root = std::env::current_dir().context("determining current directory")?;
    validate_case_id(&id)?;
    let base_ref = git::rev_parse(&root, &git_ref)
        .await
        .context("resolving --git-ref (is the current directory inside a git repo?)")?;
    let case = BenchCase {
        id,
        prompt,
        base_ref,
        check,
        timeout_secs: timeout,
        check_timeout_secs: check_timeout,
        tags,
        source: "manual".to_string(),
        created: Utc::now(),
        notes,
    };
    let path = write_case(&root, &case)?;
    println!("added case '{}' → {}", case.id, path.display());
    Ok(())
}

fn list(trajectories: bool) -> Result<()> {
    let root = std::env::current_dir().context("determining current directory")?;
    let cases = load_cases(&root)?;
    if cases.is_empty() {
        println!(
            "no cases yet — record tasks by running wizard headless, then \
             `wizard bench promote`, or create one with `wizard bench add`"
        );
    } else {
        let id_width = cases.iter().map(|c| c.id.len()).max().unwrap_or(0);
        for case in &cases {
            let sha: String = case.base_ref.chars().take(8).collect();
            println!(
                "{:<id_width$}  {sha:<8}  {:<8}  {:<40}  {}",
                case.id,
                case.source,
                truncate(&case.check, 40),
                case.tags.join(",")
            );
        }
    }
    if trajectories {
        let records = read_trajectories(&root)?;
        println!();
        if records.is_empty() {
            println!(
                "no trajectories yet — wizard records them automatically \
                 during headless (sovereign / continuous) runs"
            );
        } else {
            println!("last {} trajectories:", records.len().min(20));
            for record in records.iter().rev().take(20) {
                let short: String = record.id.chars().take(8).collect();
                let dirty = if record.dirty { "dirty" } else { "clean" };
                println!(
                    "{short}  {}  {:<14}  {dirty:<5}  {}",
                    record.timestamp.format("%Y-%m-%d %H:%M:%S"),
                    record.done_reason,
                    truncate(&record.prompt, 60)
                );
            }
        }
    }
    Ok(())
}

fn promote(
    trajectory: String,
    check: String,
    id: Option<String>,
    timeout: u64,
    check_timeout: u64,
    tags: Vec<String>,
    notes: Option<String>,
) -> Result<()> {
    let root = std::env::current_dir().context("determining current directory")?;
    let records = read_trajectories(&root)?;
    let matches: Vec<&TrajectoryRecord> = records
        .iter()
        .filter(|record| record.id.starts_with(&trajectory))
        .collect();
    let record = match matches.as_slice() {
        [] => bail!(
            "no recorded trajectory matches {trajectory:?} — \
             see `wizard bench list --trajectories`"
        ),
        [one] => *one,
        many => bail!(
            "{} trajectories match {trajectory:?} — use a longer prefix",
            many.len()
        ),
    };
    let Some(base_ref) = record.git_ref.clone() else {
        bail!(
            "trajectory {} was not recorded in a git repo, so there is no base ref to replay from",
            record.id
        );
    };
    if record.dirty {
        bail!(
            "trajectory {}: repo was dirty when recorded; replay would not reproduce the \
             starting state",
            record.id
        );
    }
    let case = BenchCase {
        id: id.unwrap_or_else(|| record.id.clone()),
        prompt: record.prompt.clone(),
        base_ref,
        check,
        timeout_secs: timeout,
        check_timeout_secs: check_timeout,
        tags,
        source: "recorded".to_string(),
        created: Utc::now(),
        notes,
    };
    let path = write_case(&root, &case)?;
    println!("promoted trajectory {} → {}", record.id, path.display());
    Ok(())
}

/// One line of the compare table.
#[derive(Debug, PartialEq)]
pub(crate) struct CompareRow {
    pub id: String,
    pub a_status: String,
    pub b_status: String,
    /// "↑" pass gained (A→B), "↓" pass lost, "" unchanged.
    pub marker: &'static str,
}

/// Pure comparison over the union of case ids; "—" marks a missing case.
pub(crate) fn compare_rows(a: &RunResults, b: &RunResults) -> Vec<CompareRow> {
    use std::collections::{BTreeMap, BTreeSet};

    let a_cases: BTreeMap<&str, &CaseResult> = a.cases.iter().map(|c| (c.id.as_str(), c)).collect();
    let b_cases: BTreeMap<&str, &CaseResult> = b.cases.iter().map(|c| (c.id.as_str(), c)).collect();
    let ids: BTreeSet<&str> = a_cases.keys().chain(b_cases.keys()).copied().collect();

    ids.into_iter()
        .map(|id| {
            let in_a = a_cases.get(id).copied();
            let in_b = b_cases.get(id).copied();
            let a_passed = in_a.is_some_and(|c| c.passed);
            let b_passed = in_b.is_some_and(|c| c.passed);
            let marker = match (a_passed, b_passed) {
                (false, true) => "↑",
                (true, false) => "↓",
                _ => "",
            };
            CompareRow {
                id: id.to_string(),
                a_status: in_a.map_or_else(|| "—".to_string(), |c| c.status.clone()),
                b_status: in_b.map_or_else(|| "—".to_string(), |c| c.status.clone()),
                marker,
            }
        })
        .collect()
}

fn compare(a_path: &Path, b_path: &Path) -> Result<()> {
    let a = load_results(a_path)?;
    let b = load_results(b_path)?;
    let rows = compare_rows(&a, &b);

    let id_width = rows.iter().map(|r| r.id.len()).max().unwrap_or(4).max(4);
    println!("{:<id_width$}  {:<8}  {:<8}", "case", "A", "B");
    for row in &rows {
        println!(
            "{:<id_width$}  {:<8}  {:<8}  {}",
            row.id, row.a_status, row.b_status, row.marker
        );
    }
    println!();
    println!(
        "A: {}  {}/{} ({:.1}%)",
        a.label,
        a.passed,
        a.total,
        a.pass_rate * 100.0
    );
    println!(
        "B: {}  {}/{} ({:.1}%)",
        b.label,
        b.passed,
        b.total,
        b.pass_rate * 100.0
    );
    println!("delta: {:+.1} pts", (b.pass_rate - a.pass_rate) * 100.0);
    Ok(())
}

fn load_results(path: &Path) -> Result<RunResults> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading results {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing results {}", path.display()))
}

/// Truncate to at most `max` chars, marking the cut with an ellipsis.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn case_result(id: &str, status: &str) -> CaseResult {
        CaseResult {
            id: id.to_string(),
            status: status.to_string(),
            passed: status == "pass",
            harness_exit: Some(0),
            check_exit: Some(if status == "pass" { 0 } else { 1 }),
            harness_secs: 1.0,
            check_secs: 0.1,
            error: None,
        }
    }

    #[test]
    fn case_id_validation() {
        for ok in ["touch-case", "abc_123", "X", "0-9_a"] {
            assert!(validate_case_id(ok).is_ok(), "{ok:?} must be accepted");
        }
        for bad in ["", "../evil", "a b", "a/b", "a.b", "café"] {
            assert!(validate_case_id(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn bench_case_toml_round_trip() {
        let case = BenchCase {
            id: "round-trip".to_string(),
            prompt: "do the thing\nwith a newline".to_string(),
            base_ref: "0123456789abcdef0123456789abcdef01234567".to_string(),
            check: "cargo test".to_string(),
            timeout_secs: 900,
            check_timeout_secs: 300,
            tags: vec!["smoke".to_string()],
            source: "manual".to_string(),
            created: Utc::now(),
            notes: Some("hand-made".to_string()),
        };
        let text = toml::to_string_pretty(&case).expect("serializes");
        let back: BenchCase = toml::from_str(&text).expect("parses back");
        assert_eq!(back.id, case.id);
        assert_eq!(back.prompt, case.prompt);
        assert_eq!(back.base_ref, case.base_ref);
        assert_eq!(back.check, case.check);
        assert_eq!(back.timeout_secs, case.timeout_secs);
        assert_eq!(back.check_timeout_secs, case.check_timeout_secs);
        assert_eq!(back.tags, case.tags);
        assert_eq!(back.source, case.source);
        assert_eq!(back.created, case.created);
        assert_eq!(back.notes, case.notes);
    }

    #[test]
    fn trajectory_record_jsonl_round_trip() {
        let record = TrajectoryRecord {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: Utc::now(),
            prompt: "fix the flaky test".to_string(),
            git_ref: Some("0123456789abcdef0123456789abcdef01234567".to_string()),
            dirty: false,
            done_reason: "Completed".to_string(),
            duration_secs: 12.5,
            model: "qwen3.5:9b".to_string(),
            mode: "sovereign".to_string(),
        };
        let line = serde_json::to_string(&record).expect("serializes");
        assert!(!line.contains('\n'), "one record must stay on one line");
        let back: TrajectoryRecord = serde_json::from_str(&line).expect("parses back");
        assert_eq!(back.id, record.id);
        assert_eq!(back.timestamp, record.timestamp);
        assert_eq!(back.git_ref, record.git_ref);
        assert_eq!(back.dirty, record.dirty);
        assert_eq!(back.done_reason, record.done_reason);
    }

    #[test]
    fn summarize_aggregation_math() {
        let results = summarize(
            "label".to_string(),
            "true".to_string(),
            vec![
                case_result("a", "pass"),
                case_result("b", "fail"),
                case_result("c", "pass"),
                case_result("d", "timeout"),
            ],
        );
        assert_eq!(results.total, 4);
        assert_eq!(results.passed, 2);
        assert!((results.pass_rate - 0.5).abs() < f64::EPSILON);

        let empty = summarize("e".to_string(), "true".to_string(), Vec::new());
        assert_eq!(empty.pass_rate, 0.0);
    }

    #[test]
    fn compare_rows_marks_gains_losses_and_missing() {
        let a = summarize(
            "a".to_string(),
            "true".to_string(),
            vec![case_result("x", "pass"), case_result("y", "fail")],
        );
        let b = summarize(
            "b".to_string(),
            "true".to_string(),
            vec![case_result("y", "pass"), case_result("z", "fail")],
        );
        let rows = compare_rows(&a, &b);
        assert_eq!(
            rows,
            vec![
                CompareRow {
                    id: "x".to_string(),
                    a_status: "pass".to_string(),
                    b_status: "—".to_string(),
                    marker: "↓",
                },
                CompareRow {
                    id: "y".to_string(),
                    a_status: "fail".to_string(),
                    b_status: "pass".to_string(),
                    marker: "↑",
                },
                CompareRow {
                    id: "z".to_string(),
                    a_status: "—".to_string(),
                    b_status: "fail".to_string(),
                    marker: "",
                },
            ]
        );
    }
}
