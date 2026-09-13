//! What is left of a timed run's wall clock, in words the model can act on.
//!
//! A run started with `--max-hours` (or a `schedule.toml` entry's `max_hours`)
//! has always had a deadline, and until now the deadline was a kill switch
//! nobody announced: the loop checked it between steps, ended the run when it
//! passed, and the model was never told the clock existed. A model that does
//! not know it has four minutes left spends them the way it spent the first
//! four, which in practice means drafting in its head and being cut off with
//! nothing on disk.
//!
//! So the same seam the context-pressure note rides on
//! ([`turn::attach_notes`](super::turn)) carries a second line: how much time
//! is left, and — past [`Stage::WrapUp`] — which of the files the task named
//! are still not there. Nothing here fires without a deadline, which is what
//! keeps an interactive session byte-identical to what it was.

use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

/// Prefix of the ephemeral per-step time note. Recognized the same way
/// [`CONTEXT_PRESSURE_HEADING`](super::context::CONTEXT_PRESSURE_HEADING) is:
/// it is an ordinary user message, pushed last and popped after the call, and
/// never written to the session file.
pub const TIME_BUDGET_HEADING: &str = "[time budget]";

/// Fraction of the budget spent before the note starts asking for the
/// deliverable on disk.
///
/// Four tenths of a run is what is left here, and on the runs this was built
/// from that is several model calls: long enough to write a draft and then
/// keep improving it, short enough that a fresh direction will not land.
pub const DEFAULT_WRAP_UP_AT: f64 = 0.6;

/// Fraction of the budget spent before the note says to stop changing things.
///
/// The last seventh of a run is one or two model calls at the latencies a
/// sovereign run actually sees, which is room to save and say where it is and
/// not much else.
pub const DEFAULT_FINISH_AT: f64 = 0.85;

/// Most paths one note will name. A note that lists a dozen files is one the
/// model skims.
const MAX_DELIVERABLES: usize = 6;

/// Longest token [`named_paths`] will even consider. Anything longer is prose
/// that happens to have no spaces in it.
const MAX_PATH_CHARS: usize = 200;

/// Where a timed run is in its budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Plenty left: the note is the number and nothing else.
    Working,
    /// Past [`TimeBudget::wrap_up_at`]: get the deliverable written, stop
    /// opening new fronts.
    WrapUp,
    /// Past [`TimeBudget::finish_at`]: stop changing things and land what is
    /// there.
    Finish,
}

/// A run's wall-clock budget, plus the output paths its task named.
///
/// Carries `start` as well as the deadline because the note quotes both the
/// time left and what the run was given, and "4m left" means something
/// different in a fifteen-minute run and an eight-hour one.
#[derive(Debug, Clone)]
pub struct TimeBudget {
    start: Instant,
    deadline: Instant,
    total: Duration,
    wrap_up_at: f64,
    finish_at: f64,
    deliverables: Vec<PathBuf>,
}

impl TimeBudget {
    /// A budget of `total` starting now, with the two stage thresholds as
    /// fractions of it.
    ///
    /// Both fractions are clamped into `0.0..=1.0` and sorted, so a
    /// hand-edited config that puts `finish_at` before `wrap_up_at` gets two
    /// stages in the order they are named here rather than a stage that can
    /// never be reached.
    pub fn new(start: Instant, total: Duration, wrap_up_at: f64, finish_at: f64) -> Self {
        let wrap_up = clamp_fraction(wrap_up_at, DEFAULT_WRAP_UP_AT);
        let finish = clamp_fraction(finish_at, DEFAULT_FINISH_AT);
        Self {
            start,
            deadline: start + total,
            total,
            wrap_up_at: wrap_up.min(finish),
            finish_at: wrap_up.max(finish),
            deliverables: Vec::new(),
        }
    }

    /// Attach the output paths the task text named (see [`named_paths`]).
    #[must_use]
    pub fn with_deliverables(mut self, deliverables: Vec<PathBuf>) -> Self {
        self.deliverables = deliverables;
        self
    }

    /// When the run must be over.
    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    /// How much of the budget is gone, `0.0..=1.0`.
    fn elapsed_fraction(&self, now: Instant) -> f64 {
        let total = self.total.as_secs_f64();
        if total <= 0.0 {
            return 1.0;
        }
        (now.saturating_duration_since(self.start).as_secs_f64() / total).clamp(0.0, 1.0)
    }

    /// Which stage `now` falls in.
    pub fn stage(&self, now: Instant) -> Stage {
        let spent = self.elapsed_fraction(now);
        if spent >= self.finish_at {
            Stage::Finish
        } else if spent >= self.wrap_up_at {
            Stage::WrapUp
        } else {
            Stage::Working
        }
    }

    /// The paths the task named that are not on disk yet. Empty in
    /// [`Stage::Working`], which is what keeps an ordinary step free of
    /// filesystem work.
    pub fn missing(&self, now: Instant) -> Vec<&Path> {
        if self.stage(now) == Stage::Working {
            return Vec::new();
        }
        self.deliverables
            .iter()
            .filter(|path| !path.exists())
            .map(PathBuf::as_path)
            .take(MAX_DELIVERABLES)
            .collect()
    }

    /// The line the model reads, given the paths [`Self::missing`] found.
    ///
    /// Split from `missing` so the wording is testable without a filesystem,
    /// and so one `stat` per named path serves both.
    pub fn signal_line(&self, now: Instant, missing: &[&Path]) -> String {
        let left = words(self.deadline.saturating_duration_since(now));
        let total = words(self.total);
        let mut line = format!("{TIME_BUDGET_HEADING} {left} of {total} left.");
        match self.stage(now) {
            Stage::Working => return line,
            Stage::WrapUp => line.push_str(
                " Get the deliverable on disk now with what you have, then improve it in \
                 place rather than starting new work.",
            ),
            Stage::Finish => line.push_str(
                " The run stops when the clock does, finished or not. Stop starting \
                 anything that changes state, save what you have, and say where it is.",
            ),
        }
        if !missing.is_empty() {
            let names = missing
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            line.push_str(&format!(" Not written yet: {names}."));
        }
        line
    }
}

/// A fraction from config, or `fallback` when it is not one.
fn clamp_fraction(value: f64, fallback: f64) -> f64 {
    if value.is_finite() && (0.0..=1.0).contains(&value) {
        value
    } else {
        fallback
    }
}

/// A duration in the units a person would say it in: `45s`, `6m10s`, `2h`.
fn words(left: Duration) -> String {
    let secs = left.as_secs();
    if secs < 60 {
        return format!("{secs}s");
    }
    let (minutes, seconds) = (secs / 60, secs % 60);
    if minutes < 60 {
        return if seconds == 0 {
            format!("{minutes}m")
        } else {
            format!("{minutes}m{seconds:02}s")
        };
    }
    let (hours, minutes) = (minutes / 60, minutes % 60);
    if minutes == 0 {
        format!("{hours}h")
    } else {
        format!("{hours}h{minutes:02}m")
    }
}

/// Extensions a bare token may carry to be taken for a path.
///
/// A list rather than a shape test, because the shape is worthless: `3.11`,
/// `v1.2.3` and `e.g.` all look like `name.ext` and none of them is a file.
/// Curated for what tasks actually ask to be written.
const KNOWN_EXTENSIONS: &[&str] = &[
    "bin", "c", "cc", "cfg", "conf", "cpp", "cs", "css", "csv", "dat", "diff", "env", "go", "h",
    "hpp", "htm", "html", "ini", "ipynb", "java", "js", "json", "jsonl", "jpeg", "jpg", "kt",
    "log", "lua", "md", "mjs", "ndjson", "out", "parquet", "patch", "pdf", "php", "pkl", "png",
    "py", "r", "rb", "rs", "sh", "sql", "svg", "swift", "tar", "toml", "ts", "tsv", "tsx", "txt",
    "wasm", "xml", "yaml", "yml", "zip",
];

/// Output paths a task's text names, for the late-stage note.
///
/// Deliberately timid. Every candidate has to survive all of:
///
/// - No whitespace, no glob or shell metacharacter, no `://`, no `@`, no `~`,
///   no `..` component, no leading `-`, no trailing `/`, and at most
///   [`MAX_PATH_CHARS`].
/// - A known extension ([`KNOWN_EXTENSIONS`]), or a quoted token (backticks,
///   `"` or `'`) that contains a `/`.
/// - Absolute, or relative and still under `cwd` once joined.
///
/// So it finds `` `/app/out.txt` ``, `write solution.txt`, and `"build/report"`,
/// and it passes over `https://x/y.json`, `*.txt`, `$OUT/f.txt`, `../secrets`,
/// `Dockerfile`, `v1.2.3` and `e.g.`. Missing a real path costs a less specific
/// note; inventing one would put a made-up filename in front of the model
/// every step, so the rule errs at the first.
pub fn named_paths(text: &str, cwd: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    for (token, quoted) in candidates(text) {
        let Some(path) = as_path(token, quoted, cwd) else {
            continue;
        };
        if !found.contains(&path) {
            found.push(path);
        }
        if found.len() >= MAX_DELIVERABLES {
            break;
        }
    }
    found
}

/// Every token worth testing, with whether it arrived inside quotes.
///
/// Two passes over the same text: the quoted spans (which may hold a path with
/// characters a bare token could not carry), then whitespace splitting for the
/// rest. A path that is both shows up twice and dedupes in `named_paths`.
fn candidates(text: &str) -> Vec<(&str, bool)> {
    let mut out: Vec<(&str, bool)> = Vec::new();
    for delimiter in ['`', '"', '\''] {
        let mut rest = text;
        while let Some(open) = rest.find(delimiter) {
            let after = &rest[open + delimiter.len_utf8()..];
            let Some(close) = after.find(delimiter) else {
                break;
            };
            out.push((after[..close].trim(), true));
            rest = &after[close + delimiter.len_utf8()..];
        }
    }
    for token in text.split_whitespace() {
        out.push((trim_punctuation(token), false));
    }
    out
}

/// Strip the punctuation a path picks up from the sentence around it.
fn trim_punctuation(token: &str) -> &str {
    token
        .trim_start_matches(['(', '[', '<', '`', '"', '\''])
        .trim_end_matches([')', ']', '>', '`', '"', '\'', ',', ';', ':', '!', '?', '.'])
}

/// One candidate, as a path, or `None` when any of the rules in
/// [`named_paths`] refuses it.
fn as_path(token: &str, quoted: bool, cwd: &Path) -> Option<PathBuf> {
    if token.is_empty() || token.chars().count() > MAX_PATH_CHARS {
        return None;
    }
    if token.contains("://") || token.starts_with('-') || token.ends_with('/') {
        return None;
    }
    const REFUSED: &[char] = &[
        '*', '?', '[', ']', '{', '}', '$', '|', ';', '&', '<', '>', '(', ')', '"', '\'', '`', '@',
        '~', '=', '#', ',',
    ];
    if token
        .chars()
        .any(|c| c.is_whitespace() || REFUSED.contains(&c))
    {
        return None;
    }
    let candidate = Path::new(token);
    if candidate
        .components()
        .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
    {
        return None;
    }
    let name = candidate.file_name()?.to_str()?;
    let known_extension = candidate
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| KNOWN_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()));
    let quoted_path = quoted && token.contains('/');
    if !(known_extension || quoted_path) {
        return None;
    }
    if name.starts_with('.') && !known_extension {
        return None;
    }
    if candidate.is_absolute() {
        return Some(candidate.to_path_buf());
    }
    Some(cwd.join(candidate))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget(total_secs: u64) -> (Instant, TimeBudget) {
        let start = Instant::now();
        (
            start,
            TimeBudget::new(
                start,
                Duration::from_secs(total_secs),
                DEFAULT_WRAP_UP_AT,
                DEFAULT_FINISH_AT,
            ),
        )
    }

    #[test]
    fn the_early_note_is_the_clock_and_nothing_else() {
        let (start, budget) = budget(900);
        let at = start + Duration::from_secs(290);
        assert_eq!(budget.stage(at), Stage::Working);
        assert_eq!(
            budget.signal_line(at, &[]),
            "[time budget] 10m10s of 15m left."
        );
    }

    #[test]
    fn wrap_up_asks_for_the_file_and_names_what_is_missing() {
        let (start, budget) = budget(900);
        let at = start + Duration::from_secs(560);
        assert_eq!(budget.stage(at), Stage::WrapUp);
        let missing = [Path::new("/app/out.txt"), Path::new("/app/vm.js")];
        assert_eq!(
            budget.signal_line(at, &missing),
            "[time budget] 5m40s of 15m left. Get the deliverable on disk now with what you \
             have, then improve it in place rather than starting new work. Not written yet: \
             /app/out.txt, /app/vm.js."
        );
    }

    #[test]
    fn the_last_note_says_to_stop_changing_things() {
        let (start, budget) = budget(900);
        let at = start + Duration::from_secs(800);
        assert_eq!(budget.stage(at), Stage::Finish);
        assert_eq!(
            budget.signal_line(at, &[]),
            "[time budget] 1m40s of 15m left. The run stops when the clock does, finished or \
             not. Stop starting anything that changes state, save what you have, and say \
             where it is."
        );
    }

    /// The stage boundaries are the configured fractions, exactly, and an
    /// expired budget still reads `0s` rather than going negative.
    #[test]
    fn stages_turn_over_on_their_thresholds() {
        let (start, budget) = budget(1000);
        assert_eq!(
            budget.stage(start + Duration::from_secs(599)),
            Stage::Working
        );
        assert_eq!(
            budget.stage(start + Duration::from_secs(600)),
            Stage::WrapUp
        );
        assert_eq!(
            budget.stage(start + Duration::from_secs(849)),
            Stage::WrapUp
        );
        assert_eq!(
            budget.stage(start + Duration::from_secs(850)),
            Stage::Finish
        );
        let over = budget.signal_line(start + Duration::from_secs(5000), &[]);
        assert!(
            over.starts_with("[time budget] 0s of 16m40s left."),
            "{over}"
        );
    }

    /// A config that names the thresholds in the wrong order gets both stages
    /// in the order they mean, not a `WrapUp` that can never be reached.
    #[test]
    fn thresholds_out_of_order_are_sorted_and_nonsense_falls_back() {
        let start = Instant::now();
        let swapped = TimeBudget::new(start, Duration::from_secs(100), 0.9, 0.2);
        assert_eq!(
            swapped.stage(start + Duration::from_secs(30)),
            Stage::WrapUp
        );
        assert_eq!(
            swapped.stage(start + Duration::from_secs(95)),
            Stage::Finish
        );

        let nonsense = TimeBudget::new(start, Duration::from_secs(100), f64::NAN, -3.0);
        assert_eq!(
            nonsense.stage(start + Duration::from_secs(50)),
            Stage::Working
        );
        assert_eq!(
            nonsense.stage(start + Duration::from_secs(70)),
            Stage::WrapUp
        );
        assert_eq!(
            nonsense.stage(start + Duration::from_secs(90)),
            Stage::Finish
        );
    }

    #[test]
    fn durations_read_the_way_a_person_says_them() {
        assert_eq!(words(Duration::from_secs(0)), "0s");
        assert_eq!(words(Duration::from_secs(45)), "45s");
        assert_eq!(words(Duration::from_secs(60)), "1m");
        assert_eq!(words(Duration::from_secs(370)), "6m10s");
        assert_eq!(words(Duration::from_secs(3600)), "1h");
        assert_eq!(words(Duration::from_secs(7_500)), "2h05m");
    }

    #[test]
    fn nothing_is_named_before_the_wrap_up_stage() {
        let (start, budget) = budget(900);
        let budget = budget.with_deliverables(vec![PathBuf::from("/nonexistent/out.txt")]);
        assert!(budget.missing(start + Duration::from_secs(10)).is_empty());
        assert_eq!(budget.missing(start + Duration::from_secs(700)).len(), 1);
    }

    #[test]
    fn a_deliverable_that_exists_is_not_named() {
        let dir = tempfile::tempdir().expect("tempdir");
        let written = dir.path().join("out.txt");
        std::fs::write(&written, "done").expect("write");
        let start = Instant::now();
        let budget = TimeBudget::new(start, Duration::from_secs(100), 0.6, 0.85)
            .with_deliverables(vec![written, dir.path().join("missing.txt")]);
        let want = dir.path().join("missing.txt");
        assert_eq!(
            budget.missing(start + Duration::from_secs(70)),
            vec![want.as_path()]
        );
    }

    #[test]
    fn paths_are_taken_from_quotes_extensions_and_the_working_directory() {
        let cwd = Path::new("/work");
        assert_eq!(
            named_paths("Write the answer to /app/out.txt when you are done.", cwd),
            vec![PathBuf::from("/app/out.txt")]
        );
        assert_eq!(
            named_paths("Leave the result in `build/report`.", cwd),
            vec![PathBuf::from("/work/build/report")]
        );
        assert_eq!(
            named_paths("Produce solution.txt and ANSWER.JSON.", cwd),
            vec![
                PathBuf::from("/work/solution.txt"),
                PathBuf::from("/work/ANSWER.JSON")
            ]
        );
        // The same path quoted and bare is one path.
        assert_eq!(
            named_paths("Write `/app/vm.js`. /app/vm.js must run under node.", cwd),
            vec![PathBuf::from("/app/vm.js")]
        );
    }

    #[test]
    fn things_that_merely_look_like_paths_are_refused() {
        let cwd = Path::new("/work");
        for text in [
            "Fetch https://example.com/data.json for input.",
            "Delete every *.txt under the tree.",
            "Mail the result to someone@example.com.",
            "Write it to $OUT/result.txt.",
            "It lives in ../secrets.txt, do not touch it.",
            "Use ~/notes.md as a reference.",
            "Target python 3.11 exactly.",
            "Tag the release v1.2.3, e.g. the usual way.",
            "Add a Dockerfile and a Makefile.",
            "Pass --output=out.txt to the tool.",
            "Everything under logs/ is yours.",
        ] {
            assert_eq!(
                named_paths(text, cwd),
                Vec::<PathBuf>::new(),
                "took a path out of: {text}"
            );
        }
    }

    #[test]
    fn no_more_than_a_handful_of_paths_are_ever_named() {
        let cwd = Path::new("/work");
        let text = (0..20)
            .map(|n| format!("out{n}.txt"))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(named_paths(&text, cwd).len(), MAX_DELIVERABLES);
    }
}
