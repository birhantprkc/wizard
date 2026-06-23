//! JSONL session persistence under `~/.wizard/sessions/<timestamp>.jsonl`.
//! One [`SessionRecord`] per line, appended after each message lands.
//! Turn boundaries are marked by interleaved [`TurnMarker`] lines so
//! `/rewind` can truncate history at a turn; files without markers (older
//! sessions) still load.

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::llm::ChatMessage;

/// One line of a session file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub timestamp: DateTime<Utc>,
    pub message: ChatMessage,
}

/// A turn-boundary line, written just before the turn's user message. Anchors
/// [`Session::truncate_after`] and labels the `/rewind` picker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnMarker {
    pub timestamp: DateTime<Utc>,
    /// Checkpoint turn id (monotonic per project — see
    /// [`crate::checkpoint::CheckpointStore::begin_turn`]).
    pub turn: u64,
    /// First line of the user prompt that started the turn (truncated).
    #[serde(default)]
    pub prompt: String,
}

/// Either kind of session line. Untagged: a marker has a `turn` field, a
/// message record has `message` — old files (messages only) parse unchanged.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum SessionLine {
    Marker(TurnMarker),
    Message(SessionRecord),
}

/// Cap on the prompt snippet stored in a [`TurnMarker`].
const MARKER_PROMPT_CHARS: usize = 120;

/// Read the last `n` message lines of session `id` for the dashboard peek
/// panel, as `(role, text)` pairs in file order. Only the tail of the file is
/// read (bounded by [`PEEK_TAIL_BYTES`]) so a huge transcript stays cheap to
/// poll, and each message's text is clipped to [`PEEK_MSG_CHARS`]. Best-effort:
/// any error (no such session, unreadable, corrupt) yields an empty vec.
pub fn peek(id: &str, n: usize) -> Vec<(String, String)> {
    use std::io::{Read, Seek, SeekFrom};

    /// Read at most this many bytes from the end of the session file.
    const PEEK_TAIL_BYTES: u64 = 96 * 1024;
    /// Clip each message's text so one huge tool result can't dominate.
    const PEEK_MSG_CHARS: usize = 4000;

    let Ok(dir) = crate::config::Config::sessions_dir() else {
        return Vec::new();
    };
    let Ok(mut file) = std::fs::File::open(dir.join(format!("{id}.jsonl"))) else {
        return Vec::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let seeked = len > PEEK_TAIL_BYTES;
    if seeked && file.seek(SeekFrom::Start(len - PEEK_TAIL_BYTES)).is_err() {
        return Vec::new();
    }
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        return Vec::new();
    }
    // The seek may land mid-codepoint; lossily decode and drop the first
    // (partial) line.
    let text = String::from_utf8_lossy(&bytes);
    let mut messages = Vec::new();
    let mut lines = text.lines();
    if seeked {
        lines.next();
    }
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(SessionLine::Message(record)) = serde_json::from_str::<SessionLine>(line) {
            let role = format!("{:?}", record.message.role).to_lowercase();
            let mut content = record.message.content;
            if content.chars().count() > PEEK_MSG_CHARS {
                content = content.chars().take(PEEK_MSG_CHARS).collect::<String>() + " …";
            }
            messages.push((role, content));
        }
    }
    let start = messages.len().saturating_sub(n);
    messages.split_off(start)
}

/// One past session, summarized for the `/resume` picker.
#[derive(Debug, Clone)]
pub struct SessionSummary {
    /// Session id (filename stem), used to reopen the file.
    pub id: String,
    /// First user prompt (or turn-marker snippet), for the picker label.
    pub summary: String,
    /// Number of message records (turn markers excluded).
    pub messages: usize,
    /// File mtime as unix seconds (0 if unavailable), for the "… ago" label.
    pub updated_unix: u64,
}

/// List past sessions in `dir`, newest id first, skipping empty ones (no
/// message records). Best-effort: unreadable or corrupt files are dropped.
/// Each file is scanned once for its first prompt and message count.
pub fn summaries(dir: &Path) -> Vec<SessionSummary> {
    if !dir.is_dir() {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "jsonl") {
            continue;
        }
        let Some(id) = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
        else {
            continue;
        };
        let Ok(file) = std::fs::File::open(&path) else {
            continue;
        };
        let mut messages = 0usize;
        let mut marker_prompt: Option<String> = None;
        let mut first_user: Option<String> = None;
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<SessionLine>(&line) {
                Ok(SessionLine::Message(record)) => {
                    messages += 1;
                    if first_user.is_none() && record.message.role == crate::llm::Role::User {
                        first_user = record.message.content.lines().next().map(str::to_string);
                    }
                }
                Ok(SessionLine::Marker(marker)) => {
                    if marker_prompt.is_none() && !marker.prompt.is_empty() {
                        marker_prompt = Some(marker.prompt);
                    }
                }
                Err(_) => {}
            }
        }
        if messages == 0 {
            continue;
        }
        let summary = marker_prompt
            .or(first_user)
            .unwrap_or_else(|| "(no prompt)".to_string());
        let updated_unix = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|dur| dur.as_secs())
            .unwrap_or(0);
        out.push(SessionSummary {
            id,
            summary,
            messages,
            updated_unix,
        });
    }
    // Ids are zero-padded timestamps, so lexical order is chronological.
    out.sort_by(|a, b| b.id.cmp(&a.id));
    out
}

/// Handle to one session file. Append-only; cheap to clone the path out of.
#[derive(Debug, Clone)]
pub struct Session {
    /// Session id (the filename stem, e.g. `2026-06-09T13-45-02`).
    pub id: String,
    path: PathBuf,
}

impl Session {
    /// Create a new session file `<dir>/<timestamp>.jsonl`, creating `dir`
    /// if needed.
    pub fn create(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let id = Utc::now().format("%Y-%m-%dT%H-%M-%S").to_string();
        let path = dir.join(format!("{id}.jsonl"));
        std::fs::File::create(&path).with_context(|| format!("creating {}", path.display()))?;
        Ok(Self { id, path })
    }

    /// Open the most recent session in `dir` (for `--resume`). `None` when
    /// no sessions exist.
    pub fn open_latest(dir: &Path) -> Result<Option<Self>> {
        if !dir.is_dir() {
            return Ok(None);
        }
        let mut latest: Option<PathBuf> = None;
        for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
            let path = entry?.path();
            if path.extension().is_some_and(|ext| ext == "jsonl")
                && latest.as_ref().is_none_or(|cur| &path > cur)
            {
                latest = Some(path);
            }
        }
        Ok(latest.map(|path| {
            let id = path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default();
            Self { id, path }
        }))
    }

    /// Open a specific session by id (its filename stem), for `/resume`.
    /// `None` when `id` is empty or no such file exists.
    pub fn open_by_id(dir: &Path, id: &str) -> Result<Option<Self>> {
        if id.is_empty() {
            return Ok(None);
        }
        let path = dir.join(format!("{id}.jsonl"));
        Ok(path.is_file().then(|| Self {
            id: id.to_string(),
            path,
        }))
    }

    /// File this session persists to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one message as a JSONL record.
    pub fn append(&self, message: &ChatMessage) -> Result<()> {
        let record = SessionRecord {
            timestamp: Utc::now(),
            message: message.clone(),
        };
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("opening {}", self.path.display()))?;
        let line = serde_json::to_string(&record).context("serializing session record")?;
        writeln!(file, "{line}").with_context(|| format!("writing {}", self.path.display()))?;
        Ok(())
    }

    /// Append a turn-boundary marker. `prompt` is reduced to its first line,
    /// capped at [`MARKER_PROMPT_CHARS`] characters.
    pub fn append_marker(&self, turn: u64, prompt: &str) -> Result<()> {
        let snippet: String = prompt
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .take(MARKER_PROMPT_CHARS)
            .collect();
        let marker = TurnMarker {
            timestamp: Utc::now(),
            turn,
            prompt: snippet,
        };
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("opening {}", self.path.display()))?;
        let line = serde_json::to_string(&marker).context("serializing turn marker")?;
        writeln!(file, "{line}").with_context(|| format!("writing {}", self.path.display()))?;
        Ok(())
    }

    /// Load all messages back (for `--resume`). Turn markers and corrupt
    /// lines are skipped.
    pub fn load_messages(&self) -> Result<Vec<ChatMessage>> {
        let file = std::fs::File::open(&self.path)
            .with_context(|| format!("opening {}", self.path.display()))?;
        let mut messages = Vec::new();
        for line in BufReader::new(file).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<SessionLine>(&line) {
                Ok(SessionLine::Message(record)) => messages.push(record.message),
                Ok(SessionLine::Marker(_)) => {}
                Err(err) => tracing::warn!("skipping corrupt session line: {err}"),
            }
        }
        Ok(messages)
    }

    /// All turn markers in this session, in file order. Old-format files
    /// (no markers) yield an empty vec.
    pub fn turn_markers(&self) -> Result<Vec<TurnMarker>> {
        let file = std::fs::File::open(&self.path)
            .with_context(|| format!("opening {}", self.path.display()))?;
        let mut markers = Vec::new();
        for line in BufReader::new(file).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(SessionLine::Marker(marker)) = serde_json::from_str::<SessionLine>(&line) {
                markers.push(marker);
            }
        }
        Ok(markers)
    }

    /// Drop turn `turn_id` and everything after it: the file is cut at the
    /// first marker with `turn >= turn_id` (`>=` so a rewind survives gaps
    /// in the marker sequence). Old-format files without markers are left
    /// unchanged. Returns true when the file was truncated.
    pub fn truncate_after(&self, turn_id: u64) -> Result<bool> {
        let raw = std::fs::read_to_string(&self.path)
            .with_context(|| format!("reading {}", self.path.display()))?;
        let lines: Vec<&str> = raw.lines().collect();
        let cut = lines.iter().position(|line| {
            matches!(
                serde_json::from_str::<SessionLine>(line),
                Ok(SessionLine::Marker(marker)) if marker.turn >= turn_id
            )
        });
        let Some(cut) = cut else {
            return Ok(false);
        };
        let mut kept = lines[..cut].join("\n");
        if !kept.is_empty() {
            kept.push('\n');
        }
        std::fs::write(&self.path, kept)
            .with_context(|| format!("rewriting {}", self.path.display()))?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::llm::{FunctionCall, Role, ToolCall};

    /// Temp sessions dir removed on drop.
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

    #[test]
    fn round_trips_messages_including_tool_calls() {
        let tmp = TempDir::new();
        let session = Session::create(&tmp.0).unwrap();

        let mut assistant = ChatMessage::assistant("I'll read that file.");
        assistant.tool_calls.push(ToolCall {
            function: FunctionCall {
                name: "read_file".to_string(),
                arguments: json!({ "path": "src/main.rs" }),
            },
        });
        let messages = [
            ChatMessage::system("You are Wizard."),
            ChatMessage::user("read main.rs"),
            assistant,
            ChatMessage::tool_result("read_file", "fn main() {}"),
        ];
        for message in &messages {
            session.append(message).unwrap();
        }

        let loaded = session.load_messages().unwrap();
        assert_eq!(loaded.len(), 4);
        assert_eq!(loaded[0].role, Role::System);
        assert_eq!(loaded[1].content, "read main.rs");
        assert_eq!(loaded[2].tool_calls.len(), 1);
        assert_eq!(loaded[2].tool_calls[0].function.name, "read_file");
        assert_eq!(
            loaded[2].tool_calls[0].function.arguments["path"],
            "src/main.rs"
        );
        assert_eq!(loaded[3].role, Role::Tool);
        assert_eq!(loaded[3].tool_name.as_deref(), Some("read_file"));
        assert_eq!(loaded[3].content, "fn main() {}");
    }

    #[test]
    fn save_then_load_round_trips_through_a_fresh_handle() {
        let dir = tempfile::tempdir().unwrap();
        let session = Session::create(dir.path()).unwrap();
        let messages = [
            ChatMessage::user("hello"),
            ChatMessage::assistant("hi there"),
            ChatMessage::tool_result("git_status", "clean"),
        ];
        for message in &messages {
            session.append(message).unwrap();
        }

        // Reload through open_latest, as `--resume` would.
        let reopened = Session::open_latest(dir.path())
            .unwrap()
            .expect("session exists");
        assert_eq!(reopened.id, session.id);
        let loaded = reopened.load_messages().unwrap();
        assert_eq!(loaded.len(), messages.len());
        for (loaded, original) in loaded.iter().zip(&messages) {
            assert_eq!(loaded.role, original.role);
            assert_eq!(loaded.content, original.content);
            assert_eq!(loaded.tool_name, original.tool_name);
        }
    }

    #[test]
    fn corrupt_and_blank_lines_are_skipped() {
        let tmp = TempDir::new();
        let session = Session::create(&tmp.0).unwrap();
        session.append(&ChatMessage::user("first")).unwrap();

        // Simulate a crash mid-write plus stray whitespace.
        let mut file = OpenOptions::new()
            .append(true)
            .open(session.path())
            .unwrap();
        writeln!(file, "{{\"timestamp\":\"2026-").unwrap();
        writeln!(file).unwrap();
        drop(file);
        session.append(&ChatMessage::user("second")).unwrap();

        let loaded = session.load_messages().unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].content, "first");
        assert_eq!(loaded[1].content, "second");
    }

    #[test]
    fn create_names_file_after_id() {
        let tmp = TempDir::new();
        let session = Session::create(&tmp.0).unwrap();
        assert!(session.path().exists());
        assert_eq!(
            session.path().file_name().unwrap().to_string_lossy(),
            format!("{}.jsonl", session.id)
        );
        assert!(
            session.load_messages().unwrap().is_empty(),
            "new session is empty"
        );
    }

    #[test]
    fn open_latest_picks_the_newest_session() {
        let tmp = TempDir::new();
        std::fs::write(tmp.0.join("2026-06-08T10-00-00.jsonl"), "").unwrap();
        std::fs::write(tmp.0.join("2026-06-09T09-30-00.jsonl"), "").unwrap();
        std::fs::write(tmp.0.join("notes.txt"), "not a session").unwrap();

        let latest = Session::open_latest(&tmp.0)
            .unwrap()
            .expect("a session exists");
        assert_eq!(latest.id, "2026-06-09T09-30-00");
    }

    #[test]
    fn turn_markers_are_skipped_by_load_and_listed_separately() {
        let tmp = TempDir::new();
        let session = Session::create(&tmp.0).unwrap();
        session
            .append_marker(1, "first prompt\nsecond line ignored")
            .unwrap();
        session.append(&ChatMessage::user("first prompt")).unwrap();
        session.append(&ChatMessage::assistant("reply")).unwrap();
        session.append_marker(2, "second prompt").unwrap();
        session.append(&ChatMessage::user("second prompt")).unwrap();

        let messages = session.load_messages().unwrap();
        assert_eq!(messages.len(), 3, "markers are not messages");
        let markers = session.turn_markers().unwrap();
        assert_eq!(markers.len(), 2);
        assert_eq!(markers[0].turn, 1);
        assert_eq!(markers[0].prompt, "first prompt");
        assert_eq!(markers[1].turn, 2);
    }

    #[test]
    fn truncate_after_drops_the_turn_and_its_tail() {
        let tmp = TempDir::new();
        let session = Session::create(&tmp.0).unwrap();
        session.append_marker(1, "one").unwrap();
        session.append(&ChatMessage::user("one")).unwrap();
        session.append(&ChatMessage::assistant("ack one")).unwrap();
        session.append_marker(2, "two").unwrap();
        session.append(&ChatMessage::user("two")).unwrap();
        session.append(&ChatMessage::assistant("ack two")).unwrap();

        assert!(session.truncate_after(2).unwrap());
        let messages = session.load_messages().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].content, "one");
        assert_eq!(messages[1].content, "ack one");
        assert_eq!(session.turn_markers().unwrap().len(), 1);

        // Appending continues to work after the rewrite.
        session.append_marker(3, "three").unwrap();
        session.append(&ChatMessage::user("three")).unwrap();
        assert_eq!(session.load_messages().unwrap().len(), 3);
    }

    #[test]
    fn truncate_after_matches_the_next_marker_across_gaps() {
        let tmp = TempDir::new();
        let session = Session::create(&tmp.0).unwrap();
        session.append_marker(1, "one").unwrap();
        session.append(&ChatMessage::user("one")).unwrap();
        session.append_marker(5, "five").unwrap();
        session.append(&ChatMessage::user("five")).unwrap();

        // Turn 3 has no marker: the cut lands on the next marker (5).
        assert!(session.truncate_after(3).unwrap());
        let messages = session.load_messages().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "one");
    }

    #[test]
    fn old_format_files_without_markers_load_and_survive_truncate() {
        let tmp = TempDir::new();
        let session = Session::create(&tmp.0).unwrap();
        // Old format: message records only.
        session.append(&ChatMessage::user("hello")).unwrap();
        session.append(&ChatMessage::assistant("hi")).unwrap();

        assert!(session.turn_markers().unwrap().is_empty());
        assert!(
            !session.truncate_after(1).unwrap(),
            "no marker to anchor on: the file is left unchanged"
        );
        let messages = session.load_messages().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].content, "hello");
    }

    #[test]
    fn open_by_id_finds_the_named_session_only() {
        let tmp = TempDir::new();
        let session = Session::create(&tmp.0).unwrap();
        session.append(&ChatMessage::user("hi")).unwrap();

        let reopened = Session::open_by_id(&tmp.0, &session.id)
            .unwrap()
            .expect("found by id");
        assert_eq!(reopened.id, session.id);
        assert_eq!(reopened.load_messages().unwrap().len(), 1);

        assert!(Session::open_by_id(&tmp.0, "nope").unwrap().is_none());
        assert!(Session::open_by_id(&tmp.0, "").unwrap().is_none());
    }

    #[test]
    fn summaries_lists_nonempty_sessions_newest_first_with_prompts() {
        let tmp = TempDir::new();
        // Oldest: marker prompt wins over the user message.
        let a = Session::create(&tmp.0).unwrap();
        std::fs::rename(a.path(), tmp.0.join("2026-06-08T10-00-00.jsonl")).unwrap();
        let a = Session::open_by_id(&tmp.0, "2026-06-08T10-00-00")
            .unwrap()
            .unwrap();
        a.append_marker(1, "fix the parser\nignored").unwrap();
        a.append(&ChatMessage::user("fix the parser")).unwrap();
        a.append(&ChatMessage::assistant("on it")).unwrap();

        // Newest: no marker, so the first user line is used.
        let b = Session::create(&tmp.0).unwrap();
        std::fs::rename(b.path(), tmp.0.join("2026-06-09T09-30-00.jsonl")).unwrap();
        let b = Session::open_by_id(&tmp.0, "2026-06-09T09-30-00")
            .unwrap()
            .unwrap();
        b.append(&ChatMessage::user("add resume\nmore")).unwrap();

        // Empty session: excluded.
        Session::create(&tmp.0).unwrap();

        let list = summaries(&tmp.0);
        assert_eq!(list.len(), 2, "the empty session is skipped");
        assert_eq!(list[0].id, "2026-06-09T09-30-00", "newest first");
        assert_eq!(list[0].summary, "add resume");
        assert_eq!(list[0].messages, 1);
        assert_eq!(list[1].summary, "fix the parser");
        assert_eq!(list[1].messages, 2);
    }

    #[test]
    fn open_latest_is_none_for_missing_or_empty_dir() {
        let tmp = TempDir::new();
        assert!(Session::open_latest(&tmp.0).unwrap().is_none());
        assert!(
            Session::open_latest(&tmp.0.join("missing"))
                .unwrap()
                .is_none(),
            "missing dir is not an error"
        );
    }
}
