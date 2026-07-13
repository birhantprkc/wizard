//! Token-usage accounting: per-call counters accumulated by the agent loop,
//! per-turn records appended to `~/.wizard/usage.jsonl`, and optional cost
//! estimation from per-provider `usd_per_mtok_{in,out}` config.
//!
//! Counts come from [`ChatChunk`](crate::llm::ChatChunk)'s
//! `prompt_eval_count` / `eval_count` fields (every provider reports them on
//! its final chunk when the backend exposes usage). Backends that report
//! nothing simply accumulate zeros and write no records.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
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

    /// Zero every counter (session, turn, last prompt). Used by `/clear` so
    /// the TUI context meter and `/cost` do not keep totals from the wiped
    /// conversation.
    pub fn clear_session(&self) {
        self.session_prompt.store(0, Ordering::Relaxed);
        self.session_completion.store(0, Ordering::Relaxed);
        self.turn_prompt.store(0, Ordering::Relaxed);
        self.turn_completion.store(0, Ordering::Relaxed);
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

// ---------------------------------------------------------------------------
// `wizard usage` — rollup over ~/.wizard/usage.jsonl
// ---------------------------------------------------------------------------

/// Read-side view of one usage.jsonl line. Liberal on purpose (missing
/// fields default, unknown fields are ignored) so old and future records
/// both roll up; `cost_usd` is summed when a writer recorded one.
#[derive(Debug, Clone, Deserialize)]
struct LoggedTurn {
    ts: u64,
    #[serde(default)]
    project: String,
    #[serde(default)]
    provider: String,
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    cost_usd: Option<f64>,
}

/// Aggregated usage for one rollup key (a project or a provider).
#[derive(Debug, Default, Clone, PartialEq)]
struct Rollup {
    turns: u64,
    prompt: u64,
    completion: u64,
    /// Sum of the records that carried a cost; `None` when none did.
    cost_usd: Option<f64>,
}

impl Rollup {
    fn add(&mut self, turn: &LoggedTurn) {
        self.turns += 1;
        self.prompt += turn.prompt_tokens;
        self.completion += turn.completion_tokens;
        if let Some(cost) = turn.cost_usd {
            *self.cost_usd.get_or_insert(0.0) += cost;
        }
    }
}

/// Parse a `--since` value: `7d`, `7D`, or a bare day count.
fn parse_since_days(raw: &str) -> Result<u64> {
    let days: u64 = raw
        .trim()
        .trim_end_matches(['d', 'D'])
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid --since {raw:?} (expected a day count like 7d)"))?;
    if days == 0 {
        bail!("--since must be at least 1 day");
    }
    Ok(days)
}

/// Group turns under a key ("(unknown)" for records missing it).
fn roll_up<'a>(
    turns: &'a [LoggedTurn],
    key: impl Fn(&'a LoggedTurn) -> &'a str,
) -> BTreeMap<String, Rollup> {
    let mut groups: BTreeMap<String, Rollup> = BTreeMap::new();
    for turn in turns {
        let raw = key(turn);
        let name = if raw.is_empty() { "(unknown)" } else { raw };
        groups.entry(name.to_string()).or_default().add(turn);
    }
    groups
}

/// Print one aligned rollup table.
fn print_rollup(title: &str, groups: &BTreeMap<String, Rollup>) {
    let name_width = groups
        .keys()
        .map(String::len)
        .max()
        .unwrap_or(0)
        .max(title.len());
    println!(
        "{title:<name_width$}  {:>6}  {:>10}  {:>10}  {:>10}",
        "turns", "prompt", "completion", "cost"
    );
    for (name, rollup) in groups {
        let cost = rollup
            .cost_usd
            .map_or_else(|| "-".to_string(), |usd| format!("${usd:.2}"));
        println!(
            "{name:<name_width$}  {:>6}  {:>10}  {:>10}  {cost:>10}",
            rollup.turns,
            format_tokens(rollup.prompt),
            format_tokens(rollup.completion),
        );
    }
}

/// `wizard usage [--since <days>d]`: per-project and per-provider rollup of
/// `~/.wizard/usage.jsonl`. Self-contained: no config load, no LLM.
pub fn run_cli(since: Option<&str>) -> Result<i32> {
    let days = since.map(parse_since_days).transpose()?;
    let path = default_log_path().context("could not resolve ~/.wizard")?;
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            println!("no usage recorded yet ({} does not exist)", path.display());
            return Ok(0);
        }
        Err(err) => return Err(err).context(format!("reading {}", path.display())),
    };

    let cutoff = days.map(|d| unix_now().saturating_sub(d * 86_400));
    let turns: Vec<LoggedTurn> = raw
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| match serde_json::from_str::<LoggedTurn>(line) {
            Ok(turn) => Some(turn),
            Err(err) => {
                tracing::warn!("skipping malformed usage line: {err}");
                None
            }
        })
        .filter(|turn| cutoff.is_none_or(|cutoff| turn.ts >= cutoff))
        .collect();

    if turns.is_empty() {
        match days {
            Some(days) => println!("no usage recorded in the last {days} day(s)"),
            None => println!("no usage recorded yet"),
        }
        return Ok(0);
    }

    let window = days.map_or_else(|| "all time".to_string(), |d| format!("last {d}d"));
    println!("usage ({window}) — {} turn(s)\n", turns.len());
    print_rollup("project", &roll_up(&turns, |t| t.project.as_str()));
    println!();
    print_rollup("provider", &roll_up(&turns, |t| t.provider.as_str()));
    Ok(0)
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

        tracker.record(Some(10), Some(5));
        tracker.clear_session();
        assert_eq!(tracker.session_totals(), (0, 0));
        assert_eq!(tracker.turn_totals(), (0, 0));
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

    #[test]
    fn since_parsing_accepts_day_suffix_and_rejects_junk() {
        assert_eq!(parse_since_days("7d").unwrap(), 7);
        assert_eq!(parse_since_days("30D").unwrap(), 30);
        assert_eq!(parse_since_days(" 2 ").unwrap(), 2);
        assert!(parse_since_days("0d").is_err());
        assert!(parse_since_days("soon").is_err());
        assert!(parse_since_days("").is_err());
    }

    #[test]
    fn rollup_groups_by_key_and_sums_optional_cost() {
        let turn = |project: &str, provider: &str, cost: Option<f64>| LoggedTurn {
            ts: 1_700_000_000,
            project: project.to_string(),
            provider: provider.to_string(),
            prompt_tokens: 100,
            completion_tokens: 10,
            cost_usd: cost,
        };
        let turns = vec![
            turn("/a", "local", None),
            turn("/a", "claude", Some(0.25)),
            turn("/b", "claude", Some(0.50)),
            turn("", "", None),
        ];

        let by_project = roll_up(&turns, |t| t.project.as_str());
        assert_eq!(by_project.len(), 3);
        let a = &by_project["/a"];
        assert_eq!((a.turns, a.prompt, a.completion), (2, 200, 20));
        assert_eq!(a.cost_usd, Some(0.25));
        assert!(by_project.contains_key("(unknown)"), "empty key is labeled");

        let by_provider = roll_up(&turns, |t| t.provider.as_str());
        let claude = &by_provider["claude"];
        assert_eq!(claude.turns, 2);
        assert_eq!(claude.cost_usd, Some(0.75));
        assert_eq!(
            by_provider["local"].cost_usd, None,
            "no cost recorded stays None, not $0"
        );
    }

    #[test]
    fn logged_turn_parses_current_records_and_tolerates_extras() {
        // A record exactly as `append` writes it today (no cost_usd).
        let line = r#"{"ts":1700000000,"project":"/p","model":"m","provider":"local","prompt_tokens":5,"completion_tokens":2,"mode":"genie"}"#;
        let turn: LoggedTurn = serde_json::from_str(line).expect("parses");
        assert_eq!(turn.prompt_tokens, 5);
        assert_eq!(turn.cost_usd, None);

        // Future records may carry cost_usd and new fields.
        let line = r#"{"ts":1,"project":"/p","provider":"x","prompt_tokens":1,"completion_tokens":1,"cost_usd":0.1,"new_field":true}"#;
        let turn: LoggedTurn = serde_json::from_str(line).expect("parses");
        assert_eq!(turn.cost_usd, Some(0.1));
    }
}
