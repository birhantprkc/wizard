//! `wizard gui`: open Wizard GUI, the desktop app built from `gui/` and
//! shipped as its own release asset.
//!
//! The app is a separate program, so this only finds it and starts it. It
//! does not wait for it: the terminal comes back the way it does after
//! `open` or `code .`.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::platform::host::is_executable;

/// Where the installers are.
pub const RELEASES: &str = "https://github.com/teddytennant/wizard/releases/latest";

/// How to start the app on this machine.
#[derive(Debug, PartialEq, Eq)]
pub enum Launch {
    /// A macOS bundle, opened with `open`.
    Bundle(PathBuf),
    /// The app's binary, run with no arguments, which opens the window.
    Binary(PathBuf),
}

/// The app's binary name. The fork kept upstream Zeron's, so the bundle is
/// `Zeron.app` and the Linux tarball's `install.sh` puts `zeron` in
/// `~/.local/bin`.
fn binary_name(os: &str) -> &'static str {
    if os == "windows" {
        "zeron.exe"
    } else {
        "zeron"
    }
}

/// Find the installed app: on macOS the bundle in `/Applications` or
/// `~/Applications` first, then the binary on `path`, then in
/// `~/.local/bin`, which is where the Linux installer puts it and which is not
/// always on `PATH`.
pub fn find(os: &str, path: Option<&OsStr>, home: Option<&Path>) -> Option<Launch> {
    if os == "macos" {
        let mut bundles = vec![PathBuf::from("/Applications/Zeron.app")];
        if let Some(home) = home {
            bundles.push(home.join("Applications/Zeron.app"));
        }
        if let Some(bundle) = bundles.into_iter().find(|b| b.is_dir()) {
            return Some(Launch::Bundle(bundle));
        }
    }
    let name = binary_name(os);
    let mut dirs: Vec<PathBuf> = path
        .map(|p| std::env::split_paths(p).collect())
        .unwrap_or_default();
    if let Some(home) = home {
        dirs.push(home.join(".local/bin"));
    }
    dirs.into_iter()
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
        .map(Launch::Binary)
}

/// Start the app in `cwd` and return without waiting for it.
pub fn open(cwd: Option<&Path>) -> Result<()> {
    let home = dirs::home_dir();
    let path = std::env::var_os("PATH");
    let Some(launch) = find(std::env::consts::OS, path.as_deref(), home.as_deref()) else {
        bail!("Wizard GUI is not installed; download wizard-gui for this platform from {RELEASES}");
    };
    let mut cmd = match &launch {
        Launch::Bundle(bundle) => {
            let mut cmd = Command::new("open");
            cmd.arg(bundle);
            cmd
        }
        Launch::Binary(binary) => Command::new(binary),
    };
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("starting Wizard GUI ({launch:?})"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn executable(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[test]
    fn the_binary_on_path_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin/zeron");
        executable(&bin);
        let path =
            std::env::join_paths([dir.path().join("empty"), dir.path().join("bin")]).unwrap();
        assert_eq!(find("linux", Some(&path), None), Some(Launch::Binary(bin)));
    }

    /// The Linux installer writes to `~/.local/bin`, which a fresh login
    /// shell does not always have on `PATH`.
    #[test]
    fn the_linux_installers_location_is_found_off_path() {
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join(".local/bin/zeron");
        executable(&bin);
        assert_eq!(
            find("linux", Some(OsStr::new("")), Some(home.path())),
            Some(Launch::Binary(bin))
        );
    }

    #[test]
    fn a_bundle_in_the_users_applications_is_opened_as_a_bundle() {
        let home = tempfile::tempdir().unwrap();
        let bundle = home.path().join("Applications/Zeron.app");
        std::fs::create_dir_all(&bundle).unwrap();
        // Unless the machine running the test has the app installed system-wide.
        if Path::new("/Applications/Zeron.app").is_dir() {
            return;
        }
        assert_eq!(
            find("macos", None, Some(home.path())),
            Some(Launch::Bundle(bundle))
        );
    }

    #[test]
    fn nothing_installed_is_none() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(find("linux", Some(OsStr::new("")), Some(home.path())), None);
    }

    /// A file that is not executable is not the app.
    #[cfg(unix)]
    #[test]
    fn a_non_executable_file_is_skipped() {
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join(".local/bin/zeron");
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, "").unwrap();
        assert_eq!(find("linux", Some(OsStr::new("")), Some(home.path())), None);
    }
}
