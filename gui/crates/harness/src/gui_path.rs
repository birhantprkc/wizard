//! Directories a GUI launch's PATH is missing.
//!
//! An app started from the Dock, Finder or a desktop launcher inherits
//! launchd's or the session manager's PATH (`/usr/bin:/bin:/usr/sbin:/sbin` on
//! macOS), not the one a terminal builds from shell init. The login-shell
//! snapshot in [`crate::shell_env`] recovers the rest when the user's shell
//! answers in time; these are the places CLIs and Node land on a stock
//! machine, searched even when it does not. Homebrew's prefix is missing from
//! a Dock launch on every Mac, and it is where `brew install node` puts npm.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

/// The well-known bin directories for `os` (`std::env::consts::OS`), in
/// search order. Only the ones that belong to `os` are listed; whether they
/// exist is the caller's concern.
pub(crate) fn well_known_bins_with(
    env: &impl Fn(&str) -> Option<OsString>,
    os: &str,
) -> Vec<PathBuf> {
    if os == "windows" {
        return Vec::new();
    }
    let home = env("HOME").filter(|h| !h.is_empty()).map(PathBuf::from);
    let mut dirs = Vec::new();
    if let Some(home) = &home {
        dirs.push(home.join(".local").join("bin"));
    }
    if let Some(node) = pi_node_bin_with(env) {
        dirs.push(node);
    }
    if os == "macos" {
        // Apple silicon Homebrew, then Intel Homebrew and most .pkg installers.
        dirs.push(PathBuf::from("/opt/homebrew/bin"));
        dirs.push(PathBuf::from("/opt/homebrew/sbin"));
        dirs.push(PathBuf::from("/usr/local/bin"));
    } else {
        dirs.push(PathBuf::from("/usr/local/bin"));
        dirs.push(PathBuf::from("/home/linuxbrew/.linuxbrew/bin"));
    }
    if let Some(home) = &home {
        dirs.push(home.join(".npm-global").join("bin"));
    }
    dirs
}

pub(crate) fn well_known_bins() -> Vec<PathBuf> {
    well_known_bins_with(&|key| std::env::var_os(key), std::env::consts::OS)
}

/// Where a standalone Node.js lives when neither the user nor a package
/// manager provided one: pi.dev's installer layout,
/// `${XDG_DATA_HOME:-~/.local/share}/pi-node/current`, which the GUI's own
/// bootstrap shares so either one's copy serves both.
pub(crate) fn pi_node_root_with(env: &impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    if let Some(data) = env("XDG_DATA_HOME").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(data).join("pi-node"));
    }
    env("HOME").filter(|h| !h.is_empty()).map(|home| {
        PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("pi-node")
    })
}

pub(crate) fn pi_node_bin_with(env: &impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    pi_node_root_with(env).map(|root| root.join("current").join("bin"))
}

/// A child's PATH: `front` first, then the current PATH, the login shell's,
/// and `back`, with empty and repeated entries dropped. Earlier wins, so an
/// explicit PATH entry keeps beating a well-known fallback.
pub fn compose(
    front: &[PathBuf],
    current: Option<&OsStr>,
    login_shell: Option<&OsStr>,
    back: &[PathBuf],
) -> Option<OsString> {
    let mut paths: Vec<PathBuf> = front.to_vec();
    for path in [current, login_shell].into_iter().flatten() {
        paths.extend(std::env::split_paths(path));
    }
    paths.extend(back.iter().cloned());
    let mut seen = std::collections::HashSet::new();
    paths.retain(|p| !p.as_os_str().is_empty() && seen.insert(p.clone()));
    std::env::join_paths(paths).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(values: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> + use<> {
        let map: HashMap<String, OsString> = values
            .iter()
            .map(|(k, v)| (k.to_string(), OsString::from(v)))
            .collect();
        move |key| map.get(key).cloned()
    }

    #[test]
    fn a_dock_launch_on_macos_gets_homebrew_and_the_users_bins() {
        let dirs = well_known_bins_with(&env(&[("HOME", "/Users/u")]), "macos");
        let dirs: Vec<_> = dirs.iter().map(|d| d.to_str().unwrap()).collect();
        assert_eq!(
            dirs,
            [
                "/Users/u/.local/bin",
                "/Users/u/.local/share/pi-node/current/bin",
                "/opt/homebrew/bin",
                "/opt/homebrew/sbin",
                "/usr/local/bin",
                "/Users/u/.npm-global/bin",
            ]
        );
    }

    #[test]
    fn linux_gets_usr_local_and_linuxbrew_but_not_homebrew() {
        let dirs = well_known_bins_with(&env(&[("HOME", "/home/u")]), "linux");
        assert!(dirs.contains(&PathBuf::from("/usr/local/bin")));
        assert!(dirs.contains(&PathBuf::from("/home/linuxbrew/.linuxbrew/bin")));
        assert!(!dirs.iter().any(|d| d.starts_with("/opt/homebrew")));
    }

    #[test]
    fn pi_node_follows_xdg_data_home() {
        let lookup = env(&[("HOME", "/Users/u"), ("XDG_DATA_HOME", "/data")]);
        assert_eq!(
            pi_node_bin_with(&lookup),
            Some(PathBuf::from("/data/pi-node/current/bin"))
        );
        // An empty XDG_DATA_HOME is unset, per the spec.
        let lookup = env(&[("HOME", "/Users/u"), ("XDG_DATA_HOME", "")]);
        assert_eq!(
            pi_node_bin_with(&lookup),
            Some(PathBuf::from("/Users/u/.local/share/pi-node/current/bin"))
        );
    }

    #[test]
    fn no_home_still_lists_system_dirs_and_windows_lists_nothing() {
        let dirs = well_known_bins_with(&env(&[]), "macos");
        assert_eq!(dirs.first(), Some(&PathBuf::from("/opt/homebrew/bin")));
        assert!(well_known_bins_with(&env(&[("HOME", "C:/u")]), "windows").is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn compose_keeps_order_and_drops_repeats() {
        let path = compose(
            &[PathBuf::from("/front")],
            Some(OsStr::new("/usr/bin:/bin::/front")),
            Some(OsStr::new("/opt/homebrew/bin:/usr/bin")),
            &[
                PathBuf::from("/opt/homebrew/bin"),
                PathBuf::from("/usr/local/bin"),
            ],
        )
        .unwrap();
        assert_eq!(
            path,
            OsString::from("/front:/usr/bin:/bin:/opt/homebrew/bin:/usr/local/bin")
        );
    }

    /// The launchd PATH plus nothing from the shell still reaches Homebrew.
    #[cfg(unix)]
    #[test]
    fn a_failed_shell_probe_still_reaches_homebrew_node() {
        let back = well_known_bins_with(&env(&[("HOME", "/Users/u")]), "macos");
        let path = compose(
            &[],
            Some(OsStr::new("/usr/bin:/bin:/usr/sbin:/sbin")),
            None,
            &back,
        )
        .unwrap();
        let dirs: Vec<_> = std::env::split_paths(&path).collect();
        assert_eq!(
            dirs[..4],
            ["/usr/bin", "/bin", "/usr/sbin", "/sbin"].map(PathBuf::from)
        );
        assert!(dirs.contains(&PathBuf::from("/opt/homebrew/bin")));
    }
}
