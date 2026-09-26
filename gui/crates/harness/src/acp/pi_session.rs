//! Pi's failed turns, read back from its session log.
//!
//! When pi's model call fails (an expired sign-in, a provider outage), pi
//! records the assistant message with `stopReason: "error"` and an
//! `errorMessage`, but `pi-acp` settles the ACP prompt as a plain `end_turn`
//! with no content, so the chat would show nothing at all. Pi names each
//! session log `<agent-dir>/sessions/<cwd-slug>/<timestamp>_<session-id>.jsonl`
//! with the same id `pi-acp` uses as the ACP session id, so the turn's error
//! can be recovered from there.

use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

/// The tail read from a session log; one turn's final entries fit easily.
const TAIL_BYTES: u64 = 256 * 1024;

/// Pi's agent dir: `PI_CODING_AGENT_DIR`, else `~/.pi/agent`.
fn agent_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("PI_CODING_AGENT_DIR").filter(|dir| !dir.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(PathBuf::from(home).join(".pi").join("agent"))
}

/// The error pi recorded for the session's latest turn, when that turn
/// failed. `None` when it succeeded or the log can't be found.
pub(super) fn turn_error(session_id: &str) -> Option<String> {
    turn_error_in(&agent_dir()?.join("sessions"), session_id)
}

fn turn_error_in(sessions: &Path, session_id: &str) -> Option<String> {
    let suffix = format!("_{session_id}.jsonl");
    let log = std::fs::read_dir(sessions)
        .ok()?
        .flatten()
        .filter(|dir| dir.path().is_dir())
        .flat_map(|dir| {
            std::fs::read_dir(dir.path())
                .into_iter()
                .flatten()
                .flatten()
        })
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(&suffix))
        })?;
    let mut file = std::fs::File::open(log).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES)))
        .ok()?;
    let mut tail = String::new();
    file.read_to_string(&mut tail).ok()?;
    // The last message entry decides: a later user message means the error
    // belonged to an earlier turn.
    let message = tail.lines().rev().find_map(|line| {
        let entry: Value = serde_json::from_str(line).ok()?;
        (entry.get("type")?.as_str()? == "message").then(|| entry.get("message").cloned())?
    })?;
    if message.get("role")?.as_str()? != "assistant"
        || message.get("stopReason")?.as_str()? != "error"
    {
        return None;
    }
    let error = message
        .get("errorMessage")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|error| !error.is_empty())
        .unwrap_or("the model request failed");
    Some(describe(error))
}

/// Pi's error, with the next step when it is a sign-in problem.
fn describe(error: &str) -> String {
    let lower = error.to_ascii_lowercase();
    let sign_in = [
        "oauth",
        "invalid_grant",
        "unauthorized",
        "401",
        "api key",
        "no api key",
    ]
    .iter()
    .any(|needle| lower.contains(needle));
    if sign_in {
        format!(
            "Pi couldn't reach its model: {error}. Sign Pi in again on this device: \
             run `pi` in a terminal and type /login."
        )
    } else {
        format!("Pi couldn't reach its model: {error}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(dir: &Path, session_id: &str, lines: &[Value]) {
        let slug = dir.join("--home-me--");
        std::fs::create_dir_all(&slug).unwrap();
        let text: Vec<String> = lines.iter().map(Value::to_string).collect();
        std::fs::write(
            slug.join(format!("2026-09-26T18-00-47-084Z_{session_id}.jsonl")),
            text.join("\n") + "\n",
        )
        .unwrap();
    }

    fn message(role: &str, extra: Value) -> Value {
        let mut message = serde_json::json!({"role": role, "content": []});
        message
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::json!({"type": "message", "message": message})
    }

    #[test]
    fn a_failed_turn_reports_pis_error_with_the_sign_in_step() {
        let dir = tempfile::tempdir().unwrap();
        log(
            dir.path(),
            "abc",
            &[
                serde_json::json!({"type": "session", "id": "abc"}),
                message("user", serde_json::json!({})),
                message(
                    "assistant",
                    serde_json::json!({
                        "stopReason": "error",
                        "errorMessage": "OAuth refresh failed for xai: invalid_grant",
                    }),
                ),
            ],
        );
        let error = turn_error_in(dir.path(), "abc").unwrap();
        assert!(error.contains("OAuth refresh failed for xai"), "{error}");
        assert!(error.contains("/login"), "{error}");
    }

    #[test]
    fn successful_or_superseded_turns_report_nothing() {
        let dir = tempfile::tempdir().unwrap();
        log(
            dir.path(),
            "ok",
            &[
                message("user", serde_json::json!({})),
                message("assistant", serde_json::json!({"stopReason": "stop"})),
            ],
        );
        log(
            dir.path(),
            "later",
            &[
                message("assistant", serde_json::json!({"stopReason": "error"})),
                message("user", serde_json::json!({})),
            ],
        );
        assert_eq!(turn_error_in(dir.path(), "ok"), None);
        assert_eq!(turn_error_in(dir.path(), "later"), None);
        assert_eq!(turn_error_in(dir.path(), "missing"), None);
    }
}
