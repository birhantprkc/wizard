//! Diagnostic logging: a JSONL `tracing` subscriber that never touches the
//! terminal.
//!
//! Wizard emits `tracing` events from most of its interesting failure paths
//! (the agent loop, the provider clients, MCP, the tool registry). For a long
//! time no subscriber was installed, so every one of them was dropped on the
//! floor and a bug report arrived with nothing but the reporter's memory of
//! what happened. This module is the sink that makes them real.
//!
//! One constraint shapes the whole design: **an event must never reach stdout
//! or stderr**. `wizard acp` and `wizard mcp-serve` speak JSON-RPC over
//! stdout, and the TUI owns the terminal through crossterm's alternate
//! screen. A subscriber that printed would corrupt the protocol in the first
//! two cases and the frame in the third, so the only sink here is a file,
//! `~/.wizard/logs/<session>.jsonl`, and every fallback that could print (the
//! fmt layer's internal-error `eprintln!` included) is turned off.
//!
//! What lands there is chosen by `WIZARD_LOG`, in the usual `RUST_LOG`
//! directive syntax. The default ([`DEFAULT_FILTER`]) is wizard's own
//! warnings and errors and nothing at all from dependencies, which keeps a
//! normal session to a handful of lines. Panics are appended by
//! [`log_panic`] from the panic hook in `main`, filter or no filter.

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{SecondsFormat, Utc};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt as _;

use crate::config::Config;

/// Environment variable selecting what gets logged, in `RUST_LOG` directive
/// syntax: `WIZARD_LOG=debug`, `WIZARD_LOG=wizard::agent=trace`,
/// `WIZARD_LOG=off`.
pub const FILTER_ENV: &str = "WIZARD_LOG";

/// Filter used when `WIZARD_LOG` is unset, empty, or unparseable: wizard's own
/// warnings and errors, nothing else. The leading `off` is the global
/// directive, so every target that is not wizard is disabled outright and a
/// chatty dependency (hyper, reqwest, mlua's host) can never fill the log.
pub const DEFAULT_FILTER: &str = "off,wizard=warn";

/// How many session logs `~/.wizard/logs/` keeps. Older ones are deleted when
/// a new session first writes something.
const MAX_SESSION_LOGS: usize = 20;

/// Byte budget for a single session log. [`MAX_SESSION_LOGS`] bounds the file
/// count but not the size, and one long-lived sovereign run under
/// `WIZARD_LOG=trace` would otherwise write until the disk filled.
const MAX_SESSION_BYTES: u64 = 8 * 1024 * 1024;

/// This process's session log, once [`init`] has installed the subscriber.
/// Held globally so [`log_panic`] can append to the same file from the panic
/// hook without threading a handle through `main`.
static SINK: OnceLock<Sink> = OnceLock::new();

/// Install the global JSONL subscriber and return the path it will write to.
///
/// Call this once, as early in `main` as possible: every surface that starts
/// afterwards emits events, and anything emitted before the global default is
/// set is dropped. `None` means logging could not be set up at all (no
/// resolvable home directory, or a subscriber was already installed), which is
/// deliberately not an error: there is nowhere safe to report one from, and a
/// missing log must never stop wizard from starting.
///
/// The file itself is not created here. See [`SessionLog::file`].
pub fn init() -> Option<PathBuf> {
    let path = Config::logs_dir()
        .ok()?
        .join(format!("{}.jsonl", session_stem()));
    let sink = Sink::new(path.clone());
    tracing::subscriber::set_global_default(subscriber(sink.clone(), env_filter())).ok()?;
    // Publish the sink for the panic hook only once the subscriber it feeds is
    // actually installed, so a process without logging never has a panic
    // conjure a log file out of nowhere.
    let _ = SINK.set(sink);
    Some(path)
}

/// Stem of this process's log file: the timestamp format
/// `crate::agent::session` names session transcripts with, plus the pid so two
/// wizards started in the same second do not interleave into one file.
fn session_stem() -> String {
    format!(
        "{}-{}",
        Utc::now().format("%Y-%m-%dT%H-%M-%S"),
        std::process::id()
    )
}

/// Compose the subscriber: an [`EnvFilter`] over a JSON fmt layer writing to
/// `sink`.
///
/// Kept separate from [`init`] because a global default can only be installed
/// once per process, so the tests install this same stack per-thread with
/// `tracing::subscriber::with_default` instead.
fn subscriber(sink: Sink, filter: EnvFilter) -> impl tracing::Subscriber + Send + Sync + 'static {
    let layer = tracing_subscriber::fmt::layer()
        .json()
        // Nothing in wizard opens a tracing span, so the per-event span fields
        // would be two permanently empty keys on every line.
        .with_current_span(false)
        .with_span_list(false)
        // The constraint, restated in code: tracing-subscriber's fallback for
        // a failed write is an `eprintln!`. Over an ACP stdio session or a TUI
        // frame that fallback does more damage than the lost event.
        //
        // Honest accounting: `false` is also tracing-subscriber 0.3's own
        // default, so this line changes nothing today. It is here so that a
        // future version flipping that default cannot quietly turn the
        // `eprintln!` back on. `subscriber_never_writes_to_stdout_or_stderr`
        // catches this being set to `true`; nothing can catch it being
        // deleted, because deleting it leaves the same behaviour until the
        // day the library's default moves.
        .log_internal_errors(false)
        .with_writer(sink);
    tracing_subscriber::registry().with(filter).with(layer)
}

/// The filter for this process, from `WIZARD_LOG` or [`DEFAULT_FILTER`].
fn env_filter() -> EnvFilter {
    filter_from(std::env::var(FILTER_ENV).ok().as_deref())
}

/// Testable core of [`env_filter`]; `raw` is the `WIZARD_LOG` value, if any.
///
/// Unset, empty, and unparseable all fall back to [`DEFAULT_FILTER`]. A typo
/// in an environment variable must not stop wizard from starting, and this
/// runs before any surface exists to complain on.
///
/// Every parse here is the fallible one. `EnvFilter::new` is the obvious way
/// to build the fallback, but it is lossy: a bad directive is dropped *after*
/// an `eprintln!` from inside tracing-subscriber, which is exactly the write
/// to stderr this module exists to make impossible. So a broken
/// [`DEFAULT_FILTER`] degrades to silence instead, and
/// `default_filter_parses` is what keeps that branch unreachable.
fn filter_from(raw: Option<&str>) -> EnvFilter {
    raw.map(str::trim)
        .filter(|raw| !raw.is_empty())
        .and_then(|raw| EnvFilter::try_new(raw).ok())
        .or_else(|| EnvFilter::try_new(DEFAULT_FILTER).ok())
        .unwrap_or_default()
}

/// Append a panic to the session log. Called from the panic hook in `main`,
/// after the terminal has been restored.
///
/// This writes the record by hand instead of going through `tracing::error!`
/// for two reasons: a panic has to be recorded whatever `WIZARD_LOG` says, and
/// by the time the hook runs the thread is already on its way out, so the
/// fewer moving parts between here and the file the better. It is also the one
/// record allowed past the [`MAX_SESSION_BYTES`] budget.
pub fn log_panic(info: &std::panic::PanicHookInfo<'_>) {
    // No sink means `init` never ran (or could not): a panic before logging
    // exists has nowhere to go, and a panic hook that panicked itself would
    // abort the process instead of letting the crash report through.
    let Some(sink) = SINK.get() else {
        return;
    };
    log_panic_to(sink, info);
}

/// Testable core of [`log_panic`], with the sink passed in rather than read
/// from the process-global [`SINK`] that only [`init`] may set.
fn log_panic_to(sink: &Sink, info: &std::panic::PanicHookInfo<'_>) {
    let payload = info.payload();
    let message = payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("panic with a non-string payload");
    // `force_capture`, not `capture`: whoever hits the crash will not have
    // thought to set `RUST_BACKTRACE`, and this file is the only record that
    // outlives the process.
    let backtrace = std::backtrace::Backtrace::force_capture().to_string();
    let line = record_line(
        "ERROR",
        "wizard::panic",
        serde_json::json!({
            "message": message,
            "location": info.location().map(|at| at.to_string()),
            "backtrace": backtrace,
        }),
    );
    // A panic can be the first thing this process ever writes, so the
    // directory may still need building, and that has to happen before the
    // lock for the reason [`Sink::prepare_dir_with`] gives.
    sink.prepare_dir();
    // Locking here cannot deadlock against the subscriber: the guard is only
    // ever held across a single `write_all`, and nothing on that path logs or
    // panics.
    let mut log = sink.lock();
    let _ = log.write_line(line.as_bytes());
    let _ = log.flush();
}

/// One JSONL line shaped like the fmt layer's own output (`timestamp`,
/// `level`, `target`, `fields`).
///
/// Used for the two records this module writes itself, the truncation notice
/// and the panic report. Both bypass the subscriber, so they have to match its
/// shape by hand or a reader would need two parsers for one file.
fn record_line(level: &str, target: &str, fields: serde_json::Value) -> String {
    let record = serde_json::json!({
        "timestamp": Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true),
        "level": level,
        "target": target,
        "fields": fields,
    });
    format!("{record}\n")
}

/// Delete session logs until at most `keep` remain, oldest first.
///
/// Only this module's own files are candidates: `*.jsonl` directly inside
/// `dir`. `crate::schedule` keeps `scheduler.log` and a `jobs/` subdirectory in
/// the same place, and neither is ours to delete.
///
/// Errors are swallowed on purpose. This runs from inside the subscriber, on
/// the first event of a session, where a failure has nowhere to go; a log
/// directory one file over its cap is not worth propagating.
fn prune(dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut logs: Vec<(SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "jsonl"))
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .map(|entry| {
            let modified = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .unwrap_or(UNIX_EPOCH);
            (modified, entry.path())
        })
        .collect();
    if logs.len() <= keep {
        return;
    }
    // Newest first, by [`eviction_key`].
    logs.sort_by(|left, right| eviction_key(right.0, &right.1).cmp(&eviction_key(left.0, &left.1)));
    for (_, path) in logs.into_iter().skip(keep) {
        let _ = std::fs::remove_file(path);
    }
}

/// Ordering key for [`prune`]: mtime first, then the two halves of the file
/// stem.
///
/// Filesystems with one-second mtime granularity leave same-second logs tied,
/// and the stem ([`session_stem`], `<%Y-%m-%dT%H-%M-%S>-<pid>`) is what breaks
/// the tie. Its timestamp half is fixed width, so comparing it as text is
/// chronological; its pid half is *not* zero-padded, so it is parsed and
/// compared as a number. As text, pid `9` sorts after pid `10`, which would
/// evict the later of two same-second logs first, the opposite of what
/// pruning oldest-first means. A stem in any other shape sorts by its whole
/// text, which is all the order a foreign name deserves.
fn eviction_key(modified: SystemTime, path: &Path) -> (SystemTime, &str, u64) {
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("");
    match stem.rsplit_once('-').map(|(rest, pid)| (rest, pid.parse())) {
        Some((rest, Ok(pid))) => (modified, rest, pid),
        _ => (modified, stem, 0),
    }
}

/// Clonable handle on the session log, shaped as a [`MakeWriter`] so the fmt
/// layer can write through it.
#[derive(Clone)]
struct Sink(Arc<SinkState>);

/// What every clone of a [`Sink`] shares: the log itself behind its mutex, and
/// the flag that keeps the directory setup off that mutex.
struct SinkState {
    log: Mutex<SessionLog>,
    /// Set by the first thread to reach [`Sink::prepare_dir`], so the
    /// directory is built once per process and, more importantly, so a nested
    /// event emitted *while* it is being built stops there instead of
    /// recursing. See [`Sink::prepare_dir_with`].
    dir_prepared: AtomicBool,
}

impl Sink {
    fn new(path: PathBuf) -> Self {
        Self(Arc::new(SinkState {
            log: Mutex::new(SessionLog {
                path,
                file: None,
                written: 0,
                capped: false,
            }),
            dir_prepared: AtomicBool::new(false),
        }))
    }

    /// Lock the log, recovering from poisoning rather than propagating it: a
    /// thread that panicked mid-write leaves at most one torn line, and losing
    /// every later event over that would defeat the point of the log.
    ///
    /// Everything that runs under this guard must be incapable of emitting a
    /// `tracing` event: `create_dir_all`, `OpenOptions::open`, `remove_file`,
    /// `write_all`. `std::sync::Mutex` is not reentrant, and with a *global*
    /// default subscriber installed tracing-core has no reentrancy guard of
    /// its own (its `get_default` fast path dispatches straight to the global
    /// subscriber), so an event emitted under this guard would come back
    /// through [`MakeWriter::make_writer`] and hang the process on its own
    /// lock. That is why the directory setup, which *can* log, is
    /// [`Sink::prepare_dir`] and runs before the guard exists.
    fn lock(&self) -> MutexGuard<'_, SessionLog> {
        self.0.log.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Build the directory the session log lives in, once per process, with no
    /// lock held. Every path that is about to take [`Sink::lock`] calls this
    /// first.
    fn prepare_dir(&self) {
        // The error is dropped here on purpose: `SessionLog::create` creates
        // the directory again, silently, and reports a real failure through
        // the write it is serving, which is the only caller that can act on
        // one.
        self.prepare_dir_with(|dir| {
            let _ = ensure_log_dir(dir);
        });
    }

    /// Testable core of [`prepare_dir`](Self::prepare_dir): `ensure` is what
    /// actually creates the directory.
    ///
    /// The flag is swapped *before* `ensure` runs, not after. `ensure` reaches
    /// `Config::ensure_dirs`, which logs a warning when it cannot chmod the
    /// tree (exFAT, a CIFS/NFS mount, WSL DrvFs, a `~/.wizard` some `sudo
    /// wizard` left owned by root), and that warning comes straight back into
    /// this sink. Finding the flag already set is what stops it recursing;
    /// finding the mutex free is what stops it deadlocking. A `std::sync::Once`
    /// would deadlock on exactly that reentrant call, and a second mutex would
    /// deadlock against itself, so the flag is a plain atomic.
    ///
    /// `Relaxed` because it orders nothing: it bounds recursion, and it is not
    /// a claim that the directory exists by the time another thread reads it.
    /// The racing thread's write lands in `SessionLog::create`, which creates
    /// the directory itself.
    fn prepare_dir_with(&self, ensure: impl FnOnce(&Path)) {
        if self.0.dir_prepared.swap(true, Ordering::Relaxed) {
            return;
        }
        // Scoped so the guard is dropped before `ensure` runs: reading a path
        // cannot log, running `ensure` very much can.
        let dir = {
            let log = self.lock();
            log.path.parent().map(Path::to_path_buf)
        };
        if let Some(dir) = dir {
            ensure(&dir);
        }
    }
}

impl<'a> MakeWriter<'a> for Sink {
    type Writer = SinkWriter<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        // Before the lock, never under it. This is the sink's whole lock
        // discipline in one line; see [`Sink::prepare_dir_with`].
        self.prepare_dir();
        SinkWriter(self.lock())
    }
}

/// The lock guard, which the fmt layer holds for exactly one `write_all`.
/// That is what keeps two threads' events from interleaving inside a line.
struct SinkWriter<'a>(MutexGuard<'a, SessionLog>);

impl std::io::Write for SinkWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.append(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

/// The session log: where it lives, the file once anything has been logged,
/// and the byte-budget bookkeeping.
struct SessionLog {
    path: PathBuf,
    /// Opened lazily on the first event. Most invocations (`wizard usage`,
    /// `wizard doctor`, a shell completion probe) log nothing at all, and
    /// creating an empty file for each of them would push the logs that do
    /// have content out of the directory long before they were stale.
    file: Option<File>,
    /// Bytes written to `file`, including the truncation notice.
    written: u64,
    /// Set once the budget is spent, so the notice is written exactly once.
    capped: bool,
}

impl SessionLog {
    /// Take one already-formatted event line from the fmt layer.
    fn append(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.written >= MAX_SESSION_BYTES {
            if !self.capped {
                self.capped = true;
                let notice = record_line(
                    "WARN",
                    "wizard::logging",
                    serde_json::json!({
                        "message": format!(
                            "session log passed its {MAX_SESSION_BYTES} byte budget; \
                             later events dropped"
                        ),
                    }),
                );
                self.write_line(notice.as_bytes())?;
            }
            // Report the write as accepted. The caller's only error path is the
            // `eprintln!` disabled in `subscriber`, and dropping the event is
            // exactly what the budget is for.
            return Ok(buf.len());
        }
        self.write_line(buf)?;
        Ok(buf.len())
    }

    /// Write one whole line, opening the file if this is the first.
    ///
    /// `write_all` rather than `write`, so a line is never half-written: a
    /// short write would leave unparseable JSON in a JSONL file. The budget can
    /// therefore overshoot by one event, which is the right way round.
    fn write_line(&mut self, line: &[u8]) -> std::io::Result<()> {
        if self.file.is_none() {
            let file = self.create()?;
            self.file = Some(file);
        }
        // `if let` rather than an unwrap: the block above either filled the
        // slot or returned, and a diagnostic sink is the last place in the
        // binary that should be able to panic.
        if let Some(file) = self.file.as_mut() {
            file.write_all(line)?;
            self.written += line.len() as u64;
        }
        Ok(())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self.file.as_mut() {
            Some(file) => file.flush(),
            None => Ok(()),
        }
    }

    /// Create the log file, pruning the directory first.
    ///
    /// Pruning before the new file exists (with room reserved for it) means
    /// this session's own log can never be the one deleted. No `BufWriter`
    /// wraps the result: an event is a single `write_all` of a few hundred
    /// bytes, and buffering would lose precisely the last events before a
    /// crash, which are the ones worth having.
    ///
    /// The directory is created with a bare `create_dir_all` and not with
    /// [`ensure_log_dir`], which is the private-and-loud one: this runs with
    /// the sink's mutex held (see [`Sink::lock`]), so it may only call things
    /// that cannot log. On the normal path there is nothing left to do,
    /// because [`Sink::prepare_dir`] has already built the whole `~/.wizard`
    /// tree at 0700 before any of this was locked. This is the fallback for a
    /// relocated sink (the tests), and for the moment inside `ensure_dirs`
    /// where a nested event arrives before the logs directory has its turn;
    /// `ensure_dirs` tightens the mode when it gets there.
    fn create(&self) -> std::io::Result<File> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
            prune(dir, MAX_SESSION_LOGS.saturating_sub(1));
        }
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
    }
}

/// Create the directory the session log lives in, privately.
///
/// This is often the call that brings `~/.wizard` itself into existence: the
/// subcommands that dispatch before `Config::load` (`wizard mcp-serve`,
/// `wizard usage`, `wizard harness`) never run `Config::ensure_dirs`, and any
/// of them can emit the first warning of a fresh install. A bare
/// `create_dir_all` would create `~/.wizard` and `~/.wizard/logs` at the
/// process umask, world-readable on a stock distro, and a session log carries
/// prompts, tool output, and error detail. So the real logs directory goes
/// through `Config::ensure_dirs`, the one place that knows the whole tree is
/// 0700 (and which tightens a tree an older release left loose).
///
/// That path is *not* silent, which is why only [`Sink::prepare_dir`] may call
/// this: `Config::ensure_dirs` warns through `tracing` when it cannot chmod a
/// directory (which is the documented, supported case on exFAT, CIFS/NFS and
/// WSL DrvFs), and that event comes back into this sink. It is harmless with
/// no lock held and a hang with one, so this function must never be reached
/// from under [`Sink::lock`].
fn ensure_log_dir(dir: &Path) -> std::io::Result<()> {
    if Config::logs_dir().is_ok_and(|logs| logs == dir) && Config::ensure_dirs().is_ok() {
        return Ok(());
    }
    // Either the sink was relocated (the tests) or the private tree could not
    // be built at all; a plain directory still beats losing the diagnostics.
    std::fs::create_dir_all(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Marker the stdout/stderr probe logs; distinctive enough that finding it
    /// in the child's output can only mean the subscriber printed it.
    const PROBE_MARKER: &str = "probe-event-6f2c1d";

    /// Marker the panic probe panics with, for the parent to find in the
    /// child's session log.
    const PANIC_MARKER: &str = "probe-panic-9c4b7a";

    /// Set by the parent so the probe test does something. Without it a normal
    /// `cargo test` run would have the probe install a global subscriber, which
    /// would then fight with every other test in this module.
    const PROBE_ENV: &str = "WIZARD_LOGGING_STDIO_PROBE";

    /// [`PROBE_ENV`] for the panic probe, which needs its own process: `init`
    /// and `set_hook` are both once-per-process.
    const PANIC_PROBE_ENV: &str = "WIZARD_LOGGING_PANIC_PROBE";

    /// Prefix the probe prints its log path under, for the parent to read back.
    const PROBE_PATH_PREFIX: &str = "probe-log-path=";

    /// Serialises the two tests that install a panic hook.
    ///
    /// The hook is process-global and `take_hook` + `set_hook` is a
    /// read-modify-write, so two tests doing it at once lose one another's
    /// update: the second one takes the first one's closure as "the previous
    /// hook" and restores *that* at the end, leaving the whole test binary
    /// with a hook pointing at a deleted temp directory and no default hook to
    /// print the message and location of any later failure. Both tests hold
    /// this for the length of their window.
    static PANIC_HOOK: Mutex<()> = Mutex::new(());

    /// Run `events` against a subscriber writing to `path` under `filter`, then
    /// return the parsed log lines. `with_default` scopes the subscriber to
    /// this thread, which is what lets every test here install one of its own
    /// in the same process.
    fn capture(path: &Path, filter: Option<&str>, events: impl FnOnce()) -> Vec<serde_json::Value> {
        let sink = Sink::new(path.to_path_buf());
        tracing::subscriber::with_default(subscriber(sink, filter_from(filter)), events);
        let Ok(raw) = std::fs::read_to_string(path) else {
            return Vec::new();
        };
        raw.lines()
            .map(|line| serde_json::from_str(line).expect("every log line is valid json"))
            .collect()
    }

    /// The `fields.message` of each captured record.
    fn messages(records: &[serde_json::Value]) -> Vec<String> {
        records
            .iter()
            .filter_map(|record| record["fields"]["message"].as_str())
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn default_filter_parses() {
        // Not cosmetic, and not a panic either: `EnvFilter` parsing is lossy,
        // so a typo in this constant would be reported by an `eprintln!` from
        // inside tracing-subscriber on every start (into the TUI's terminal,
        // into `wizard acp`'s JSON-RPC stream) and then silently drop the
        // wizard directive. `try_new` is the parse that can say no.
        EnvFilter::try_new(DEFAULT_FILTER)
            .unwrap_or_else(|err| panic!("DEFAULT_FILTER ({DEFAULT_FILTER:?}) must parse: {err}"));
    }

    #[test]
    fn empty_and_blank_filters_fall_back_to_the_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        // `WIZARD_LOG=` and `WIZARD_LOG="   "` are what a half-written shell
        // export leaves behind; both mean "unset", not "log nothing".
        for (index, raw) in ["", "   ", "\t"].iter().enumerate() {
            let path = dir.path().join(format!("blank-{index}.jsonl"));
            let records = capture(&path, Some(raw), || {
                tracing::warn!("default filter applies");
                tracing::info!("and still drops info");
                tracing::warn!(target: "hyper::client", "and still drops dependencies");
            });
            assert_eq!(
                messages(&records),
                vec!["default filter applies"],
                "{raw:?} must behave exactly like an unset WIZARD_LOG"
            );
        }
    }

    #[test]
    fn writes_parseable_jsonl_to_the_session_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("logs").join("session.jsonl");

        let records = capture(&path, None, || {
            tracing::warn!(attempt = 3, "provider call failed");
        });

        assert!(path.exists(), "the log file is created on the first event");
        assert_eq!(records.len(), 1, "one line per event");
        let record = &records[0];
        assert_eq!(record["level"], "WARN");
        assert_eq!(record["target"], "wizard::logging::tests");
        assert_eq!(record["fields"]["message"], "provider call failed");
        assert_eq!(record["fields"]["attempt"], 3);
        assert!(
            record["timestamp"].is_string(),
            "records carry a timestamp: {record}"
        );
    }

    #[test]
    fn no_events_leaves_no_file_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("logs").join("session.jsonl");

        let records = capture(&path, None, || {});

        assert!(records.is_empty());
        assert!(
            !path.exists(),
            "a run that logs nothing must not evict a log that has content"
        );
    }

    #[test]
    fn default_filter_keeps_wizard_warnings_and_drops_dependencies() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("session.jsonl");

        let records = capture(&path, None, || {
            tracing::info!("wizard info is below the default");
            tracing::warn!("wizard warning");
            tracing::error!("wizard error");
            tracing::warn!(target: "hyper::client", "dependency warning");
        });

        assert_eq!(
            messages(&records),
            vec!["wizard warning", "wizard error"],
            "default filter is warn for wizard, off for everything else"
        );
    }

    #[test]
    fn wizard_log_overrides_the_default() {
        let dir = tempfile::tempdir().expect("tempdir");

        let verbose = dir.path().join("verbose.jsonl");
        let records = capture(&verbose, Some("wizard=debug"), || {
            tracing::debug!("now recorded");
            tracing::trace!("still too fine");
        });
        assert_eq!(messages(&records), vec!["now recorded"]);

        let targeted = dir.path().join("targeted.jsonl");
        let records = capture(&targeted, Some("wizard::logging::tests=error"), || {
            tracing::warn!("below the target's level");
            tracing::error!("kept");
        });
        assert_eq!(messages(&records), vec!["kept"]);

        let silenced = dir.path().join("off.jsonl");
        let records = capture(&silenced, Some("off"), || {
            tracing::error!("nothing survives off");
        });
        assert!(records.is_empty());
        assert!(!silenced.exists(), "`off` never opens the file");

        // A directive wizard cannot parse falls back to the default rather
        // than taking the process down.
        let bogus = dir.path().join("bogus.jsonl");
        let records = capture(&bogus, Some("=?=not a directive"), || {
            tracing::warn!("default filter applies");
            tracing::info!("and still drops info");
        });
        assert_eq!(messages(&records), vec!["default filter applies"]);
    }

    #[test]
    fn pruning_keeps_the_log_directory_bounded() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Named like real session logs, created oldest first so both the mtime
        // order and the name order agree on which are newest.
        for index in 0..30 {
            let name = format!("2026-01-01T00-00-{index:02}-{index}.jsonl");
            std::fs::write(dir.path().join(name), b"{}\n").expect("write log");
        }
        // Neighbours in ~/.wizard/logs/ that belong to `crate::schedule`.
        std::fs::write(dir.path().join("scheduler.log"), b"started\n").expect("write log");
        std::fs::create_dir_all(dir.path().join("jobs")).expect("create jobs dir");

        prune(dir.path(), 5);

        let mut kept: Vec<String> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        kept.sort();
        assert_eq!(
            kept,
            vec![
                "2026-01-01T00-00-25-25.jsonl".to_string(),
                "2026-01-01T00-00-26-26.jsonl".to_string(),
                "2026-01-01T00-00-27-27.jsonl".to_string(),
                "2026-01-01T00-00-28-28.jsonl".to_string(),
                "2026-01-01T00-00-29-29.jsonl".to_string(),
                "jobs".to_string(),
                "scheduler.log".to_string(),
            ],
            "the newest five survive; the scheduler's own files are untouched"
        );

        // Idempotent: a second pass over a directory already at the cap is a
        // no-op rather than an off-by-one that keeps eating logs.
        prune(dir.path(), 5);
        let count = std::fs::read_dir(dir.path()).expect("read dir").count();
        assert_eq!(count, 7);
    }

    #[test]
    fn same_second_logs_are_evicted_by_pid_order_not_text_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Three wizards started inside one second, as a filesystem with
        // one-second mtime granularity sees them: identical mtimes, stems that
        // differ only in an unpadded pid. Compared as text, "…-9" sorts after
        // "…-10", so the newest of the three would be the first one deleted.
        let stamped = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_767_225_600);
        for pid in [2, 9, 10] {
            let path = dir.path().join(format!("2026-01-01T00-00-00-{pid}.jsonl"));
            std::fs::write(&path, b"{}\n").expect("write log");
            File::options()
                .write(true)
                .open(&path)
                .expect("open log")
                .set_modified(stamped)
                .expect("stamp log");
        }

        prune(dir.path(), 1);

        let kept: Vec<String> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            kept,
            vec!["2026-01-01T00-00-00-10.jsonl".to_string()],
            "the highest pid is the last process to have started in that second"
        );
    }

    #[test]
    fn creating_a_session_log_evicts_down_to_the_cap_but_never_itself() {
        let dir = tempfile::tempdir().expect("tempdir");
        for index in 0..MAX_SESSION_LOGS {
            let name = format!("2026-01-01T00-00-{index:02}-{index}.jsonl");
            std::fs::write(dir.path().join(name), b"{}\n").expect("write log");
        }

        // A new session's first event is what prunes, and the reservation in
        // `create` is why the file it is about to open cannot be the one
        // evicted (its own name sorts last: it is the newest).
        let path = dir.path().join("2026-01-01T00-01-00-99.jsonl");
        let mut log = SessionLog {
            path: path.clone(),
            file: None,
            written: 0,
            capped: false,
        };
        log.write_line(b"{\"fields\":{\"message\":\"first event\"}}\n")
            .expect("write");
        log.flush().expect("flush");

        let count = std::fs::read_dir(dir.path()).expect("read dir").count();
        assert_eq!(count, MAX_SESSION_LOGS, "the directory stays at its cap");
        assert!(
            path.exists(),
            "the session that did the pruning kept its own log"
        );
        assert!(
            std::fs::read_to_string(&path)
                .expect("readable")
                .contains("first event")
        );
    }

    #[test]
    fn the_log_directory_is_prepared_before_the_sink_is_locked() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sink = Sink::new(dir.path().join("logs").join("session.jsonl"));

        // Building the directory goes through `Config::ensure_dirs`, which
        // emits a `tracing::warn!` when it cannot chmod the tree, and on a
        // `WIZARD_HOME` that lives on exFAT, a CIFS/NFS mount or WSL DrvFs it
        // cannot. That warning comes straight back into this sink, through
        // `make_writer` and so into `Sink::lock`; `std::sync::Mutex` is not
        // reentrant, so if the directory were built with the lock held the
        // first warning of the session would hang the process forever.
        //
        // The hang itself cannot be staged in-process: it needs the *global*
        // default subscriber (tracing-core's scoped dispatcher has a
        // reentrancy guard of its own, and the one global default a process
        // gets is already spent by `stdio_probe`). So the invariant is
        // asserted directly, which also fails in a second instead of hanging
        // the suite for its whole timeout.
        let mut prepared = false;
        sink.prepare_dir_with(|dir| {
            prepared = true;
            assert!(
                sink.0.log.try_lock().is_ok(),
                "the sink must not be locked while the log directory is built: \
                 a chmod warning from Config::ensure_dirs re-enters this sink \
                 and would deadlock on its own mutex"
            );
            std::fs::create_dir_all(dir).expect("create the log directory");
        });
        assert!(prepared, "the first call builds the directory");

        // And only the first: the nested event arriving from inside `ensure`
        // has to stop at the flag rather than start the whole thing again.
        sink.prepare_dir_with(|_| panic!("the log directory was prepared twice"));

        // The sink is still usable afterwards, i.e. nothing above left the
        // mutex or the flag in a state that swallows events.
        tracing::subscriber::with_default(subscriber(sink.clone(), filter_from(None)), || {
            tracing::warn!("after the directory was prepared");
        });
        let logged =
            std::fs::read_to_string(dir.path().join("logs").join("session.jsonl")).expect("log");
        assert!(
            logged.contains("after the directory was prepared"),
            "{logged}"
        );
    }

    #[test]
    fn the_first_write_leaves_the_state_directory_private() {
        // Hermetic by construction: the child gets a `~/.wizard` of its own,
        // named after its pid and created at the process umask (0755 on a
        // stock distro). Nothing in the probe calls `Config::load`, which is
        // exactly the position `wizard mcp-serve`, `wizard usage` and `wizard
        // harness` are in: they dispatch before any config is loaded, so the
        // first logged event is the only thing that can tighten the tree.
        let (stdout, stderr) = run_probe("stdio_probe", PROBE_ENV, None);
        let path = probe_log_path(&stdout);
        let logs = path.parent().expect("the log has a directory");
        let home = logs.parent().expect("the logs dir lives under ~/.wizard");
        let private = |dir: &Path| {
            crate::platform::secrets::is_private_dir(dir)
                .unwrap_or_else(|err| panic!("{err:#}\nstderr: {stderr}"))
        };

        // A session log carries prompts, tool output and error detail; left at
        // the umask every other local user on the box can read it.
        assert!(
            private(logs),
            "{} is {}",
            logs.display(),
            crate::platform::secrets::protection_summary(logs)
        );
        assert!(
            private(home),
            "the first write left {} at {}",
            home.display(),
            crate::platform::secrets::protection_summary(home)
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn a_session_log_stops_growing_at_its_budget() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("capped.jsonl");
        let mut log = SessionLog {
            path: path.clone(),
            file: None,
            written: MAX_SESSION_BYTES - 1,
            capped: false,
        };

        log.append(b"{\"fields\":{\"message\":\"last one in\"}}\n")
            .expect("under budget");
        log.append(b"{\"fields\":{\"message\":\"over budget\"}}\n")
            .expect("over budget is not an error");
        log.append(b"{\"fields\":{\"message\":\"also dropped\"}}\n")
            .expect("over budget is not an error");
        log.flush().expect("flush");

        let raw = std::fs::read_to_string(&path).expect("readable");
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(
            lines.len(),
            2,
            "the last in-budget line plus one truncation notice: {raw}"
        );
        for line in &lines {
            let _: serde_json::Value =
                serde_json::from_str(line).expect("truncation keeps the file parseable");
        }
        assert!(lines[1].contains("byte budget"), "notice explains itself");
    }

    #[test]
    fn panics_are_recorded_with_a_backtrace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("panic.jsonl");
        // Deliberately started past the byte budget: the crash is the record
        // the budget may not eat, and a run long enough to spend 8 MiB of
        // trace is exactly the one that is going to crash.
        let sink = Sink(Arc::new(SinkState {
            log: Mutex::new(SessionLog {
                path: path.clone(),
                file: None,
                written: MAX_SESSION_BYTES + 1,
                capped: false,
            }),
            dir_prepared: AtomicBool::new(false),
        }));

        // The only way to hold a `PanicHookInfo` is to be the panic hook, so
        // it is installed for the length of these three panics and restored
        // straight after. Anything else in this binary that panics inside that
        // window lands in the same file, which is why the assertions search
        // the records rather than indexing them.
        let _serialised = PANIC_HOOK.lock().unwrap_or_else(PoisonError::into_inner);
        let hook_sink = sink.clone();
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| log_panic_to(&hook_sink, info)));
        let _ = std::panic::catch_unwind(|| panic!("index out of bounds"));
        let _ = std::panic::catch_unwind(|| panic!("slot {} was already taken", 7));
        let _ = std::panic::catch_unwind(|| std::panic::panic_any(42u32));
        std::panic::set_hook(previous);

        let raw = std::fs::read_to_string(&path).expect("the panic opened the log");
        let records: Vec<serde_json::Value> = raw
            .lines()
            .map(|line| serde_json::from_str(line).expect("every panic line is valid json"))
            .collect();
        let find = |message: &str| {
            records
                .iter()
                .find(|record| record["fields"]["message"] == message)
                .unwrap_or_else(|| panic!("no record for {message:?} in:\n{raw}"))
                .clone()
        };

        // `panic!("literal")` hands the hook a `&str` payload...
        let literal = find("index out of bounds");
        assert_eq!(literal["level"], "ERROR");
        assert_eq!(literal["target"], "wizard::panic");
        let location = literal["fields"]["location"]
            .as_str()
            .expect("the panic's location is recorded");
        assert!(
            location.contains("logging.rs"),
            "location points at the panicking line: {location}"
        );
        assert!(
            !literal["fields"]["backtrace"]
                .as_str()
                .expect("a backtrace is captured")
                .is_empty(),
            "`force_capture` means the crash report never depends on RUST_BACKTRACE"
        );

        // ...while a formatted `panic!` hands it a `String`, which used to be
        // the case that degraded to "non-string payload".
        let formatted = find("slot 7 was already taken");
        assert_eq!(formatted["target"], "wizard::panic");
        assert!(formatted["fields"]["location"].is_string());

        // And anything else is named rather than dropped.
        let other = find("panic with a non-string payload");
        assert_eq!(other["level"], "ERROR");
    }

    #[test]
    fn log_panic_is_inert_until_init_publishes_a_sink() {
        // `init` is the only thing that sets `SINK`, and the only test that
        // calls it runs in a child process (see `stdio_probe`), so this
        // process is permanently in the pre-`init` state every early panic
        // hits.
        assert!(
            SINK.get().is_none(),
            "no test in this process may install the global sink"
        );
        // The assertion is that this returns: a hook that unwrapped the sink
        // would panic inside a panic, which aborts the process and takes the
        // whole test binary with it rather than failing one test.
        let _serialised = PANIC_HOOK.lock().unwrap_or_else(PoisonError::into_inner);
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(log_panic));
        let _ = std::panic::catch_unwind(|| panic!("there is nowhere to record this yet"));
        std::panic::set_hook(previous);
    }

    /// Run one of this module's probe tests in a child process and return its
    /// stdout and stderr.
    ///
    /// `gate` is the environment variable that wakes that probe up, and
    /// `filter` is the `WIZARD_LOG` the child should see (`None` removes it, so
    /// a developer or CI job with `WIZARD_LOG=off` exported cannot have a probe
    /// log nothing and its assertions pass for the wrong reason). Each child
    /// gets a `~/.wizard` named after its own pid, which is what keeps these
    /// tests off the state the rest of this binary shares.
    fn run_probe(name: &str, gate: &str, filter: Option<&str>) -> (String, String) {
        let exe = std::env::current_exe().expect("test binary path");
        let mut command = std::process::Command::new(exe);
        command
            .args(["--exact", &format!("logging::tests::{name}"), "--nocapture"])
            .env(gate, "1");
        match filter {
            Some(filter) => command.env(FILTER_ENV, filter),
            None => command.env_remove(FILTER_ENV),
        };
        let output = command.output().expect("run the probe child");
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            output.status.success(),
            "{name} failed\nstdout: {stdout}\nstderr: {stderr}"
        );
        (stdout, stderr)
    }

    /// The session log path a probe printed for its parent.
    fn probe_log_path(stdout: &str) -> PathBuf {
        stdout
            .lines()
            .find_map(|line| line.strip_prefix(PROBE_PATH_PREFIX))
            .map(PathBuf::from)
            .unwrap_or_else(|| panic!("probe printed no log path\nstdout: {stdout}"))
    }

    /// The child half of [`subscriber_never_writes_to_stdout_or_stderr`]. Inert
    /// unless the parent set [`PROBE_ENV`].
    #[test]
    fn stdio_probe() {
        if std::env::var_os(PROBE_ENV).is_none() {
            return;
        }
        let path = init().expect("probe installs the real subscriber");
        // A global default can only be installed once. A second `init` has to
        // decline rather than panic or repoint the sink the panic hook holds.
        assert!(
            init().is_none(),
            "a second init must not overwrite the installed sink"
        );
        // Stand in for an install that predates the private tree (and for a
        // developer whose umask is already 0077, which would otherwise make
        // `the_first_write_leaves_the_state_directory_private` pass without
        // anything having tightened it). This is the child's own `~/.wizard`.
        if let Some(home) = path.parent().and_then(Path::parent) {
            crate::platform::secrets::expose_to_other_users(home)
                .expect("loosen the probe's own state dir");
        }
        tracing::error!("{PROBE_MARKER}");

        // A sink that cannot be opened is the only way to reach the fmt
        // layer's internal-error path, which is an `eprintln!` unless
        // `log_internal_errors(false)` turned it off. Its parent is a regular
        // file, so `create_dir_all` fails and every write fails with it.
        let blocked = path.with_file_name("blocked");
        std::fs::write(&blocked, b"not a directory").expect("plant the blocking file");
        let unwritable = Sink::new(blocked.join("session.jsonl"));
        tracing::subscriber::with_default(subscriber(unwritable, filter_from(None)), || {
            tracing::error!("{PROBE_MARKER} into a sink that cannot be opened");
        });

        // The probe's own `println!` is not the subscriber, so it proves
        // nothing about the sink; it just hands the parent the file to check.
        println!("{PROBE_PATH_PREFIX}{}", path.display());
    }

    /// The child half of [`a_panic_is_recorded_even_with_logging_off`]. Inert
    /// unless the parent set [`PANIC_PROBE_ENV`].
    #[test]
    fn panic_probe() {
        if std::env::var_os(PANIC_PROBE_ENV).is_none() {
            return;
        }
        let path = init().expect("probe installs the real subscriber");
        // Exactly the wiring `main` uses.
        std::panic::set_hook(Box::new(log_panic));
        let _ = std::panic::catch_unwind(|| panic!("{PANIC_MARKER}"));
        println!("{PROBE_PATH_PREFIX}{}", path.display());
    }

    #[test]
    fn a_panic_is_recorded_even_with_logging_off() {
        // The load-bearing property: a crash is recorded whatever the filter
        // says, because whoever hits one did not know to turn logging on
        // beforehand. Needs a real process for the same reason the stdio probe
        // does: `init` and `set_hook` are both once per process.
        let (stdout, stderr) = run_probe("panic_probe", PANIC_PROBE_ENV, Some("off"));
        let path = probe_log_path(&stdout);
        let logged = std::fs::read_to_string(&path).expect("the probe's session log");
        let record: serde_json::Value = logged
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find(|record| record["target"] == "wizard::panic")
            .unwrap_or_else(|| panic!("no panic record under WIZARD_LOG=off:\n{logged}"));
        assert_eq!(record["fields"]["message"], PANIC_MARKER);
        assert!(record["fields"]["location"].is_string(), "{record}");
        assert!(
            !record["fields"]["backtrace"]
                .as_str()
                .unwrap_or_default()
                .is_empty(),
            "{record}"
        );
        // `off` really was in force: nothing but the panic is in the file.
        assert!(
            !logged.contains("\"target\":\"wizard::logging::tests\""),
            "the filter was not off after all:\n{logged}"
        );
        // And the hook stays off the terminal it was called from.
        assert!(!stderr.contains(PANIC_MARKER), "stderr: {stderr}");

        if let Some(home) = path.parent().and_then(Path::parent) {
            let _ = std::fs::remove_dir_all(home);
        }
    }

    #[test]
    fn subscriber_never_writes_to_stdout_or_stderr() {
        // Proving this needs a whole process: file descriptors 1 and 2 are
        // process-wide, so redirecting them in-process would swallow the output
        // of every other test running alongside this one. So the probe runs as
        // a child of this test binary and the assertion is on its real stdout
        // and stderr.
        let (stdout, stderr) = run_probe("stdio_probe", PROBE_ENV, None);
        let path = probe_log_path(&stdout);
        let logged = std::fs::read_to_string(&path).expect("the probe's session log");
        // Without this the test would pass just as happily if the probe had
        // logged nothing at all.
        assert!(
            logged.contains(PROBE_MARKER),
            "the event reached the file: {logged}"
        );
        assert!(
            !stdout.contains(PROBE_MARKER),
            "an event reached stdout, which is `wizard acp`'s JSON-RPC transport:\n{stdout}"
        );
        assert!(
            !stderr.contains(PROBE_MARKER),
            "an event reached stderr, which the TUI shares with the terminal:\n{stderr}"
        );
        // The probe also logged into a sink that cannot be opened. With
        // `log_internal_errors(true)`, tracing-subscriber reports that failure
        // with an `eprintln!` of its own, which corrupts the frame or the
        // protocol just as thoroughly as the event would have. (Setting it to
        // `false` is what this pins. It cannot pin the line's *presence*: see
        // the comment on it in `subscriber`.)
        assert!(
            !stderr.contains("tracing-subscriber"),
            "a failed write printed the subscriber's own internal error:\n{stderr}"
        );

        // The child's `~/.wizard` is a temp directory of its own (see
        // `config::use_temp_wizard_dir`); nothing else will clean it up.
        if let Some(home) = path.parent().and_then(Path::parent) {
            let _ = std::fs::remove_dir_all(home);
        }
    }
}
