//! Spill: the bytes [`truncate_output`](super::truncate_output) would
//! otherwise throw away, kept on disk where the model can go and get them.
//!
//! Truncation used to be lossy. A 400 KB test log came back as a head, a tail,
//! and a note saying "rerun a narrower command", so the only way to see the
//! middle was to run the command again, pay for it again, and hope the second
//! run was narrow enough. The middle of a build log is usually where the first
//! error is, so that instruction sent the model back to the start of the work
//! it had just done.
//!
//! Spilling keeps the same preview and adds a path. The full text is written
//! to a private file and the model reads it back with `read_file` or searches
//! it with `search_files`, tools it already has, at whatever granularity the
//! question needs.
//!
//! ## Why the sink is ambient
//!
//! `truncate_output` has around forty call sites and takes no context. Passing
//! a spill directory to each of them would touch every tool in the tree to
//! deliver one behaviour change, so the sink lives in a process-wide slot that
//! a session installs once and `truncate_output` consults. An empty slot means
//! the old behaviour, byte for byte, which is what a unit test and a
//! short-lived subprocess want: neither has a session, and neither should be
//! scattering files under the temp dir.
//!
//! Nothing in this module installs the sink. A session does, by calling
//! [`install`] with [`SpillSink::for_session`] once it knows its id: at agent
//! construction, and again on `/clear`, which is where the image store is
//! reopened for the same reason. Until that call exists every tool keeps the
//! old lossy truncation, which is why an uninstalled slot has to stay a
//! supported state rather than a panic.
//!
//! ## Why a spill can fail without anyone hearing about it
//!
//! A spill is an optimisation on an already-degraded path. The output was too
//! big, so something is being lost either way; a read-only temp dir or a full
//! disk should cost the model the middle of one log, not the tool call. Every
//! failure here returns to the caller, which falls back to plain truncation.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use crate::platform::secrets;

/// Environment override for the spill root, for a box where the temp dir is
/// tiny, noexec-mounted, or shared in a way the user would rather avoid.
///
/// An environment variable rather than a config key because a spill directory
/// is a property of the machine a run happens on, not of the user's agent
/// setup: the same `config.toml` syncs to a laptop and a CI container that
/// disagree about where scratch space lives.
pub const SPILL_DIR_ENV: &str = "WIZARD_SPILL_DIR";

/// Bytes of randomness in a spill file's name prefix.
///
/// The name has to be unguessable, not merely unique. Both halves of that
/// matter on a shared machine: a predictable name in a world-writable temp
/// tree lets another local user plant a symlink at the path Wizard is about to
/// write (which [`secrets::create_private_file`] would refuse, but only after
/// the attacker has already learned what to aim at), and it lets them watch
/// for the file appearing. Eight bytes is 64 bits, far past the point where
/// guessing beats waiting.
const NAME_RANDOM_BYTES: usize = 8;

/// Longest sanitized suggestion kept in a file name. A suggestion is a hint
/// for a human reading `ls`, so it is truncated rather than rejected.
const MAX_SUGGESTION_LEN: usize = 64;

/// Name used when a suggestion sanitizes down to nothing.
const FALLBACK_NAME: &str = "output";

/// The process-wide sink, installed by the session and read by
/// [`truncate_output`](super::truncate_output).
///
/// A `Mutex<Option<..>>` rather than a `OnceLock` because `/clear` starts a new
/// session inside a running process, and a slot that can only be filled once
/// would leave the second conversation writing into the first one's directory.
/// A thread-local would not work at all: tool calls run on tokio's worker
/// threads, not on the thread that built the agent.
static SINK: Mutex<Option<Arc<SpillSink>>> = Mutex::new(None);

/// Somewhere to put the bytes that did not fit, private to this user and
/// scoped to one session.
#[derive(Debug)]
pub struct SpillSink {
    dir: PathBuf,
}

impl SpillSink {
    /// The sink for a session, under the spill root.
    ///
    /// The directory is named from a hash of the session id rather than the id
    /// itself. Session ids reach the spill root's path, which on a stock box is
    /// a directory every user on the machine can list, and a session id is a
    /// handle to the transcript under `~/.wizard/sessions/`. Hashing keeps the
    /// one-directory-per-session grouping without publishing which sessions
    /// exist.
    pub fn for_session(session_id: &str) -> Self {
        Self {
            dir: spill_root().join(session_dir_name(session_id)),
        }
    }

    /// A sink writing directly into `dir`. Used by tests, which want a
    /// directory they can inspect and delete.
    pub fn in_dir(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Where this sink's files land.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Write `text` to a fresh private file and return its path.
    ///
    /// `suggested_name` is a hint from the caller about what the content is;
    /// it is sanitized down to a single path segment, so a caller passing a
    /// tool name, a model-supplied string, or `../../.ssh/authorized_keys`
    /// all end up with a file inside this sink's directory and nowhere else.
    pub fn spill(&self, text: &str, suggested_name: &str) -> Result<PathBuf> {
        self.ensure_dir()?;
        let path = self.dir.join(format!(
            "{}-{}",
            random_hex(NAME_RANDOM_BYTES)?,
            safe_segment(suggested_name)
        ));
        write_new_private_file(&path, text.as_bytes())?;
        Ok(path)
    }

    /// Create the sink directory, and the root above it, owner-only.
    ///
    /// Lazily, on the first spill, because most sessions never overflow a
    /// budget and an empty directory per session is litter. Strict at both
    /// levels: the root is the shared, guessable part of the path, so a root
    /// this process cannot make private is one somebody else may already own,
    /// and refusing to write there (falling back to plain truncation) is the
    /// right answer rather than dropping a tool's full output into it.
    fn ensure_dir(&self) -> Result<()> {
        if let Some(root) = self.dir.parent() {
            secrets::create_private_dir_strict(root)?;
        }
        secrets::create_private_dir_strict(&self.dir)
    }
}

/// Install `sink` as the process-wide spill destination, replacing any
/// previous one. Called once per session.
pub fn install(sink: SpillSink) {
    *lock() = Some(Arc::new(sink));
}

/// Remove the installed sink, restoring the plain-truncation behaviour.
pub fn clear() {
    *lock() = None;
}

/// The installed sink, or `None` when nothing installed one.
pub fn installed() -> Option<Arc<SpillSink>> {
    lock().clone()
}

/// The sink slot, ignoring poisoning: a panic in a caller holding this lock
/// says nothing about the `Option<Arc<..>>` inside it, and refusing to spill
/// for the rest of the process because an unrelated test panicked would turn
/// one failure into every later output being truncated lossily.
fn lock() -> std::sync::MutexGuard<'static, Option<Arc<SpillSink>>> {
    SINK.lock().unwrap_or_else(|err| err.into_inner())
}

/// Root directory for every session's spill files.
///
/// The temp dir, not `~/.wizard/`, because spill files are scratch: they are
/// meaningful for the length of one conversation and the OS already knows how
/// to reclaim them. Putting them in the state tree would grow it without
/// bound and back them up on machines where the home directory is synced.
fn spill_root() -> PathBuf {
    match std::env::var_os(SPILL_DIR_ENV) {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => std::env::temp_dir().join("wizard-spill"),
    }
}

/// A short, stable, non-reversible directory name for a session id.
fn session_dir_name(session_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(session_id.as_bytes());
    hex_lower(&digest[..8])
}

/// Reduce an arbitrary suggestion to exactly one safe path segment.
///
/// The rule is allow-list, not deny-list: anything outside `[A-Za-z0-9._-]`
/// becomes `_`, which disposes of separators, `..`, NUL, newlines, shell
/// metacharacters and Windows drive letters in one pass instead of leaving a
/// list of special cases to keep current. Leading dots go too, so no
/// suggestion can produce `.`, `..`, or a name that hides from `ls`.
fn safe_segment(suggested: &str) -> String {
    let mut out = String::with_capacity(suggested.len().min(MAX_SUGGESTION_LEN));
    for ch in suggested.chars() {
        if out.len() >= MAX_SUGGESTION_LEN {
            break;
        }
        match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '_' | '-' => out.push(ch),
            _ => out.push('_'),
        }
    }
    let trimmed = out.trim_start_matches('.');
    if trimmed.is_empty() {
        FALLBACK_NAME.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Write `bytes` to `path`, which must not already exist.
///
/// [`secrets::create_private_file`] opens with `O_EXCL` and mode 0600, so a
/// file or a symlink already sitting at the path fails the open instead of
/// being followed or overwritten. That is the property that makes writing into
/// a shared temp tree safe: the classic attack is to guess the name and point
/// it at something worth clobbering, and `O_EXCL` refuses to traverse the link
/// at all.
fn write_new_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = secrets::create_private_file(path)?;
    file.write_all(bytes)
        .with_context(|| format!("writing {}", path.display()))?;
    file.flush()
        .with_context(|| format!("flushing {}", path.display()))?;
    Ok(())
}

/// `n` random bytes as lowercase hex, from the OS entropy source. `getrandom`
/// is already a dependency (OAuth PKCE, sync keys) and this crate takes no
/// `rand` dependency.
fn random_hex(n: usize) -> Result<String> {
    let mut bytes = vec![0u8; n];
    getrandom::fill(&mut bytes).context("gathering randomness for a spill file name")?;
    Ok(hex_lower(&bytes))
}

/// Lowercase hex encoding of a byte slice (small helper; no `hex` dependency).
fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Exclusive, test-only hold on the process-wide sink slot.
///
/// The slot is shared by every test in the binary and the tests run in
/// parallel, so a test that asserts "no sink installed means the old
/// behaviour" has to be the only one touching the slot while it runs. The
/// mutex gives it that; `Drop` puts the slot back to empty, so a failing
/// assertion cannot leave a sink pointed at a deleted temp directory for
/// whichever test acquires the lock next.
#[cfg(test)]
pub(crate) struct SinkHold {
    _guard: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl Drop for SinkHold {
    fn drop(&mut self) {
        clear();
    }
}

/// Take the sink slot for the duration of a test, installing `sink` (or
/// leaving it empty for `None`).
#[cfg(test)]
pub(crate) fn hold_sink(sink: Option<SpillSink>) -> SinkHold {
    static LOCK: Mutex<()> = Mutex::new(());
    let guard = LOCK.lock().unwrap_or_else(|err| err.into_inner());
    match sink {
        Some(sink) => install(sink),
        None => clear(),
    }
    SinkHold { _guard: guard }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spill_file_holds_the_whole_original_text() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sink = SpillSink::in_dir(tmp.path().join("session"));
        let text = format!("HEAD{}TAIL", "x".repeat(50_000));

        let path = sink.spill(&text, "log.txt").expect("spill");

        assert_eq!(
            std::fs::read_to_string(&path).expect("read the spill file"),
            text,
            "the point of spilling is that nothing is lost"
        );
    }

    #[test]
    fn two_identical_spills_do_not_collide() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sink = SpillSink::in_dir(tmp.path().join("session"));
        let text = "same bytes, twice";

        let first = sink.spill(text, "out.txt").expect("first spill");
        let second = sink.spill(text, "out.txt").expect("second spill");

        assert_ne!(first, second, "the random prefix separates them");
        assert_eq!(std::fs::read_to_string(&first).unwrap(), text);
        assert_eq!(std::fs::read_to_string(&second).unwrap(), text);
    }

    #[test]
    fn a_suggestion_cannot_escape_the_sink_directory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("session");
        let sink = SpillSink::in_dir(&dir);

        for hostile in [
            "../../etc/passwd",
            "..",
            "/absolute/path",
            "..\\windows\\system32",
            "sub/dir/file.txt",
            "",
            "...",
            "\0newline\nname",
        ] {
            let path = sink.spill("payload", hostile).expect("spill");
            assert_eq!(
                path.parent(),
                Some(dir.as_path()),
                "{hostile:?} landed outside the sink: {}",
                path.display()
            );
            let name = path.file_name().and_then(|n| n.to_str()).expect("a name");
            // One component, so nothing the suggestion said can be read as a
            // directory step. `_.._etc_passwd` is fine: the separators are what
            // made `..` mean anything, and they are gone.
            assert_eq!(
                Path::new(name).components().count(),
                1,
                "{hostile:?} produced more than one path segment: {name}"
            );
            assert!(
                !name.contains(['/', '\\']) && !name.starts_with('.'),
                "{hostile:?} kept a separator or hid itself: {name}"
            );
            assert!(
                name.len() > NAME_RANDOM_BYTES * 2,
                "{hostile:?} lost its random prefix: {name}"
            );
        }
    }

    #[test]
    fn a_planted_symlink_is_not_written_through() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let victim = tmp.path().join("victim.txt");
        std::fs::write(&victim, "do not clobber me").expect("write the victim");
        let planted = tmp.path().join("planted");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&victim, &planted).expect("plant the symlink");
        #[cfg(not(unix))]
        std::fs::write(&planted, "an existing file stands in for the link").expect("plant");

        let err = write_new_private_file(&planted, b"attacker's payload").unwrap_err();

        assert!(
            err.to_string().contains("creating"),
            "the refusal comes from the create step: {err}"
        );
        assert_eq!(
            std::fs::read_to_string(&victim).expect("the victim survives"),
            "do not clobber me"
        );
    }

    #[test]
    fn a_spill_file_is_owner_only_inside_an_owner_only_directory() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let tmp = tempfile::tempdir().expect("tempdir");
            let dir = tmp.path().join("session");
            let sink = SpillSink::in_dir(&dir);

            let path = sink
                .spill("secrets in a build log", "out.txt")
                .expect("spill");

            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&path), 0o600, "the file is owner-only");
            assert_eq!(mode(&dir), 0o700, "so is the directory holding it");
        }
    }

    #[test]
    fn session_directories_do_not_publish_the_session_id() {
        let name = session_dir_name("019507f3-0000-7000-8000-abcdef012345");

        assert!(!name.contains("019507f3"), "not the id itself: {name}");
        assert_eq!(name.len(), 16, "eight bytes of hex: {name}");
        assert_eq!(
            name,
            session_dir_name("019507f3-0000-7000-8000-abcdef012345"),
            "stable, so one session keeps one directory"
        );
        assert_ne!(name, session_dir_name("some other session"));
    }

    #[test]
    fn the_env_var_overrides_the_temp_dir_root() {
        // Reading the variable rather than setting it: `set_var` is unsafe in
        // this edition and the process environment is shared with every other
        // test in the binary. What is worth asserting is the default, which is
        // where the files land on a box that sets nothing.
        if std::env::var_os(SPILL_DIR_ENV).is_none() {
            let dir = SpillSink::for_session("abc").dir().to_path_buf();
            assert!(
                dir.starts_with(std::env::temp_dir()),
                "the default root is under the OS temp dir: {}",
                dir.display()
            );
            assert_ne!(
                dir.file_name(),
                Some(std::ffi::OsStr::new("abc")),
                "the session id is hashed, not used raw"
            );
        }
    }
}
