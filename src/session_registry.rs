//! Cross-process session registry.
//!
//! Every running Wizard TUI heartbeats a small JSON record to
//! `~/.wizard/running/<id>.json`, refreshed every few seconds. The
//! `/dashboard` reads the directory to list every live session on the machine
//! (Milestone 1 of the agent-view feature). Records whose last heartbeat is
//! older than [`STALE`] are treated as exited and pruned — a clean exit removes
//! its own file, and a crash ages out.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::Config;

/// A non-terminal session whose heartbeat is older than this is considered
/// gone (its process died without cleaning up).
pub const STALE_SECS: u64 = 12;

/// Terminal records (completed/failed background sessions) are kept this long
/// so the result stays visible in the dashboard, then aged out.
pub const RETAIN_SECS: u64 = 24 * 60 * 60;

/// What a session is currently doing, for the dashboard's grouping and icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// Actively running tools or generating a response.
    Working,
    /// Paused waiting on the user (plan approval, a gate).
    NeedsInput,
    /// Nothing to do; ready for the next prompt.
    Idle,
    /// A background/autonomous run finished successfully.
    Completed,
    /// A background/autonomous run ended with an error.
    Failed,
}

impl SessionState {
    /// Dashboard group header this state sorts under.
    pub fn group(self) -> &'static str {
        match self {
            SessionState::NeedsInput => "Needs input",
            SessionState::Working => "Working",
            SessionState::Idle => "Idle",
            SessionState::Completed | SessionState::Failed => "Completed",
        }
    }

    /// A finished background session (no live process behind it).
    pub fn is_terminal(self) -> bool {
        matches!(self, SessionState::Completed | SessionState::Failed)
    }

    /// Sort key: the ones that need you first, finished last.
    pub fn order(self) -> u8 {
        match self {
            SessionState::NeedsInput => 0,
            SessionState::Working => 1,
            SessionState::Idle => 2,
            SessionState::Completed => 3,
            SessionState::Failed => 4,
        }
    }
}

/// One running session's heartbeat record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    /// Session id (also the heartbeat filename stem).
    pub id: String,
    /// Human label, derived from the first prompt (or the id).
    pub name: String,
    /// Working directory the session runs in.
    pub cwd: String,
    pub model: String,
    /// `"genie"` or `"sovereign"`.
    pub mode: String,
    pub state: SessionState,
    /// One-line summary of what the session is doing.
    pub activity: String,
    /// OS process id, for later attach/stop.
    pub pid: u32,
    pub started_unix: u64,
    pub updated_unix: u64,
}

/// `~/.wizard/running/` — heartbeat files live here.
pub fn running_dir() -> Option<PathBuf> {
    Config::wizard_dir().ok().map(|dir| dir.join("running"))
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Write (or refresh) `record`'s heartbeat, stamping `updated_unix` to now.
/// Best-effort: a registry write failure must never take down the session, so
/// errors are logged and dropped.
pub fn write(record: &SessionRecord) {
    let Some(dir) = running_dir() else { return };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let record = SessionRecord {
        updated_unix: now_unix(),
        ..record.clone()
    };
    let path = dir.join(format!("{}.json", record.id));
    let tmp = dir.join(format!(".{}.tmp", record.id));
    match serde_json::to_vec(&record) {
        Ok(bytes) => {
            // Write to a temp file and rename so a reader never sees a partial
            // record.
            if std::fs::write(&tmp, &bytes).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
        Err(err) => tracing::warn!("serializing session record: {err}"),
    }
}

/// Remove a session's heartbeat (called on clean exit).
pub fn remove(id: &str) {
    if let Some(dir) = running_dir() {
        let _ = std::fs::remove_file(dir.join(format!("{id}.json")));
    }
}

/// Every live session on the machine, sorted by state (needs-input first) then
/// most-recently-active. Stale records are skipped and their files pruned.
pub fn list() -> Vec<SessionRecord> {
    let Some(dir) = running_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let now = now_unix();
    let mut records: Vec<SessionRecord> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(record) = serde_json::from_str::<SessionRecord>(&raw) else {
            continue;
        };
        let age = now.saturating_sub(record.updated_unix);
        // Terminal records (finished background runs) persist so their result
        // stays visible, then age out after RETAIN_SECS. A non-terminal record
        // older than STALE_SECS means its process died without cleaning up.
        let expired = if record.state.is_terminal() {
            age > RETAIN_SECS
        } else {
            age > STALE_SECS
        };
        if expired {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        records.push(record);
    }
    records.sort_by(|a, b| {
        a.state
            .order()
            .cmp(&b.state.order())
            .then(b.updated_unix.cmp(&a.updated_unix))
    });
    records
}
