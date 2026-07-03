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
