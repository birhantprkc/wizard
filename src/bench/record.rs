//! Trajectory recorder: append one JSONL line per completed headless turn.
//!
//! Recording is how `wizard bench` gets benchmark candidates for free — every
//! real sovereign/continuous turn lands in `.wizard/trajectories.jsonl`, and
//! `wizard bench promote` turns the good ones into replayable cases.

use std::io::Write as _;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;

use super::{TrajectoryRecord, git, trajectories_path};

/// Record one completed headless turn.
///
/// Infallible by design: recording is a side channel, and a full disk or odd
/// permissions must never break the agent run itself, so every error is
/// swallowed into a warning. A no-op when `WIZARD_BENCH` or `WIZARD_FLEET`
/// is set — bench replays and fleet workers must not pollute the trajectory
/// log with their own runs.
pub async fn record(
    project_root: &Path,
    prompt: &str,
    done_reason: &str,
    duration: Duration,
    model: &str,
    mode: &str,
) {
    if std::env::var_os("WIZARD_BENCH").is_some() || std::env::var_os("WIZARD_FLEET").is_some() {
        return;
    }
    let (git_ref, dirty) = match git::head_and_dirty(project_root).await {
        Some((sha, dirty)) => (Some(sha), dirty),
        None => (None, false),
    };
    let record = TrajectoryRecord {
        id: uuid::Uuid::new_v4().to_string(),
        timestamp: Utc::now(),
        prompt: prompt.to_string(),
        git_ref,
        dirty,
        done_reason: done_reason.to_string(),
        duration_secs: duration.as_secs_f64(),
        model: model.to_string(),
        mode: mode.to_string(),
    };
    if let Err(err) = append(project_root, &record) {
        tracing::warn!("failed to record trajectory: {err:#}");
    }
}

fn append(project_root: &Path, record: &TrajectoryRecord) -> Result<()> {
    let path = trajectories_path(project_root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut line = serde_json::to_string(record).context("serializing trajectory record")?;
    line.push('\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    file.write_all(line.as_bytes())
        .with_context(|| format!("appending to {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::super::read_trajectories;
    use super::*;

    fn record(prompt: &str) -> TrajectoryRecord {
        TrajectoryRecord {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: Utc::now(),
            prompt: prompt.to_string(),
            git_ref: Some("0123456789abcdef0123456789abcdef01234567".to_string()),
            dirty: false,
            done_reason: "Completed".to_string(),
            duration_secs: 1.5,
            model: "test-model".to_string(),
            mode: "sovereign".to_string(),
        }
    }

    #[test]
    fn append_creates_the_log_and_never_truncates() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        assert!(
            read_trajectories(root)
                .expect("missing log reads")
                .is_empty(),
            "no log yet"
        );

        append(root, &record("first")).expect("first append creates .wizard/");
        append(root, &record("second")).expect("second append");
        let text = std::fs::read_to_string(trajectories_path(root)).expect("log exists");
        assert_eq!(text.lines().count(), 2, "one line per record: {text:?}");
        assert!(text.ends_with('\n'), "every line is terminated: {text:?}");

        let records = read_trajectories(root).expect("read back");
        let prompts: Vec<&str> = records.iter().map(|r| r.prompt.as_str()).collect();
        assert_eq!(prompts, vec!["first", "second"], "append order preserved");
    }

    #[test]
    fn a_corrupt_line_does_not_brick_the_log() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        append(root, &record("before")).expect("append");
        {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(trajectories_path(root))
                .expect("open log");
            file.write_all(b"{not json at all\n\n").expect("corrupt it");
        }
        append(root, &record("after")).expect("append past the corruption");

        let records = read_trajectories(root).expect("read back");
        let prompts: Vec<&str> = records.iter().map(|r| r.prompt.as_str()).collect();
        assert_eq!(
            prompts,
            vec!["before", "after"],
            "good records survive a corrupt line"
        );
    }
}
