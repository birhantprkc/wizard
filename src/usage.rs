//! Token-usage accounting: per-call counters accumulated by the agent loop,
//! per-turn records appended to `~/.wizard/usage.jsonl`, and optional cost
//! estimation from per-provider `usd_per_mtok_{in,out}` config.
//!
//! Counts come from [`ChatChunk`](crate::llm::ChatChunk)'s
//! `prompt_eval_count` / `eval_count` fields (every provider reports them on
//! its final chunk when the backend exposes usage). Backends that report
//! nothing simply accumulate zeros and write no records.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Token counters for one agent. Atomics so the agent loop can record from
/// `&self` mid-stream; the agent is single-threaded over these, so plain
/// relaxed ordering suffices.
#[derive(Debug, Default)]
pub struct UsageTracker {
    session_prompt: AtomicU64,
    session_completion: AtomicU64,
    turn_prompt: AtomicU64,
    turn_completion: AtomicU64,
    /// Prompt size of the most recent model call, +1 so 0 means "unknown"
    /// (a genuinely 0-token prompt cannot occur: the system prompt counts).
    last_prompt: AtomicU64,
}

impl UsageTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the usage of one model call. `None` fields (backend reported
    /// nothing) leave the counters untouched.
    pub fn record(&self, prompt_tokens: Option<u64>, completion_tokens: Option<u64>) {
        if let Some(prompt) = prompt_tokens {
            self.session_prompt.fetch_add(prompt, Ordering::Relaxed);
            self.turn_prompt.fetch_add(prompt, Ordering::Relaxed);
            self.last_prompt
                .store(prompt.saturating_add(1), Ordering::Relaxed);
        }
        if let Some(completion) = completion_tokens {
            self.session_completion
                .fetch_add(completion, Ordering::Relaxed);
            self.turn_completion
                .fetch_add(completion, Ordering::Relaxed);
        }
    }

    /// Reset the per-turn counters (called at the top of every turn).
    pub fn begin_turn(&self) {
        self.turn_prompt.store(0, Ordering::Relaxed);
        self.turn_completion.store(0, Ordering::Relaxed);
    }

    /// `(prompt, completion)` tokens of the current turn.
    pub fn turn_totals(&self) -> (u64, u64) {
        (
            self.turn_prompt.load(Ordering::Relaxed),
            self.turn_completion.load(Ordering::Relaxed),
        )
    }

    /// `(prompt, completion)` tokens of the whole session.
    pub fn session_totals(&self) -> (u64, u64) {
        (
            self.session_prompt.load(Ordering::Relaxed),
            self.session_completion.load(Ordering::Relaxed),
        )
    }

    /// Prompt size of the most recent model call, when the backend reported
    /// one. Drives token-aware compaction.
    pub fn last_prompt_tokens(&self) -> Option<u64> {
        match self.last_prompt.load(Ordering::Relaxed) {
            0 => None,
            stored => Some(stored - 1),
        }
    }

    /// Forget the last prompt size (after compaction shrank the history, so
    /// a stale large count does not re-trigger compaction immediately).
    pub fn clear_last_prompt(&self) {
        self.last_prompt.store(0, Ordering::Relaxed);
    }
}

/// One line of `~/.wizard/usage.jsonl`: the token usage of one agent turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageRecord {
    /// Unix seconds when the turn ended.
    pub ts: u64,
    /// Project root the agent worked in.
    pub project: String,
    pub model: String,
    /// Configured provider name (e.g. `"local"`, `"anthropic"`).
    pub provider: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Personality mode (`genie` / `sovereign`).
    pub mode: String,
}

/// `~/.wizard/usage.jsonl`, or `None` when the home directory cannot be
/// resolved (usage logging is then skipped, never fatal).
pub fn default_log_path() -> Option<PathBuf> {
    crate::config::Config::wizard_dir()
        .ok()
        .map(|dir| dir.join("usage.jsonl"))
}

/// Append one record to the JSONL usage log at `path`, creating the file
/// (and its parent directory) as needed.
pub fn append(path: &Path, record: &UsageRecord) -> Result<()> {
    use std::io::Write as _;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut line = serde_json::to_string(record).context("serializing usage record")?;
    line.push('\n');
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| file.write_all(line.as_bytes()))
        .with_context(|| format!("appending to {}", path.display()))
}

/// Current time as unix seconds.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Estimated cost in USD for the given token totals, when at least one rate
/// (`usd_per_mtok_in` / `usd_per_mtok_out`, dollars per million tokens) is
/// configured for the provider. `None` means "no rates configured".
pub fn cost_usd(
    prompt_tokens: u64,
    completion_tokens: u64,
    usd_per_mtok_in: Option<f64>,
    usd_per_mtok_out: Option<f64>,
) -> Option<f64> {
    if usd_per_mtok_in.is_none() && usd_per_mtok_out.is_none() {
        return None;
    }
    Some(
        prompt_tokens as f64 / 1e6 * usd_per_mtok_in.unwrap_or(0.0)
            + completion_tokens as f64 / 1e6 * usd_per_mtok_out.unwrap_or(0.0),
    )
}

/// Compact human form of a token count for status lines: `842 tok`,
/// `12.3k tok`, `4.2M tok`.
pub fn format_tokens(count: u64) -> String {
    if count < 1_000 {
        format!("{count} tok")
    } else if count < 1_000_000 {
        format!("{:.1}k tok", count as f64 / 1e3)
    } else {
        format!("{:.1}M tok", count as f64 / 1e6)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracker_accumulates_turn_and_session_totals() {
        let tracker = UsageTracker::new();
        assert_eq!(tracker.session_totals(), (0, 0));
        assert_eq!(tracker.last_prompt_tokens(), None);

        tracker.record(Some(100), Some(20));
        tracker.record(Some(150), Some(30));
        assert_eq!(tracker.turn_totals(), (250, 50));
        assert_eq!(tracker.session_totals(), (250, 50));
        assert_eq!(tracker.last_prompt_tokens(), Some(150));

        tracker.begin_turn();
        assert_eq!(tracker.turn_totals(), (0, 0));
        assert_eq!(tracker.session_totals(), (250, 50), "session survives");
        assert_eq!(
            tracker.last_prompt_tokens(),
            Some(150),
            "last prompt survives the turn boundary"
        );

        tracker.record(None, None);
        assert_eq!(tracker.session_totals(), (250, 50), "None records nothing");

        tracker.clear_last_prompt();
        assert_eq!(tracker.last_prompt_tokens(), None);
    }

    #[test]
    fn append_writes_one_json_line_per_record() {
        let dir = std::env::temp_dir().join(format!("wizard-usage-{}", uuid::Uuid::new_v4()));
        let path = dir.join("usage.jsonl");
        let record = UsageRecord {
            ts: 1_700_000_000,
            project: "/tmp/proj".to_string(),
            model: "qwen3-8b".to_string(),
            provider: "local".to_string(),
            prompt_tokens: 123,
            completion_tokens: 45,
            mode: "genie".to_string(),
        };
        append(&path, &record).expect("first append");
        append(&path, &record).expect("second append");

        let raw = std::fs::read_to_string(&path).expect("readable");
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 2);
        let parsed: UsageRecord = serde_json::from_str(lines[0]).expect("valid json");
        assert_eq!(parsed.prompt_tokens, 123);
        assert_eq!(parsed.completion_tokens, 45);
        assert_eq!(parsed.model, "qwen3-8b");
        assert_eq!(parsed.provider, "local");
        assert_eq!(parsed.mode, "genie");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cost_requires_at_least_one_rate() {
        assert_eq!(cost_usd(1_000_000, 1_000_000, None, None), None);
        assert_eq!(
            cost_usd(1_000_000, 2_000_000, Some(3.0), Some(15.0)),
            Some(33.0)
        );
        assert_eq!(cost_usd(2_000_000, 500_000, Some(1.0), None), Some(2.0));
        assert_eq!(cost_usd(0, 0, Some(3.0), Some(15.0)), Some(0.0));
    }

    #[test]
    fn token_formatting_scales() {
        assert_eq!(format_tokens(0), "0 tok");
        assert_eq!(format_tokens(842), "842 tok");
        assert_eq!(format_tokens(12_345), "12.3k tok");
        assert_eq!(format_tokens(4_200_000), "4.2M tok");
    }
}
