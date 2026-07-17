//! Cross-process session registry.
//!
//! Every running Wizard TUI heartbeats a small JSON record to
//! `~/.wizard/running/<id>.json`, refreshed every few seconds. The
//! `/dashboard` reads the directory to list every live session on the machine
//! (Milestone 1 of the agent-view feature). Records whose last heartbeat is
//! older than [`STALE`] are treated as exited and pruned — a clean exit removes
//! its own file, and a crash ages out.

use std::path::{Path, PathBuf};
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
    write_to(&dir, record);
}

/// [`write`] into an explicit directory (the GUI's task manager holds its own
/// handle to it; tests use a temp dir).
pub(crate) fn write_to(dir: &Path, record: &SessionRecord) {
    if std::fs::create_dir_all(dir).is_err() {
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
        remove_from(&dir, id);
    }
}

/// [`remove`] from an explicit directory.
pub(crate) fn remove_from(dir: &Path, id: &str) {
    let _ = std::fs::remove_file(dir.join(format!("{id}.json")));
}

/// Every live session on the machine, sorted by state (needs-input first) then
/// most-recently-active. Stale records are skipped and their files pruned.
pub fn list() -> Vec<SessionRecord> {
    let Some(dir) = running_dir() else {
        return Vec::new();
    };
    list_from(&dir)
}

/// [`list`] from an explicit directory (tests use a temp dir).
pub(crate) fn list_from(dir: &Path) -> Vec<SessionRecord> {
    let Ok(entries) = std::fs::read_dir(dir) else {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Temp registry dir removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("wizard-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn record(id: &str, state: SessionState) -> SessionRecord {
        SessionRecord {
            id: id.to_string(),
            name: format!("session {id}"),
            cwd: "/tmp/project".to_string(),
            model: "test-model".to_string(),
            mode: "genie".to_string(),
            state,
            activity: "testing".to_string(),
            pid: 4242,
            started_unix: now_unix(),
            updated_unix: 0,
        }
    }

    #[test]
    fn write_then_list_round_trips_and_stamps_the_heartbeat() {
        let tmp = TempDir::new();
        write_to(&tmp.0, &record("a", SessionState::Working));

        let listed = list_from(&tmp.0);
        assert_eq!(listed.len(), 1);
        let got = &listed[0];
        assert_eq!(got.id, "a");
        assert_eq!(got.name, "session a");
        assert_eq!(got.cwd, "/tmp/project");
        assert_eq!(got.state, SessionState::Working);
        assert_eq!(got.pid, 4242);
        assert!(got.updated_unix > 0, "write stamps updated_unix");
        // No stray temp files remain.
        assert_eq!(std::fs::read_dir(&tmp.0).unwrap().count(), 1);
    }

    #[test]
    fn rewrites_refresh_in_place() {
        let tmp = TempDir::new();
        write_to(&tmp.0, &record("a", SessionState::Working));
        let mut updated = record("a", SessionState::Idle);
        updated.activity = "waiting".to_string();
        write_to(&tmp.0, &updated);

        let listed = list_from(&tmp.0);
        assert_eq!(listed.len(), 1, "one file per session id");
        assert_eq!(listed[0].state, SessionState::Idle);
        assert_eq!(listed[0].activity, "waiting");
    }

    #[test]
    fn stale_non_terminal_records_are_pruned_but_terminal_ones_are_retained() {
        let tmp = TempDir::new();
        // Backdate a working record beyond STALE_SECS (crashed process) and a
        // completed record inside RETAIN_SECS (finished background run).
        let stale = SessionRecord {
            updated_unix: now_unix() - STALE_SECS - 5,
            ..record("crashed", SessionState::Working)
        };
        let finished = SessionRecord {
            updated_unix: now_unix() - STALE_SECS - 5,
            ..record("done", SessionState::Completed)
        };
        for rec in [&stale, &finished] {
            std::fs::write(
                tmp.0.join(format!("{}.json", rec.id)),
                serde_json::to_vec(rec).unwrap(),
            )
            .unwrap();
        }

        let listed = list_from(&tmp.0);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "done");
        assert!(
            !tmp.0.join("crashed.json").exists(),
            "the stale record's file is pruned"
        );
    }

    #[test]
    fn list_sorts_needs_input_first_then_recency_and_skips_junk() {
        let tmp = TempDir::new();
        let now = now_unix();
        for (id, state, age) in [
            ("idle", SessionState::Idle, 2),
            ("blocked", SessionState::NeedsInput, 4),
            ("busy-old", SessionState::Working, 8),
            ("busy-new", SessionState::Working, 1),
        ] {
            let rec = SessionRecord {
                updated_unix: now - age,
                ..record(id, state)
            };
            std::fs::write(
                tmp.0.join(format!("{id}.json")),
                serde_json::to_vec(&rec).unwrap(),
            )
            .unwrap();
        }
        // Junk that must be ignored: wrong extension, corrupt JSON.
        std::fs::write(tmp.0.join("notes.txt"), "not a record").unwrap();
        std::fs::write(tmp.0.join("corrupt.json"), "{oops").unwrap();

        let ids: Vec<String> = list_from(&tmp.0).into_iter().map(|r| r.id).collect();
        assert_eq!(ids, ["blocked", "busy-new", "busy-old", "idle"]);
    }

    #[test]
    fn list_of_a_missing_dir_is_empty() {
        let tmp = TempDir::new();
        assert!(list_from(&tmp.0.join("missing")).is_empty());
    }

    #[test]
    fn terminal_records_age_out_after_the_retention_window() {
        let tmp = TempDir::new();
        let ancient = SessionRecord {
            updated_unix: now_unix() - RETAIN_SECS - 5,
            ..record("ancient", SessionState::Completed)
        };
        std::fs::write(
            tmp.0.join("ancient.json"),
            serde_json::to_vec(&ancient).unwrap(),
        )
        .unwrap();

        assert!(list_from(&tmp.0).is_empty());
        assert!(
            !tmp.0.join("ancient.json").exists(),
            "the expired record's file is pruned"
        );
    }
}
