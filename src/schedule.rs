//! Cron scheduler: `wizard schedule` (entry CRUD + foreground run) and the
//! `wizard scheduler` daemon.
//!
//! Entries live in `~/.wizard/schedule.toml` as `[[entries]]` blocks. The
//! daemon reloads the file every pass, so edits — by hand or via `wizard
//! schedule add/remove` — are picked up without a restart. Missed runs are
//! never backfilled: each entry's clock starts at daemon startup (or its
//! last fire within this daemon's lifetime), so occurrences that passed
//! while the daemon was down collapse into nothing, and several occurrences
//! missed during one long sleep collapse into a single fire.
//!
//! Everything here is config-independent: no `config.toml` load, no LLM in
//! this process. Jobs are spawned `wizard` child processes (via
//! `current_exe()`) that load their own config, exactly like a user-invoked
//! headless run.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, TimeZone};
use croner::Cron;
use croner::parser::{CronParser, Seconds, Year};
use serde::{Deserialize, Serialize};

use crate::cli::ScheduleCmd;
use crate::config::Config;

/// Daemon sleep cap so schedule reloads, job reaping, and timeout
/// enforcement all happen at least this often.
const MAX_SLEEP: Duration = Duration::from_secs(60);

/// Grace beyond an entry's `max_hours` before the daemon hard-kills the
/// child. The child also receives `--max-hours`, so the normal path is a
/// graceful self-stop (exit code 4); the kill is the backstop.
const KILL_GRACE: Duration = Duration::from_secs(120);

/// Rotate `scheduler.log` past this size (rename to `.log.old`).
const LOG_ROTATE_BYTES: u64 = 5 * 1024 * 1024;

/// One scheduled job (`[[entries]]` in schedule.toml).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleEntry {
    /// Unique key (`[a-zA-Z0-9_-]+`).
    pub name: String,
    /// Standard 5-field cron expression, evaluated in local time.
    pub cron: String,
    /// Task prompt handed to the spawned headless run.
    pub prompt: String,
    /// Directory the run executes in.
    pub cwd: PathBuf,
    /// "sovereign" (default) or "continuous".
    #[serde(default = "default_mode")]
    pub mode: String,
    /// Wall-clock cap in hours for the spawned run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_hours: Option<f64>,
    /// Disabled entries are kept in the file but never fired.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_mode() -> String {
    "sovereign".to_string()
}

fn default_enabled() -> bool {
    true
}

/// Contents of `~/.wizard/schedule.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScheduleFile {
    #[serde(default)]
    pub entries: Vec<ScheduleEntry>,
}

/// Dispatch a `wizard schedule` subcommand against the default schedule
/// file. Returns the process exit code (`run` propagates the child's).
pub async fn run(cmd: ScheduleCmd) -> Result<i32> {
    let path = Config::schedule_path()?;
    match cmd {
        ScheduleCmd::Add {
            name,
            cron,
            prompt,
            cwd,
            max_hours,
            mode,
        } => {
            let cwd = cwd
                .canonicalize()
                .with_context(|| format!("--cwd {}: directory must exist", cwd.display()))?;
            let entry = ScheduleEntry {
                name,
                cron,
                prompt,
                cwd,
                mode,
                max_hours,
                enabled: true,
            };
            add_entry(&path, entry.clone())?;
            let next = next_occurrence(&entry, &Local::now())?;
            println!(
                "added '{}' — next fire {} ({})",
                entry.name,
                next.format("%Y-%m-%d %H:%M:%S %Z"),
                humanize_until(next - Local::now()),
            );
            Ok(0)
        }
        ScheduleCmd::List => {
            list(&path)?;
            Ok(0)
        }
        ScheduleCmd::Remove { name } => {
            remove_entry(&path, &name)?;
            println!("removed '{name}'");
            Ok(0)
        }
        ScheduleCmd::Run { name } => run_foreground(&path, &name).await,
    }
}

/// Strict 5-field cron parse (no seconds, no year field) — what crontab
/// users expect; rejects everything else up front.
pub fn parse_cron(expr: &str) -> Result<Cron> {
    CronParser::builder()
        .seconds(Seconds::Disallowed)
        .year(Year::Disallowed)
        .build()
        .parse(expr)
        .map_err(|err| {
            anyhow::anyhow!(
                "invalid cron expression {expr:?}: {err} \
                 (expected 5 fields: minute hour day month weekday)"
            )
        })
}

/// Entry names become toml keys and log labels; keep them path-safe.
fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        bail!("invalid entry name {name:?}: only [a-zA-Z0-9_-] is allowed");
    }
    Ok(())
}

/// Reject anything but the two headless job modes.
fn validate_mode(mode: &str) -> Result<()> {
    match mode {
        "sovereign" | "continuous" => Ok(()),
        other => bail!("invalid mode {other:?} (expected sovereign or continuous)"),
    }
}

/// Full per-entry validation: name, mode, cron, and an existing cwd.
fn validate_entry(entry: &ScheduleEntry) -> Result<()> {
    validate_name(&entry.name)?;
    validate_mode(&entry.mode)?;
    parse_cron(&entry.cron)?;
    if !entry.cwd.is_dir() {
        bail!(
            "cwd {} does not exist or is not a directory",
            entry.cwd.display()
        );
    }
    if let Some(hours) = entry.max_hours
        && (!hours.is_finite() || hours <= 0.0)
    {
        bail!("max_hours must be a positive number, got {hours}");
    }
    Ok(())
}

/// Load the schedule file; a missing file is an empty schedule.
pub fn load_schedule(path: &Path) -> Result<ScheduleFile> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ScheduleFile::default());
        }
        Err(err) => return Err(err).context(format!("reading {}", path.display())),
    };
    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Write the schedule file, creating parent directories as needed.
pub fn save_schedule(path: &Path, file: &ScheduleFile) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let text = toml::to_string_pretty(file).context("serializing schedule")?;
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

/// Validate and append a new entry; the name must be unique.
pub fn add_entry(path: &Path, entry: ScheduleEntry) -> Result<()> {
    validate_entry(&entry)?;
    let mut file = load_schedule(path)?;
    if file.entries.iter().any(|e| e.name == entry.name) {
        bail!(
            "entry '{}' already exists — remove it first or pick another name",
            entry.name
        );
    }
    file.entries.push(entry);
    save_schedule(path, &file)
}

/// Remove an entry by name; error when absent.
pub fn remove_entry(path: &Path, name: &str) -> Result<()> {
    let mut file = load_schedule(path)?;
    let before = file.entries.len();
    file.entries.retain(|e| e.name != name);
    if file.entries.len() == before {
        bail!("no entry named '{name}' — see `wizard schedule list`");
    }
    save_schedule(path, &file)
}

/// Next fire strictly after `now` for one entry.
fn next_occurrence<Tz: TimeZone>(
    entry: &ScheduleEntry,
    now: &DateTime<Tz>,
) -> Result<DateTime<Tz>> {
    let cron = parse_cron(&entry.cron)?;
    cron.find_next_occurrence(now, false)
        .map_err(|err| anyhow::anyhow!("computing next fire for '{}': {err}", entry.name))
}

fn list(path: &Path) -> Result<()> {
    let file = load_schedule(path)?;
    if file.entries.is_empty() {
        println!("no entries — add one with `wizard schedule add`");
        return Ok(());
    }
    let now = Local::now();
    let name_width = file
        .entries
        .iter()
        .map(|e| e.name.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let cron_width = file
        .entries
        .iter()
        .map(|e| e.cron.len())
        .max()
        .unwrap_or(4)
        .max(4);
    println!(
        "{:<name_width$}  {:<cron_width$}  {:<3}  {:<32}  cwd",
        "name", "cron", "on", "next"
    );
    for entry in &file.entries {
        let on = if entry.enabled { "yes" } else { "no" };
        let next = match next_occurrence(entry, &now) {
            Ok(next) => format!(
                "{} ({})",
                next.format("%Y-%m-%d %H:%M"),
                humanize_until(next - now)
            ),
            Err(_) => "invalid cron".to_string(),
        };
        println!(
            "{:<name_width$}  {:<cron_width$}  {on:<3}  {next:<32}  {}",
            entry.name,
            entry.cron,
            entry.cwd.display()
        );
    }
    Ok(())
}

/// Argv (after the binary itself) of the wizard child that executes `entry`
/// — the single source of truth for both the daemon and `schedule run`.
/// `--max-hours` is passed through so the child winds down gracefully on
/// its own; the daemon's kill at `max_hours + grace` is only a backstop.
pub fn child_args(entry: &ScheduleEntry) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    if entry.mode == "continuous" {
        // --continuous implies sovereign.
        args.push("--continuous".to_string());
    } else {
        args.push("--mode".to_string());
        args.push("sovereign".to_string());
    }
    args.push("-p".to_string());
    args.push(entry.prompt.clone());
    args.push("--cwd".to_string());
    args.push(entry.cwd.display().to_string());
    if let Some(hours) = entry.max_hours {
        args.push("--max-hours".to_string());
        args.push(hours.to_string());
    }
    args
}

/// Build the child command: this binary, the entry's argv, cwd set (both
/// via `--cwd` and the process working directory, so relative paths inside
/// the run resolve correctly either way).
fn child_command(entry: &ScheduleEntry) -> Result<tokio::process::Command> {
    let exe = std::env::current_exe().context("locating the wizard binary for the job")?;
    let mut cmd = tokio::process::Command::new(exe);
    cmd.args(child_args(entry)).current_dir(&entry.cwd);
    Ok(cmd)
}

/// `wizard schedule run <name>`: spawn the entry's job in the foreground
/// with inherited stdio and return its exit code. `max_hours` is enforced
/// here too: on the hard timeout the child is killed and the run exits 4
/// (the time-limit exit code).
async fn run_foreground(path: &Path, name: &str) -> Result<i32> {
    let file = load_schedule(path)?;
    let entry = file
        .entries
        .iter()
        .find(|e| e.name == name)
        .with_context(|| format!("no entry named '{name}' — see `wizard schedule list`"))?;
    validate_entry(entry)?;

    let mut child = child_command(entry)?
        .kill_on_drop(true)
        .spawn()
        .context("spawning the job")?;
    let deadline = entry
        .max_hours
        .map(|hours| Duration::from_secs_f64(hours * 3600.0) + KILL_GRACE);
    let status = match deadline {
        Some(cap) => match tokio::time::timeout(cap, child.wait()).await {
            Ok(status) => status.context("waiting for the job")?,
            Err(_elapsed) => {
                child.kill().await.ok();
                eprintln!("job '{name}' exceeded max_hours — killed");
                return Ok(4);
            }
        },
        None => child.wait().await.context("waiting for the job")?,
    };
    Ok(status.code().unwrap_or(1))
}

// ---------------------------------------------------------------------------
// Daemon
// ---------------------------------------------------------------------------

/// A job the daemon has spawned and not yet reaped.
struct RunningJob {
    name: String,
    child: tokio::process::Child,
    started: Instant,
    /// `max_hours + KILL_GRACE` from spawn; `None` = unbounded.
    deadline: Option<Instant>,
}

/// Pure due-detection: which enabled entries have an occurrence at or
/// before `now`, measured strictly after their basis — the entry's last
/// fire within this daemon's lifetime, or `started` for entries that have
/// not fired yet. Entries with invalid cron expressions are never due.
pub fn due_entries<'a, Tz: TimeZone>(
    entries: &'a [ScheduleEntry],
    last_fired: &HashMap<String, DateTime<Tz>>,
    started: &DateTime<Tz>,
    now: &DateTime<Tz>,
) -> Vec<&'a ScheduleEntry> {
    entries
        .iter()
        .filter(|entry| {
            if !entry.enabled {
                return false;
            }
            let Ok(cron) = parse_cron(&entry.cron) else {
                return false;
            };
            let basis = last_fired.get(&entry.name).unwrap_or(started);
            match cron.find_next_occurrence(basis, false) {
                Ok(next) => next <= *now,
                Err(_) => false,
            }
        })
        .collect()
}

/// Earliest next fire across enabled, valid entries, strictly after `now`.
pub fn next_fire_across<Tz: TimeZone>(
    entries: &[ScheduleEntry],
    now: &DateTime<Tz>,
) -> Option<DateTime<Tz>> {
    entries
        .iter()
        .filter(|entry| entry.enabled)
        .filter_map(|entry| {
            parse_cron(&entry.cron)
                .ok()?
                .find_next_occurrence(now, false)
                .ok()
        })
        .min()
}

/// Append a timestamped line to the scheduler log (rotating past
/// [`LOG_ROTATE_BYTES`]) and echo it to stdout for foreground / journald
/// visibility. Logging must never take the daemon down, so errors are
/// swallowed.
fn log_line(path: &Path, msg: &str) {
    let line = format!("{} {msg}", Local::now().format("%Y-%m-%d %H:%M:%S%z"));
    println!("{line}");
    if let Ok(meta) = std::fs::metadata(path)
        && meta.len() > LOG_ROTATE_BYTES
    {
        let _ = std::fs::rename(path, path.with_extension("log.old"));
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{line}");
    }
}

/// Spawn one entry's job for the daemon: stdio detached to null (the run's
/// own transcript lives in the child's `~/.wizard` state, not here).
fn spawn_job(entry: &ScheduleEntry) -> Result<RunningJob> {
    let child = child_command(entry)?
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("spawning job")?;
    let now = Instant::now();
    Ok(RunningJob {
        name: entry.name.clone(),
        child,
        started: now,
        deadline: entry
            .max_hours
            .map(|hours| now + Duration::from_secs_f64(hours * 3600.0) + KILL_GRACE),
    })
}

/// Reap finished children and kill the ones past their deadline.
async fn reap_jobs(jobs: &mut Vec<RunningJob>, log_path: &Path) {
    let mut kept = Vec::with_capacity(jobs.len());
    for mut job in jobs.drain(..) {
        match job.child.try_wait() {
            Ok(Some(status)) => {
                log_line(
                    log_path,
                    &format!(
                        "finished '{}' — exit {} after {:.0}s",
                        job.name,
                        status
                            .code()
                            .map_or_else(|| "signal".to_string(), |code| code.to_string()),
                        job.started.elapsed().as_secs_f64()
                    ),
                );
            }
            Ok(None) => {
                if job
                    .deadline
                    .is_some_and(|deadline| Instant::now() >= deadline)
                {
                    job.child.kill().await.ok();
                    log_line(
                        log_path,
                        &format!(
                            "timeout '{}' — killed after {:.0}s (max_hours exceeded)",
                            job.name,
                            job.started.elapsed().as_secs_f64()
                        ),
                    );
                } else {
                    kept.push(job);
                }
            }
            Err(err) => {
                log_line(
                    log_path,
                    &format!("error waiting on '{}': {err} — dropping it", job.name),
                );
            }
        }
    }
    *jobs = kept;
}

/// `wizard scheduler`: the foreground daemon loop. Each pass it reaps
/// children, reloads the schedule, fires every due entry (concurrently —
/// one spawn each, never serialized), then sleeps until the next fire,
/// capped at [`MAX_SLEEP`] so reloads stay timely. Ctrl-C kills running
/// jobs and exits 0.
pub async fn run_daemon() -> Result<i32> {
    let schedule_path = Config::schedule_path()?;
    let logs_dir = Config::logs_dir()?;
    std::fs::create_dir_all(&logs_dir)
        .with_context(|| format!("creating {}", logs_dir.display()))?;
    let log_path = logs_dir.join("scheduler.log");

    let started = Local::now();
    let mut last_fired: HashMap<String, DateTime<Local>> = HashMap::new();
    let mut jobs: Vec<RunningJob> = Vec::new();
    log_line(
        &log_path,
        &format!(
            "scheduler started — schedule {} (missed runs are not backfilled)",
            schedule_path.display()
        ),
    );

    loop {
        reap_jobs(&mut jobs, &log_path).await;

        let entries = match load_schedule(&schedule_path) {
            Ok(file) => file.entries,
            Err(err) => {
                log_line(&log_path, &format!("schedule load failed: {err:#}"));
                Vec::new()
            }
        };

        let now = Local::now();
        let due: Vec<ScheduleEntry> = due_entries(&entries, &last_fired, &started, &now)
            .into_iter()
            .cloned()
            .collect();
        for entry in due {
            last_fired.insert(entry.name.clone(), now);
            match spawn_job(&entry) {
                Ok(job) => {
                    log_line(
                        &log_path,
                        &format!(
                            "fired '{}' (pid {}) — {} in {}",
                            job.name,
                            job.child.id().unwrap_or(0),
                            entry.mode,
                            entry.cwd.display()
                        ),
                    );
                    jobs.push(job);
                }
                Err(err) => {
                    log_line(
                        &log_path,
                        &format!("spawn failed for '{}': {err:#}", entry.name),
                    );
                }
            }
        }

        let sleep_for = match next_fire_across(&entries, &Local::now()) {
            Some(next) => (next - Local::now())
                .to_std()
                .unwrap_or(Duration::ZERO)
                .clamp(Duration::from_secs(1), MAX_SLEEP),
            None => MAX_SLEEP,
        };
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                result.context("listening for ctrl-c")?;
                log_line(
                    &log_path,
                    &format!("interrupt — stopping; killing {} running job(s)", jobs.len()),
                );
                for mut job in jobs {
                    job.child.kill().await.ok();
                    log_line(&log_path, &format!("killed '{}' on shutdown", job.name));
                }
                log_line(&log_path, "scheduler stopped");
                return Ok(0);
            }
            () = tokio::time::sleep(sleep_for) => {}
        }
    }
}

/// "in 45s" / "in 10h 32m" / "in 3d 4h" for a future delta ("now" when it
/// is not in the future).
fn humanize_until(delta: chrono::TimeDelta) -> String {
    let secs = delta.num_seconds();
    if secs <= 0 {
        return "now".to_string();
    }
    let (days, hours, mins) = (secs / 86_400, (secs % 86_400) / 3_600, (secs % 3_600) / 60);
    if days > 0 {
        format!("in {days}d {hours}h")
    } else if hours > 0 {
        format!("in {hours}h {mins}m")
    } else if mins > 0 {
        format!("in {mins}m")
    } else {
        format!("in {secs}s")
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;

    fn entry(name: &str, cron: &str) -> ScheduleEntry {
        ScheduleEntry {
            name: name.to_string(),
            cron: cron.to_string(),
            prompt: "do the thing".to_string(),
            cwd: std::env::temp_dir(),
            mode: "sovereign".to_string(),
            max_hours: None,
            enabled: true,
        }
    }

    fn utc(s: &str) -> DateTime<Utc> {
        s.parse().expect("test timestamp parses")
    }

    #[test]
    fn cron_validation_accepts_standard_five_fields() {
        for ok in ["0 3 * * *", "*/5 * * * *", "0 0 1 1 0", "30 6 * * MON-FRI"] {
            assert!(parse_cron(ok).is_ok(), "{ok:?} must parse");
        }
    }

    #[test]
    fn cron_validation_rejects_garbage_and_wrong_field_counts() {
        for bad in ["", "not a cron", "0 3 * *", "0 0 3 * * *", "61 * * * *"] {
            assert!(parse_cron(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn next_occurrence_from_a_known_instant() {
        let cron = parse_cron("0 3 * * *").expect("parses");
        let now = utc("2026-01-01T00:00:00Z");
        let next = cron.find_next_occurrence(&now, false).expect("next");
        assert_eq!(next, utc("2026-01-01T03:00:00Z"));

        // Strictly after: exactly at the fire instant the next one is tomorrow.
        let at_fire = utc("2026-01-01T03:00:00Z");
        let next = cron.find_next_occurrence(&at_fire, false).expect("next");
        assert_eq!(next, utc("2026-01-02T03:00:00Z"));
    }

    #[test]
    fn schedule_file_round_trip_add_remove() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("schedule.toml");

        add_entry(&path, entry("nightly", "0 3 * * *")).expect("first add");
        add_entry(&path, entry("hourly", "0 * * * *")).expect("second add");

        let file = load_schedule(&path).expect("loads");
        assert_eq!(file.entries.len(), 2);
        assert_eq!(file.entries[0].name, "nightly");
        assert_eq!(file.entries[0].cron, "0 3 * * *");
        assert_eq!(file.entries[0].mode, "sovereign");
        assert!(file.entries[0].enabled, "enabled defaults to true");

        // Duplicate names are rejected.
        let err = add_entry(&path, entry("nightly", "0 4 * * *")).unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");

        remove_entry(&path, "nightly").expect("removes");
        let file = load_schedule(&path).expect("loads");
        assert_eq!(file.entries.len(), 1);
        assert_eq!(file.entries[0].name, "hourly");

        let err = remove_entry(&path, "nightly").unwrap_err();
        assert!(err.to_string().contains("no entry"), "{err}");
    }

    #[test]
    fn missing_schedule_file_is_an_empty_schedule() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = load_schedule(&dir.path().join("absent.toml")).expect("loads");
        assert!(file.entries.is_empty());
    }

    #[test]
    fn add_rejects_invalid_cron_name_mode_and_cwd() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("schedule.toml");

        let mut bad_cron = entry("a", "whenever");
        bad_cron.cron = "whenever".to_string();
        assert!(add_entry(&path, bad_cron).is_err());

        let bad_name = entry("no spaces", "0 3 * * *");
        assert!(add_entry(&path, bad_name).is_err());

        let mut bad_mode = entry("b", "0 3 * * *");
        bad_mode.mode = "genie".to_string();
        assert!(add_entry(&path, bad_mode).is_err());

        let mut bad_cwd = entry("c", "0 3 * * *");
        bad_cwd.cwd = dir.path().join("does-not-exist");
        assert!(add_entry(&path, bad_cwd).is_err());

        let mut bad_hours = entry("d", "0 3 * * *");
        bad_hours.max_hours = Some(0.0);
        assert!(add_entry(&path, bad_hours).is_err());

        assert!(
            load_schedule(&path).expect("loads").entries.is_empty(),
            "nothing invalid may be persisted"
        );
    }

    #[test]
    fn legacy_minimal_entry_parses_with_defaults() {
        let text = "[[entries]]\n\
                    name = \"n\"\n\
                    cron = \"0 3 * * *\"\n\
                    prompt = \"p\"\n\
                    cwd = \"/tmp\"\n";
        let file: ScheduleFile = toml::from_str(text).expect("parses");
        assert_eq!(file.entries[0].mode, "sovereign");
        assert_eq!(file.entries[0].max_hours, None);
        assert!(file.entries[0].enabled);
    }

    #[test]
    fn due_detection_fires_after_an_occurrence_passes() {
        let entries = vec![entry("minutely", "* * * * *")];
        let started = utc("2026-01-01T00:00:30Z");
        let mut last_fired: HashMap<String, DateTime<Utc>> = HashMap::new();

        // 10 seconds in: the first occurrence after start (00:01:00) has not
        // passed yet.
        let now = utc("2026-01-01T00:00:40Z");
        assert!(due_entries(&entries, &last_fired, &started, &now).is_empty());

        // Past 00:01:00: due exactly once.
        let now = utc("2026-01-01T00:01:05Z");
        let due = due_entries(&entries, &last_fired, &started, &now);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].name, "minutely");

        // After recording the fire, the same instant is no longer due...
        last_fired.insert("minutely".to_string(), now);
        assert!(due_entries(&entries, &last_fired, &started, &now).is_empty());

        // ...and several missed occurrences collapse into one fire.
        let later = utc("2026-01-01T00:05:30Z");
        let due = due_entries(&entries, &last_fired, &started, &later);
        assert_eq!(due.len(), 1);
    }

    #[test]
    fn due_detection_skips_disabled_and_invalid_entries() {
        let mut disabled = entry("off", "* * * * *");
        disabled.enabled = false;
        let mut invalid = entry("broken", "* * * * *");
        invalid.cron = "not a cron".to_string();
        let entries = vec![disabled, invalid, entry("on", "* * * * *")];

        let started = utc("2026-01-01T00:00:00Z");
        let now = utc("2026-01-01T00:02:00Z");
        let due = due_entries(&entries, &HashMap::new(), &started, &now);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].name, "on");
    }

    #[test]
    fn due_detection_does_not_backfill_before_startup() {
        // Daily 03:00 job; daemon starts at 10:00 — yesterday's and today's
        // 03:00 are gone, the entry is not due until tomorrow 03:00.
        let entries = vec![entry("daily", "0 3 * * *")];
        let started = utc("2026-01-01T10:00:00Z");
        let now = utc("2026-01-01T23:59:00Z");
        assert!(due_entries(&entries, &HashMap::new(), &started, &now).is_empty());

        let tomorrow = utc("2026-01-02T03:00:00Z");
        let due = due_entries(&entries, &HashMap::new(), &started, &tomorrow);
        assert_eq!(due.len(), 1);
    }

    #[test]
    fn next_fire_across_picks_the_earliest_enabled_entry() {
        let mut late = entry("late", "0 12 * * *");
        late.enabled = true;
        let early = entry("early", "0 6 * * *");
        let mut disabled = entry("disabled", "0 1 * * *");
        disabled.enabled = false;
        let entries = vec![late, early, disabled];

        let now = utc("2026-01-01T00:00:00Z");
        let next = next_fire_across(&entries, &now).expect("a next fire exists");
        assert_eq!(next, utc("2026-01-01T06:00:00Z"));

        assert!(next_fire_across(&[], &now).is_none());
    }

    #[test]
    fn child_args_sovereign_with_cap() {
        let mut e = entry("n", "0 3 * * *");
        e.prompt = "tidy the repo".to_string();
        e.cwd = PathBuf::from("/home/user/proj");
        e.max_hours = Some(2.0);
        assert_eq!(
            child_args(&e),
            vec![
                "--mode",
                "sovereign",
                "-p",
                "tidy the repo",
                "--cwd",
                "/home/user/proj",
                "--max-hours",
                "2",
            ]
        );
    }

    #[test]
    fn child_args_continuous_without_cap() {
        let mut e = entry("n", "0 3 * * *");
        e.mode = "continuous".to_string();
        e.prompt = "keep improving".to_string();
        e.cwd = PathBuf::from("/srv/app");
        assert_eq!(
            child_args(&e),
            vec!["--continuous", "-p", "keep improving", "--cwd", "/srv/app"]
        );
    }

    #[test]
    fn humanize_until_buckets() {
        use chrono::TimeDelta;
        assert_eq!(humanize_until(TimeDelta::seconds(-5)), "now");
        assert_eq!(humanize_until(TimeDelta::seconds(45)), "in 45s");
        assert_eq!(humanize_until(TimeDelta::seconds(150)), "in 2m");
        assert_eq!(humanize_until(TimeDelta::seconds(37_920)), "in 10h 32m");
        assert_eq!(humanize_until(TimeDelta::seconds(273_600)), "in 3d 4h");
    }
}
