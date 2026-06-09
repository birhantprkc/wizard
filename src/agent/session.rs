//! JSONL session persistence under `~/.wizard/sessions/<timestamp>.jsonl`.
//! One [`SessionRecord`] per line, appended after each message lands.

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

    /// Load all messages back (for `--resume`). Corrupt lines are skipped.
    pub fn load_messages(&self) -> Result<Vec<ChatMessage>> {
        let file = std::fs::File::open(&self.path)
            .with_context(|| format!("opening {}", self.path.display()))?;
        let mut messages = Vec::new();
        for line in BufReader::new(file).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<SessionRecord>(&line) {
                Ok(record) => messages.push(record.message),
                Err(err) => tracing::warn!("skipping corrupt session line: {err}"),
            }
        }
        Ok(messages)
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
