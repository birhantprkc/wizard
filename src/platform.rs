//! Host-environment detection shared by install-adjacent code paths.
//!
//! Mirrors the early probes in `install.sh` (`is_nixos`, `is_termux`) so the
//! Rust side — self-update, on-demand llama.cpp install, doctor — makes the
//! same choices the installer already made. Pure filesystem/env checks; no I/O
//! beyond reading a few well-known paths and env vars.

use std::path::Path;

/// True when running under [Termux](https://termux.dev) on Android.
///
/// Termux is a Linux userspace rooted at `$PREFIX` (typically
/// `/data/data/com.termux/files/usr`). Stock glibc/musl release binaries do not
/// run there (Bionic libc, no FHS dynamic loader), there is no `sudo`, and the
/// only writable install location on `PATH` is `$PREFIX/bin`. Detected the same
/// way `install.sh` does so installer and runtime stay in lockstep.
pub fn is_termux() -> bool {
    if std::env::var_os("TERMUX_VERSION").is_some()
        || std::env::var_os("TERMUX_APP_PID").is_some()
    {
        return true;
    }
    // `PREFIX` is always set inside a Termux session; require the Termux app
    // data path so a coincidental `PREFIX` on a desktop host does not trip this.
    if let Ok(prefix) = std::env::var("PREFIX") {
        if prefix.contains("com.termux") {
            return true;
        }
    }
    Path::new("/data/data/com.termux/files/usr").is_dir()
}

/// True on NixOS. Detected the same way `install.sh` / [`crate::update`] do so
/// the musl-vs-gnu asset preference stays consistent.
pub fn is_nixos() -> bool {
    Path::new("/etc/NIXOS").exists()
        || Path::new("/run/current-system").exists()
        || std::fs::read_to_string("/etc/os-release")
            .map(|text| {
                text.lines().any(|line| {
                    let lower = line.to_ascii_lowercase();
                    lower == "id=nixos" || lower.starts_with("id=nixos")
                })
            })
            .unwrap_or(false)
}

/// Short, user-facing note for surfaces that need to explain why a prebuilt
/// Linux asset or desktop feature is unavailable on Termux. Empty when not
/// on Termux so callers can append unconditionally.
pub fn termux_prebuilt_hint() -> Option<&'static str> {
    if is_termux() {
        Some(
            "Termux has no matching prebuilt release asset (Android/Bionic). \
             Install with a source build: \
             curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh \
             | WIZARD_BUILD_FROM_SOURCE=1 bash \
             (lands in $PREFIX/bin). Update the same way, or rebuild from ~/.wizard/src.",
        )
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn termux_prebuilt_hint_is_none_off_termux() {
        // This host is a normal Linux/macOS CI or dev box, not Termux.
        // is_termux() may still be true if someone runs the suite inside
        // Termux; the hint must track the detector either way.
        if is_termux() {
            assert!(termux_prebuilt_hint().is_some());
            assert!(
                termux_prebuilt_hint()
                    .unwrap()
                    .contains("WIZARD_BUILD_FROM_SOURCE")
            );
        } else {
            assert!(termux_prebuilt_hint().is_none());
        }
    }

    #[test]
    fn is_termux_false_without_termux_markers() {
        // Guard the negative path: without TERMUX_* and without a Termux
        // PREFIX, a desktop/CI host must not be classified as Termux. If the
        // suite itself is running inside Termux this assertion is skipped —
        // the detector is doing the right thing there.
        if std::env::var_os("TERMUX_VERSION").is_some()
            || std::env::var_os("TERMUX_APP_PID").is_some()
            || std::env::var("PREFIX")
                .map(|p| p.contains("com.termux"))
                .unwrap_or(false)
            || Path::new("/data/data/com.termux/files/usr").is_dir()
        {
            assert!(is_termux());
            return;
        }
        assert!(!is_termux());
    }
}
