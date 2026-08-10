//! Cross-process session registry.
//!
//! Every running Wizard TUI heartbeats a small JSON record to
//! `~/.wizard/running/<id>.json`, refreshed every few seconds. The
//! `/dashboard` reads the directory to list every live session on the machine.
//! Records whose last heartbeat is
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

/* ---------------------------------------------------------------------- */
/* The chat picker                                                        */
/* ---------------------------------------------------------------------- */

// What a session *picker* shows.
//
// The three things below — merging the sessions on disk with the live
// registry, grouping the result by workspace, and saying how long ago a chat
// was touched — were once written only in the browser GUI's JavaScript, which
// re-derived in the page a merge the server had already done in Rust. Grouping
// is a property of the session store rather than of whatever draws it, and "2m"
// is a format three surfaces want. They live here, beside the registry they
// read, so the window and the TUI's `/resume` picker share them instead of each
// growing a copy — which is why deleting that page took nothing with it.

/// Which store a picker row came out of — and therefore what opening it *does*.
///
/// This is not decoration. Selecting a [`Origin::Wizard`] row reopens a file
/// Wizard owns; selecting a [`Origin::Claude`] row reads another program's
/// live state and writes a **new** Wizard session from it. A picker that drew
/// the two the same way would be offering one gesture for two different acts,
/// which is why every surface is handed the distinction rather than inferring
/// it from an id shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// A session under `~/.wizard/sessions/`. Opening it resumes it in place.
    Wizard,
    /// A Claude Code transcript under `~/.claude/projects/`, which Wizard only
    /// ever reads (see [`crate::claude_session`]). Opening it *imports*: the
    /// conversation is walked back from `leaf` and written as a new Wizard
    /// session, and the file below is left exactly as it was found.
    Claude {
        /// The transcript file — the only handle
        /// [`crate::claude_resume::import`] needs.
        path: PathBuf,
        /// The tip to walk the conversation back from: `last-prompt.leafUuid`,
        /// which is the branch Claude Code itself would resume.
        ///
        /// `None` for a file with no `last-prompt` line, where the import falls
        /// back to the newest leaf. Carried on the row rather than re-derived
        /// at open time so that what the count in the row describes and what
        /// the import replays cannot drift apart.
        leaf: Option<String>,
        /// How many places that file's history forked. Zero for a session that
        /// was never edited or rewound; above zero it is the reason the row's
        /// message count is smaller than the file.
        branch_points: usize,
    },
}

impl Origin {
    /// The one-word tag a surface shows and an API serializes.
    pub fn label(&self) -> &'static str {
        match self {
            Origin::Wizard => "wizard",
            Origin::Claude { .. } => "claude",
        }
    }

    /// True for a row whose file Wizard does not own.
    pub fn is_foreign(&self) -> bool {
        matches!(self, Origin::Claude { .. })
    }
}

/// One chat in a picker: a session on disk, plus whatever is running behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatRow {
    pub id: String,
    /// The label: the first prompt, or the workspace name until there is one.
    pub title: String,
    /// The workspace it runs in, as an absolute path.
    pub cwd: String,
    /// When the session file (or the heartbeat) last moved, in unix seconds.
    pub updated_unix: u64,
    /// What is running behind it, or `None` for a session on disk with no
    /// process — which is a real distinction and not a missing value: an idle
    /// live chat can be typed at *now*, a dormant one has to be re-opened.
    pub state: Option<SessionState>,
    /// Which store it came out of. See [`Origin`].
    pub origin: Origin,
}

/// The chats of one workspace, newest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    /// The absolute path, which is what identifies the group.
    pub path: String,
    /// Its display name: the basename.
    pub name: String,
    pub chats: Vec<ChatRow>,
}

/// A directory's display name: its basename, or the path itself when it has
/// none (`/`, or the empty string a session file with no header records).
pub fn workspace_name(cwd: &str) -> String {
    Path::new(cwd)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| cwd.to_string())
}

/// How long ago, in the picker's shorthand: `2m`, `3h`, `5d`.
///
/// Rounded rather than truncated, and floored at one minute, which is what the
/// browser sidebar did: a chat touched forty seconds ago reading `0m` looks
/// like a bug, and one touched ninety minutes ago reading `1h` is what a person
/// would say out loud. Takes `now` rather than reading the clock so the
/// rendering is a pure function and can be tested at a boundary instead of
/// near one.
pub fn relative_age(updated_unix: u64, now_unix: u64) -> String {
    let seconds = now_unix.saturating_sub(updated_unix);
    let minutes = ((seconds as f64) / 60.0).round().max(1.0) as u64;
    if minutes < 60 {
        return format!("{minutes}m");
    }
    let hours = ((minutes as f64) / 60.0).round() as u64;
    if hours < 24 {
        return format!("{hours}h");
    }
    format!("{}d", ((hours as f64) / 24.0).round() as u64)
}

/// Group `rows` by their workspace, newest chat first inside a group and the
/// group with the newest chat first overall.
///
/// Sorting the groups by their own newest chat, rather than alphabetically, is
/// what makes the sidebar usable without scrolling: the repository you were
/// just in is at the top because you were just in it.
pub fn group_by_workspace(mut rows: Vec<ChatRow>) -> Vec<Workspace> {
    rows.sort_by(|a, b| b.updated_unix.cmp(&a.updated_unix).then(a.id.cmp(&b.id)));
    let mut groups: Vec<Workspace> = Vec::new();
    for row in rows {
        match groups.iter_mut().find(|group| group.path == row.cwd) {
            Some(group) => group.chats.push(row),
            None => groups.push(Workspace {
                name: workspace_name(&row.cwd),
                path: row.cwd.clone(),
                chats: vec![row],
            }),
        }
    }
    groups
}

/// Every chat this machine knows about: the sessions in `sessions_dir`, the
/// heartbeats in `running_dir`, and `live` — the states of the tasks *this*
/// process is running, which are fresher than either.
///
/// A heartbeat with no session file still lists (a Wizard configured against
/// another sessions directory), so nothing running on the machine is invisible.
pub fn chats_in(
    sessions_dir: &Path,
    running_dir: &Path,
    live: &std::collections::HashMap<String, SessionState>,
) -> Vec<ChatRow> {
    let registry: Vec<SessionRecord> = list_from(running_dir);
    let mut rows: Vec<ChatRow> = Vec::new();
    for summary in crate::agent::session::summaries(sessions_dir) {
        let heartbeat = registry.iter().find(|record| record.id == summary.id);
        let state = live
            .get(&summary.id)
            .copied()
            .or_else(|| heartbeat.map(|record| record.state));
        let cwd = summary.cwd.clone().unwrap_or_default();
        let updated = mtime_unix(&sessions_dir.join(format!("{}.jsonl", summary.id)))
            .or_else(|| heartbeat.map(|record| record.updated_unix))
            .unwrap_or(0);
        rows.push(ChatRow {
            id: summary.id,
            title: summary.summary,
            cwd,
            updated_unix: updated,
            state,
            origin: Origin::Wizard,
        });
    }
    for record in registry {
        if rows.iter().any(|row| row.id == record.id) {
            continue;
        }
        rows.push(ChatRow {
            state: Some(live.get(&record.id).copied().unwrap_or(record.state)),
            id: record.id,
            title: record.name,
            cwd: record.cwd,
            updated_unix: record.updated_unix,
            origin: Origin::Wizard,
        });
    }
    rows.sort_by(|a, b| b.updated_unix.cmp(&a.updated_unix).then(a.id.cmp(&b.id)));
    rows
}

/// [`chats_in`] against this machine's own directories.
pub fn chats(live: &std::collections::HashMap<String, SessionState>) -> Vec<ChatRow> {
    let (Ok(sessions), Some(running)) = (Config::sessions_dir(), running_dir()) else {
        return Vec::new();
    };
    chats_in(&sessions, &running, live)
}

/* ---------------------------------------------------------------------- */
/* The other program's sessions                                           */
/* ---------------------------------------------------------------------- */

// Claude Code's transcripts belong in the same picker as Wizard's own, which
// means "list the Claude sessions for this workspace" is a fact about the
// session stores rather than about a window — the same reason the merge, the
// grouping and the `2m` are here. `wizard resume --claude` asks
// `claude_session` directly because it is a one-shot command with a `--cwd`
// already applied; the two graphical surfaces ask through here, so that what a
// row *is* (and what its provenance means) is decided once.
//
// The parse is not free and the split below is the whole point: `claude_here`
// is a directory probe cheap enough to ride a five-second refresh, and
// `claude_chats` reads and parses every transcript in the project — tens of
// megabytes for a heavily used repository. Surfaces call the first on their
// timer and the second only when a person asks for the list.

/// Whether Claude Code has recorded anything against `cwd` on this machine.
///
/// One `is_dir` in the common case (see
/// [`project_dir`](crate::claude_session::project_dir)), and `false` — never an
/// error — when Claude Code is not installed at all, which is what most
/// machines look like. Cheap enough to answer on every refresh, which is what
/// lets a picker hide the whole section rather than offer an empty one.
pub fn claude_here(cwd: &str) -> bool {
    crate::import_claude::claude_projects_dir()
        .is_some_and(|root| crate::claude_session::project_dir(&root, cwd).is_some())
}

/// Picker rows for every Claude Code session recorded against `cwd`, newest
/// first.
///
/// **Expensive**: every transcript in the project is read and parsed, because a
/// row's title and its message count are both properties of the conversation
/// DAG rather than of the file's first line. Callers put this behind a
/// deliberate act — expanding a section, hitting a route — never behind a
/// timer. [`claude_here`] is the cheap question.
///
/// Read-only, structurally: everything below is
/// [`crate::claude_session`], whose own tests fail the build if a write API is
/// so much as named in it.
pub fn claude_chats_in(projects_root: &Path, cwd: &str) -> Vec<ChatRow> {
    crate::claude_session::list_sessions(projects_root, cwd)
        .into_iter()
        .map(|preview| ChatRow {
            // The timestamps inside the file, falling back to the file itself
            // for a transcript whose lines carry none.
            updated_unix: preview
                .updated
                .map(|stamp| stamp.timestamp().max(0) as u64)
                .or_else(|| mtime_unix(&preview.path))
                .unwrap_or(0),
            title: preview.title,
            // The directory the row was listed *for*, not the one recorded in
            // the file. They agree except when a project directory had to be
            // matched by prefix, and it is the listing directory that decides
            // both which group the row lands in and where the imported session
            // will run.
            cwd: cwd.to_string(),
            id: preview.session_id,
            // Nothing in this process is running it: it is a file, until it is
            // imported.
            state: None,
            origin: Origin::Claude {
                path: preview.path,
                leaf: preview.leaf_uuid,
                branch_points: preview.branch_points,
            },
        })
        .collect()
}

/// [`claude_chats_in`] against this machine's `~/.claude/projects`. Empty when
/// Claude Code is not installed.
pub fn claude_chats(cwd: &str) -> Vec<ChatRow> {
    match crate::import_claude::claude_projects_dir() {
        Some(root) => claude_chats_in(&root, cwd),
        None => Vec::new(),
    }
}

/// A file's mtime as unix seconds.
fn mtime_unix(path: &Path) -> Option<u64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(modified.duration_since(UNIX_EPOCH).ok()?.as_secs())
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

    /* ------------------------------------------------------------------ */
    /* The chat picker                                                    */
    /* ------------------------------------------------------------------ */

    fn chat(id: &str, cwd: &str, updated: u64) -> ChatRow {
        ChatRow {
            id: id.to_string(),
            title: format!("chat {id}"),
            cwd: cwd.to_string(),
            updated_unix: updated,
            state: None,
            origin: Origin::Wizard,
        }
    }

    /// The exact ladder every sidebar uses, at the boundary of each
    /// tier. Rounding rather than truncating is the part that matters: a chat
    /// touched forty seconds ago must not read `0m`, and ninety minutes must
    /// read `2h`, because that is what a person says.
    #[test]
    fn relative_ages_round_and_floor_at_a_minute() {
        let now = 1_000_000;
        let ago = |seconds: u64| relative_age(now - seconds, now);
        assert_eq!(ago(0), "1m", "newer than a minute still reads 1m");
        assert_eq!(ago(40), "1m");
        assert_eq!(ago(90), "2m", "rounded, not truncated");
        assert_eq!(ago(59 * 60), "59m");
        assert_eq!(ago(60 * 60), "1h");
        assert_eq!(ago(90 * 60), "2h", "ninety minutes is two hours, rounded");
        assert_eq!(ago(23 * 3600), "23h");
        assert_eq!(ago(24 * 3600), "1d");
        assert_eq!(ago(36 * 3600), "2d");
    }

    /// A clock that ran backwards (a session file stamped in the future by a
    /// skewed NFS mount) must not produce an enormous number from an
    /// underflow. `saturating_sub` plus the one-minute floor is what stops it.
    #[test]
    fn a_future_timestamp_reads_as_a_minute_rather_than_underflowing() {
        assert_eq!(relative_age(2_000, 1_000), "1m");
    }

    #[test]
    fn a_workspace_name_is_its_basename_and_falls_back_to_the_path() {
        assert_eq!(workspace_name("/home/user/projects/wizard"), "wizard");
        assert_eq!(workspace_name("/"), "/");
        assert_eq!(workspace_name(""), "");
    }

    /// The sidebar's whole ordering contract: newest chat first inside a
    /// workspace, and the workspace you were last in at the top. Alphabetical
    /// groups would put the repo you are working in below one you have not
    /// opened in a month.
    #[test]
    fn workspaces_are_ordered_by_their_own_newest_chat() {
        let groups = group_by_workspace(vec![
            chat("old", "/src/alpha", 100),
            chat("newest", "/src/beta", 900),
            chat("mid", "/src/alpha", 500),
            chat("older", "/src/beta", 200),
        ]);
        let names: Vec<&str> = groups.iter().map(|group| group.name.as_str()).collect();
        assert_eq!(names, ["beta", "alpha"]);
        let beta: Vec<&str> = groups[0].chats.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(beta, ["newest", "older"]);
        let alpha: Vec<&str> = groups[1].chats.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(alpha, ["mid", "old"]);
    }

    /// Two chats stamped in the same second must still order deterministically,
    /// or the sidebar reshuffles itself on every refresh.
    #[test]
    fn chats_stamped_in_the_same_second_order_by_id() {
        let groups =
            group_by_workspace(vec![chat("b", "/src/one", 500), chat("a", "/src/one", 500)]);
        let ids: Vec<&str> = groups[0].chats.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["a", "b"]);
    }

    /// A heartbeat with no session file is still a chat: it is a Wizard
    /// running on this machine, and a picker that hid it would make "where did
    /// my other window go" unanswerable.
    #[test]
    fn a_heartbeat_without_a_session_file_still_lists() {
        let tmp = TempDir::new();
        let sessions = tmp.0.join("sessions");
        std::fs::create_dir_all(&sessions).expect("sessions dir");
        write_to(&tmp.0, &record("orphan", SessionState::Working));

        let rows = chats_in(&sessions, &tmp.0, &std::collections::HashMap::new());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "orphan");
        assert_eq!(rows[0].state, Some(SessionState::Working));
        assert_eq!(rows[0].cwd, "/tmp/project");
    }

    /// This process's own view of a task wins over the heartbeat on disk: the
    /// heartbeat is up to three seconds old, and the manager knows *now*.
    ///
    /// Asserted for a chat that has **both** a session file and a heartbeat,
    /// which is the ordinary case and the one that goes down the merge path; a
    /// heartbeat with no file takes a different branch and would not exercise
    /// the precedence at all.
    #[test]
    fn a_live_state_overrides_the_heartbeat_on_disk() {
        let tmp = TempDir::new();
        let sessions = tmp.0.join("sessions");
        std::fs::create_dir_all(&sessions).expect("sessions dir");
        let session = crate::agent::session::Session::create_in(&sessions, Path::new("/src/thing"))
            .expect("create session");
        session
            .append(&crate::llm::ChatMessage::user("make it faster"))
            .expect("append a message");
        write_to(
            &tmp.0,
            &SessionRecord {
                id: session.id.clone(),
                cwd: "/src/thing".to_string(),
                ..record(&session.id, SessionState::Idle)
            },
        );

        // The heartbeat alone says idle.
        let rows = chats_in(&sessions, &tmp.0, &std::collections::HashMap::new());
        assert_eq!(rows.len(), 1, "one chat, not one per source: {rows:?}");
        assert_eq!(rows[0].state, Some(SessionState::Idle));

        // This process knows better.
        let mut live = std::collections::HashMap::new();
        live.insert(session.id.clone(), SessionState::Working);
        let rows = chats_in(&sessions, &tmp.0, &live);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, Some(SessionState::Working));
    }

    /// A session on disk that nothing is running is `None`, not `Idle`. The
    /// distinction is real: an idle live chat can be typed at now, a dormant
    /// one has to be re-opened first.
    #[test]
    fn a_dormant_session_has_no_state() {
        let tmp = TempDir::new();
        let sessions = tmp.0.join("sessions");
        std::fs::create_dir_all(&sessions).expect("sessions dir");
        let session = crate::agent::session::Session::create_in(&sessions, Path::new("/src/thing"))
            .expect("create session");
        session
            .append(&crate::llm::ChatMessage::user("make it faster"))
            .expect("append a message");

        let rows = chats_in(&sessions, &tmp.0, &std::collections::HashMap::new());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, None);
        assert_eq!(rows[0].cwd, "/src/thing");
        assert!(rows[0].updated_unix > 0, "the session file's mtime is used");
        assert_eq!(
            rows[0].origin,
            Origin::Wizard,
            "a session Wizard wrote is Wizard's"
        );
    }

    /* ------------------------------------------------------------------ */
    /* Claude Code's sessions                                             */
    /* ------------------------------------------------------------------ */

    fn claude_fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/claude_sessions")
            .join(name)
    }

    /// A `~/.claude/projects`-shaped tree holding both fixtures under `cwd`'s
    /// slug, returned with the temp dir that owns it.
    fn claude_tree(cwd: &str) -> (TempDir, PathBuf) {
        let tmp = TempDir::new();
        let root = tmp.0.join("projects");
        let dir = root.join(crate::claude_session::project_slug(cwd).name);
        std::fs::create_dir_all(&dir).expect("project dir");
        for name in ["linear.jsonl", "branched.jsonl"] {
            std::fs::copy(claude_fixture(name), dir.join(name)).expect("copy fixture");
        }
        (tmp, root)
    }

    /// The listing carries provenance and, with it, the two things an import
    /// needs: which file, and which leaf of that file's DAG. A row that lost
    /// either would leave the surface guessing at open time.
    #[test]
    fn a_claude_row_carries_its_file_and_the_leaf_to_resume_from() {
        let cwd = "/src/demo";
        let (_tmp, root) = claude_tree(cwd);

        let rows = claude_chats_in(&root, cwd);
        assert_eq!(rows.len(), 2, "both fixtures list: {rows:?}");
        for row in &rows {
            assert_eq!(row.cwd, cwd, "listed for the workspace, not the file's");
            assert_eq!(row.state, None, "a file is not a running process");
            assert!(row.updated_unix > 0, "the transcript is timestamped");
            let Origin::Claude {
                path,
                leaf,
                branch_points,
            } = &row.origin
            else {
                panic!("a Claude transcript is not a Wizard session: {row:?}");
            };
            assert!(path.is_file(), "{}", path.display());
            let session = crate::claude_session::ClaudeSession::load(path).expect("load");
            assert_eq!(
                leaf.as_deref(),
                session.tip(),
                "the row names the tip Claude Code itself would resume"
            );
            assert_eq!(*branch_points, session.branch_points().len());
        }

        // The branched fixture is the one that forked, and saying so is the
        // only warning a user gets that the row is one conversation out of
        // several in that file.
        let forked: Vec<usize> = rows
            .iter()
            .map(|row| match &row.origin {
                Origin::Claude { branch_points, .. } => *branch_points,
                Origin::Wizard => 0,
            })
            .collect();
        assert!(forked.contains(&1), "the branched fixture forks once");
        assert!(forked.contains(&0), "the linear one does not");
    }

    /// The common case: no Claude Code on the machine, or none for this
    /// directory. A picker asks this on its refresh timer, so it must be an
    /// empty answer rather than an error or a panic.
    #[test]
    fn a_workspace_claude_code_never_saw_lists_nothing() {
        let tmp = TempDir::new();
        assert!(claude_chats_in(&tmp.0, "/src/never-opened").is_empty());
        // And a projects root that does not exist at all.
        assert!(claude_chats_in(&tmp.0.join("missing"), "/src/demo").is_empty());
    }

    /// The pair a picker actually calls, against whatever this machine has.
    ///
    /// Claude Code may or may not be installed here, and that is the point: a
    /// directory nothing was ever run in has nothing recorded against it
    /// either way, and asking must be a cheap `false` and an empty list rather
    /// than an error or a dependency on another program being present.
    #[test]
    fn probing_this_machine_for_a_directory_nothing_ran_in_is_quiet() {
        let tmp = TempDir::new();
        let cwd = tmp.0.display().to_string();
        assert!(!claude_here(&cwd));
        assert!(claude_chats(&cwd).is_empty());
    }

    /// Provenance survives the grouping, which is what puts the two kinds of
    /// row into one list without making them look alike.
    #[test]
    fn claude_rows_group_beside_wizard_rows_and_keep_their_origin() {
        let cwd = "/src/demo";
        let (_tmp, root) = claude_tree(cwd);
        let mut rows = vec![chat("wizard-one", cwd, u64::MAX)];
        rows.extend(claude_chats_in(&root, cwd));

        let groups = group_by_workspace(rows);
        assert_eq!(groups.len(), 1, "one workspace, both stores");
        assert_eq!(groups[0].name, "demo");
        assert_eq!(groups[0].chats.len(), 3);
        assert_eq!(groups[0].chats[0].origin.label(), "wizard");
        assert!(!groups[0].chats[0].origin.is_foreign());
        assert!(
            groups[0].chats[1..]
                .iter()
                .all(|row| row.origin.label() == "claude" && row.origin.is_foreign()),
            "{:?}",
            groups[0].chats
        );
    }

    /// Listing another program's live state must not touch a byte of it. The
    /// source-level guard lives in `crate::claude_session`; this is the one
    /// that covers *this* call site.
    #[test]
    fn listing_claude_sessions_leaves_the_tree_untouched() {
        let cwd = "/src/demo";
        let (_tmp, root) = claude_tree(cwd);

        let before = crate::claude_session::tests_support::snapshot(&root);
        assert_eq!(before.len(), 2);
        assert_eq!(claude_chats_in(&root, cwd).len(), 2);
        assert_eq!(
            crate::claude_session::tests_support::snapshot(&root),
            before,
            "listing a Claude Code project directory must not change it"
        );
    }
}
