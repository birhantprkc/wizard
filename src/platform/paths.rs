//! Where things live on this OS.
//!
//! Unix keeps all of Wizard's state in one directory, `~/.wizard`, and always
//! has: config, sessions, logs, models, the deep-evolve checkout, the update
//! staging area. The XDG split (`~/.config`, `~/.local/state`, `~/.cache`) was
//! never adopted, and adopting it now would move every existing install's
//! state out from under it. So on Unix [`state_dir`], [`config_dir`] and
//! [`cache_dir`] all answer with the same path.
//!
//! They are still three functions, because Windows is where they diverge:
//! roaming config belongs in `%APPDATA%`, machine-local state and re-fetchable
//! downloads in `%LOCALAPPDATA%`, and a single directory for both is wrong on
//! a roaming profile. A caller that asks for the *purpose* it needs keeps
//! working when that split lands; a caller that writes `wizard_dir().join(..)`
//! for everything does not.
//!
//! The root itself is [`crate::config::Config::wizard_dir`] rather than
//! anything computed here. `WIZARD_HOME`, the first-call-wins pin, and the
//! redirect that keeps `cargo test` off the developer's real state all have to
//! have exactly one owner, and that owner is `config`. What this module
//! extracts is the *shape* of the tree, not its root.
//!
//! ## What has not moved yet
//!
//! Most of the tree still writes `Config::wizard_dir()?.join(..)` directly:
//! roughly forty call sites across eighteen modules (local_setup, sync,
//! server, doctor, config, update, evolve, usage, trust, theme,
//! session_registry, schedule, onboarding). Those are the migration this
//! module makes *possible*; they are not a migration it has already done, and
//! the Windows `%APPDATA%`/`%LOCALAPPDATA%` split cannot land until they move.
//! Read the paragraph above as the reason these functions exist, not as a
//! description of how the tree currently calls them.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::config::Config;

/// Root of Wizard's own state: `~/.wizard`, or wherever `WIZARD_HOME` points.
pub fn state_dir() -> Result<PathBuf> {
    Config::wizard_dir()
}

/// Where `config.toml` and its neighbours live. The same directory as
/// [`state_dir`] on Unix; `%APPDATA%\wizard` once Windows lands.
pub fn config_dir() -> Result<PathBuf> {
    state_dir()
}

/// Where re-fetchable downloads live (GGUF models, the llama.cpp build, the
/// unpacked release tarball). The same directory as [`state_dir`] on Unix;
/// `%LOCALAPPDATA%\wizard` once Windows lands, which is the half of the
/// profile that does not roam, and a 4 GB model must never follow a user onto
/// another machine.
pub fn cache_dir() -> Result<PathBuf> {
    state_dir()
}

/// `~/.wizard/logs`: debug traces and session JSONLs.
pub fn logs_dir() -> Result<PathBuf> {
    Config::logs_dir()
}

/// The shared system temp directory (`/tmp`, `%TEMP%`).
///
/// World-writable with predictable names, so: **nothing secret, and nothing
/// another local user winning a race could turn into a privilege escalation.**
/// A staged binary handed to `sudo install` from here is exactly that bug.
/// Use [`staging_dir`] instead for anything Wizard will later read back, run,
/// or hand to another program.
pub fn temp_dir() -> PathBuf {
    std::env::temp_dir()
}

/// A private scratch directory named `purpose`, under the state dir and
/// created 0700 (and re-restricted on every call, since a directory that
/// predates this code, or that someone loosened, must not stay loose).
///
/// This is the answer to every `std::env::temp_dir().join(...)` that holds
/// something Wizard cares about: update downloads, unpacked archives,
/// clipboard images, the editor scratch file for a composed prompt.
pub fn staging_dir(purpose: &str) -> Result<PathBuf> {
    // `purpose` is a literal at every call site; requiring exactly one ordinary
    // component keeps it that way rather than letting the first dynamic caller
    // escape the tree. `..` and `.` are components too, which is why this
    // matches on the kind rather than counting separators.
    let mut components = Path::new(purpose).components();
    let single_name = matches!(components.next(), Some(std::path::Component::Normal(_)))
        && components.next().is_none();
    if !single_name {
        bail!("staging directory name {purpose:?} must be a single path component");
    }
    let dir = state_dir()?.join(purpose);
    super::secrets::create_private_dir_strict(&dir)?;
    Ok(dir)
}

/// Create a symbolic link at `link` that resolves to `target`.
///
/// A path that says where something else lives is still a path, which is why
/// this sits here rather than in a module of its own. Wizard makes them for
/// real (`local_setup` links the llama.cpp binaries it built into
/// `~/.wizard/bin`) and, far more often, in tests: nearly every guard against
/// a planted name (the trust store's canonicalisation, the workspace escape
/// checks, the bundle writer, the hooks surface) is tested by pointing a link
/// somewhere it must not be followed to.
///
/// One function rather than the two Windows has, because every caller knows a
/// path and not a kind. The port chooses `symlink_file` or `symlink_dir` by
/// stat'ing `target`, and has to cope with the call failing outright:
/// creating a symlink on Windows needs Developer Mode or
/// `SeCreateSymbolicLinkPrivilege`, so "the API worked and the link is there"
/// is not a thing that platform guarantees the way this one does.
pub fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(not(unix))]
    {
        let _ = (target, link);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "symbolic links are not implemented on this platform yet",
        ))
    }
}

// There is deliberately no `user_bin_dir` here. Where a Wizard binary gets
// installed is `install.sh`'s decision (`~/.local/bin`, `$PREFIX/bin` on
// Termux) and the Rust side never makes it: `wizard update` and deep evolve
// both install over `current_exe()`, wherever that happens to be, and
// `local_setup` puts `llama-server` under `~/.wizard/bin`. A function here
// would have been a platform decision extracted from no call site, which is
// the failure mode this module was written to end rather than to commit.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_purpose_resolves_under_one_state_root() {
        // The Unix promise: one tree, so an install that predates the split
        // keeps finding its state. The functions are distinct because Windows
        // splits them, not because Unix does.
        let state = state_dir().expect("state dir");
        assert_eq!(config_dir().expect("config dir"), state);
        assert_eq!(cache_dir().expect("cache dir"), state);
        assert_eq!(logs_dir().expect("logs dir"), state.join("logs"));
        assert!(state.is_absolute(), "{} must be absolute", state.display());
    }

    #[test]
    fn staging_is_private_and_never_the_shared_temp_dir() {
        let dir = staging_dir("platform-test-staging").expect("staging dir");
        assert!(dir.is_dir());
        // Under the state dir, not the shared temp dir: on the update path the
        // staged file is the argument to `sudo install`.
        assert_eq!(
            dir.parent(),
            Some(state_dir().expect("state dir").as_path())
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dir).expect("stat").permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{} must be private", dir.display());
        }
        // Idempotent, and it re-tightens a directory someone loosened.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("chmod");
            let again = staging_dir("platform-test-staging").expect("staging dir again");
            let mode = std::fs::metadata(&again)
                .expect("stat")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(
                mode, 0o700,
                "an existing loose staging dir must be tightened"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_link_resolves_to_its_target_and_is_not_its_target() {
        // Both halves matter to the callers: the link has to *work* (the
        // llama.cpp binaries are run through one) and it has to remain
        // distinguishable from a real file (every guard that refuses to follow
        // one is tested by planting one).
        let tmp = tempfile::tempdir().expect("tempdir");
        let target = tmp.path().join("real");
        let link = tmp.path().join("link");
        std::fs::write(&target, b"contents").expect("write");

        symlink(&target, &link).expect("symlink");
        assert_eq!(std::fs::read(&link).expect("read through"), b"contents");
        assert!(
            link.symlink_metadata()
                .expect("lstat")
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::canonicalize(&link).ok(),
            target.canonicalize().ok()
        );

        // A name that is already taken fails rather than silently repointing.
        assert!(symlink(&target, &link).is_err());

        // A dangling link is a link: this is the shape the "the file it named
        // is gone" tests plant.
        let dangling = tmp.path().join("dangling");
        symlink(&tmp.path().join("absent"), &dangling).expect("symlink");
        assert!(dangling.symlink_metadata().is_ok());
        assert!(std::fs::read(&dangling).is_err());
    }

    #[test]
    fn a_staging_name_cannot_escape_the_state_dir() {
        // The guard exists so the first dynamic caller fails loudly instead of
        // writing outside the tree.
        for bad in ["..", "../evil", "a/b", "/etc", ""] {
            assert!(
                staging_dir(bad).is_err(),
                "{bad:?} must be rejected as a staging name"
            );
        }
    }
}
