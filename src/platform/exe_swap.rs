//! Replacing an executable that may be running right now.
//!
//! Both self-update and deep evolve end the same way: a freshly built or
//! downloaded binary has to take the place of the one currently executing.
//! Unix allows it: `rename(2)` within a directory is atomic, and the running
//! process keeps its old inode open until it exits, so the whole trick is to
//! make sure the file being renamed into place is complete. A plain
//! `fs::copy` over the destination has exactly the failure this avoids: an
//! interruption leaves a truncated 0755 file where the binary was.
//!
//! Windows cannot do this at all: an executing image is locked and cannot be
//! replaced or deleted. The port is not a different `rename` call, it is a
//! different sequence: rename the running `.exe` aside (Windows *does* allow
//! that), rename the staged file into position, and delete the displaced file
//! on the next start. [`swap_into_place`] is where that sequence goes, and
//! until it exists the non-Unix arm refuses loudly rather than half-installing.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

/// Install `source` over `dest`, keeping the displaced binary at
/// `<dest>.<backup_suffix>` (the returned path). `dest`'s directory must be
/// writable; the caller decides what to do when it is not (`wizard update`
/// escalates with `sudo` when a terminal is present).
///
/// The sequence is copy to a sibling scratch file, fsync, chmod, back the
/// current binary up (through a scratch file of its own), then one `rename`
/// onto `dest`. Every byte is written to a scratch path, so an interrupted
/// install leaves `dest` and the backup each holding either a whole binary or
/// nothing they did not already hold, never a truncated 0755 file. A
/// rename-aside-then-copy would instead have a window with no binary at all.
///
/// Every error is propagated. A caller that swallows this is claiming an
/// install that did not happen, which is how the next launch quietly runs the
/// old binary while the log says otherwise.
pub fn install_executable(source: &Path, dest: &Path, backup_suffix: &str) -> Result<PathBuf> {
    let file_name = dest
        .file_name()
        .and_then(|n| n.to_str())
        .with_context(|| format!("{} has no file name", dest.display()))?;
    let dir = dest
        .parent()
        .with_context(|| format!("{} has no parent directory", dest.display()))?;
    let backup = backup_path(dest, backup_suffix)?;
    // Hidden and pid-tagged so two concurrent installs cannot collide, and in
    // `dest`'s own directory so the final rename stays within one filesystem.
    let scratch = dir.join(format!(".{file_name}.new.{}", std::process::id()));

    let _ = std::fs::remove_file(&scratch);
    if let Err(err) = copy_and_sync(source, &scratch).and_then(|()| set_executable(&scratch)) {
        let _ = std::fs::remove_file(&scratch);
        return Err(err);
    }

    // Back the current binary up by copy rather than by rename: `dest` then
    // exists continuously, and the backup is a real file even if the rename
    // below never happens.
    //
    // Staged through a scratch file and renamed on, rather than copied
    // straight onto `backup`. Unlinking the old backup first and copying after
    // meant a copy that failed part-way — a full disk is the ordinary way —
    // left the previous binary deleted and the new one not written, so an
    // update that then failed had nothing to roll back to. Both names are
    // therefore only ever replaced by a rename of a complete file.
    if dest.exists() {
        let backup_scratch = dir.join(format!(".{file_name}.bak.{}", std::process::id()));
        let _ = std::fs::remove_file(&backup_scratch);
        let staged_backup = copy_and_sync(dest, &backup_scratch)
            // The way back has to be runnable: `--rollback` and the `.prev`
            // recipe both put the backup back by renaming it, which carries
            // its mode with it.
            .and_then(|()| set_executable(&backup_scratch))
            .and_then(|()| {
                std::fs::rename(&backup_scratch, &backup).with_context(|| {
                    format!("moving the backup into place at {}", backup.display())
                })
            });
        if let Err(err) = staged_backup {
            let _ = std::fs::remove_file(&backup_scratch);
            let _ = std::fs::remove_file(&scratch);
            return Err(err.context(format!(
                "backing up {} to {}",
                dest.display(),
                backup.display()
            )));
        }
    }

    swap_into_place(&scratch, dest)?;
    Ok(backup)
}

/// `<exe>.<suffix>` beside the executable. The suffix is the caller's
/// (`wizard.bak` for an update, `wizard.prev` for a deep evolve) so the two
/// never overwrite each other's way back.
pub fn backup_path(exe: &Path, suffix: &str) -> Result<PathBuf> {
    let file_name = exe
        .file_name()
        .and_then(|n| n.to_str())
        .with_context(|| format!("{} has no file name", exe.display()))?;
    Ok(exe.with_file_name(format!("{file_name}.{suffix}")))
}

/// Copy `source` to `dest` and flush it to disk. The fsync is the reason the
/// rename is safe across a power cut: without it the rename can land while the
/// file's contents are still only in the page cache.
fn copy_and_sync(source: &Path, dest: &Path) -> Result<()> {
    let mut input =
        std::fs::File::open(source).with_context(|| format!("opening {}", source.display()))?;
    // `create_new` (O_EXCL) rather than `create`: it refuses to follow a
    // symlink, so a scratch name someone else planted in the destination
    // directory fails the install instead of redirecting the copy.
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)
        .with_context(|| format!("writing {}", dest.display()))?;
    std::io::copy(&mut input, &mut out)
        .with_context(|| format!("copying {} to {}", source.display(), dest.display()))?;
    out.sync_all()
        .with_context(|| format!("flushing {} to disk", dest.display()))?;
    Ok(())
}

/// Move the staged file onto `dest`, the only platform-specific step of the
/// swap. `scratch` is removed when the move fails, so a failed install leaves
/// nothing behind.
#[cfg(unix)]
fn swap_into_place(scratch: &Path, dest: &Path) -> Result<()> {
    // Renaming over a running executable is fine on Unix: the running process
    // holds the old inode open, and `rename(2)` within a directory is atomic.
    std::fs::rename(scratch, dest).map_err(|err| {
        let _ = std::fs::remove_file(scratch);
        anyhow!(err).context(format!("installing the new binary to {}", dest.display()))
    })
}

/// See the module docs: an executing image is locked on Windows, so this is a
/// different sequence rather than a different call, and half-installing is
/// worse than not installing.
#[cfg(not(unix))]
fn swap_into_place(scratch: &Path, dest: &Path) -> Result<()> {
    let _ = std::fs::remove_file(scratch);
    anyhow::bail!(
        "replacing the running executable ({}) is not supported on this platform yet; \
         install the new binary manually",
        dest.display()
    )
}

/// Mark `path` executable (0755 on Unix). A no-op where the filesystem carries
/// no execute bit: on Windows an executable is one by extension.
pub fn set_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("chmod 0755 {}", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Mark `path` as an ordinary, non-executable file (0644 on Unix).
///
/// The inverse of [`set_executable`], and it exists for one case that is not
/// hypothetical: a scripted tool whose runtime *reads* the file (a LuaJIT
/// tool) can overwrite an earlier tool of the same name that was written to be
/// exec'd. Leaving the execute bit on hands a Lua file to the kernel as a
/// program, which fails with `Exec format error` at the least convenient
/// moment. A no-op where the filesystem carries no execute bit, for the same
/// reason [`set_executable`] is.
pub fn clear_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644))
            .with_context(|| format!("chmod 0644 {}", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Whether `path` is a file this platform would execute: any execute bit on
/// Unix, an executable extension on Windows.
///
/// Used to find `llama-server` on `PATH` and to decide which files in an
/// unpacked release keep their execute bit; both are questions about the OS,
/// not about the file's contents.
pub fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        // PATHEXT is the real answer; this is the subset every install has.
        let executable_extension =
            path.extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| {
                    ["exe", "bat", "cmd", "com"]
                        .iter()
                        .any(|known| ext.eq_ignore_ascii_case(known))
                });
        path.is_file() && executable_extension
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Suffix used by the tests only; the real ones live in `crate::update`.
    const TEST_SUFFIX: &str = "bak";

    #[test]
    fn the_swap_replaces_the_target_and_keeps_the_old_binary() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("new-wizard");
        let dest = dir.path().join("wizard");
        std::fs::write(&source, b"new binary").expect("write source");
        std::fs::write(&dest, b"old binary").expect("write dest");

        let backup = install_executable(&source, &dest, TEST_SUFFIX).expect("install");
        assert_eq!(std::fs::read(&dest).expect("read dest"), b"new binary");
        assert_eq!(std::fs::read(&backup).expect("read backup"), b"old binary");
        assert_eq!(backup, dir.path().join("wizard.bak"));
        assert!(
            is_executable(&dest),
            "the installed binary must be runnable"
        );
        assert!(
            is_executable(&backup),
            "the way back must be runnable too: `--rollback` and the `.prev` \
             recipe both restore it by renaming, which carries its mode along"
        );

        // Nothing left over: a scratch file beside the destination would be
        // picked up by the next `ls` in a user's ~/.local/bin.
        let strays: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".wizard"))
            .collect();
        assert!(strays.is_empty(), "scratch left behind: {strays:?}");
    }

    #[test]
    fn a_failed_install_leaves_the_old_binary_running() {
        // The rule the whole module exists for: `dest` is either the old
        // binary or the new one. A missing source must not truncate it.
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("wizard");
        std::fs::write(&dest, b"old binary").expect("write dest");

        let err = install_executable(&dir.path().join("absent"), &dest, TEST_SUFFIX)
            .expect_err("a missing source must fail");
        assert!(format!("{err:#}").contains("absent"), "got: {err:#}");
        assert_eq!(std::fs::read(&dest).expect("read dest"), b"old binary");
        assert!(!dir.path().join("wizard.bak").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_backup_leaves_the_previous_backup_intact() {
        use std::os::unix::fs::PermissionsExt;
        // The regression: the old backup was unlinked *before* the copy that
        // was meant to replace it, so a copy that failed — a full disk, an
        // unreadable source — left neither the old rollback copy nor a new
        // one, and the update it was protecting had nothing to go back to.
        if running_as_root() {
            eprintln!("skipping: running as root, where a 0000 file is still readable");
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("wizard");
        let source = dir.path().join("new-wizard");
        std::fs::write(&dest, b"v1").expect("write dest");
        std::fs::write(&source, b"v2").expect("write source");

        let backup = install_executable(&source, &dest, TEST_SUFFIX).expect("first install");
        assert_eq!(std::fs::read(&backup).expect("read backup"), b"v1");

        // Make the backup step fail while everything before it succeeds: the
        // live binary is what gets copied, and it cannot be opened.
        std::fs::write(&source, b"v3").expect("rewrite source");
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        let err = install_executable(&source, &dest, TEST_SUFFIX)
            .expect_err("an unreadable live binary must fail the backup");
        assert!(
            format!("{err:#}").contains("backing up"),
            "the error must name the step that failed: {err:#}"
        );

        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        assert_eq!(
            std::fs::read(&backup).expect("read backup"),
            b"v1",
            "the previous rollback copy must survive a failed backup"
        );
        assert_eq!(
            std::fs::read(&dest).expect("read dest"),
            b"v2",
            "and the live binary must be untouched"
        );

        let strays: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".wizard"))
            .collect();
        assert!(strays.is_empty(), "scratch left behind: {strays:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_read_only_directory_fails_the_install_rather_than_the_binary() {
        use std::os::unix::fs::PermissionsExt;
        // Root ignores the mode bits, so under uid 0 (the stock `rust` docker
        // image, and any CI container that did not drop privileges) the
        // install succeeds and there is nothing here to observe. Skip loudly:
        // the alternative, asserting `running_as_root()` in the success arm,
        // is a test that reports green while covering nothing.
        if running_as_root() {
            eprintln!("skipping: running as root, where a 0500 directory is still writable");
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let protected = dir.path().join("bin");
        std::fs::create_dir(&protected).expect("mkdir");
        let dest = protected.join("wizard");
        std::fs::write(&dest, b"old binary").expect("write dest");
        let source = dir.path().join("new-wizard");
        std::fs::write(&source, b"new binary").expect("write source");
        std::fs::set_permissions(&protected, std::fs::Permissions::from_mode(0o500))
            .expect("chmod");

        let err = install_executable(&source, &dest, TEST_SUFFIX)
            .expect_err("a read-only directory must fail the install");
        assert!(
            format!("{err:#}").contains("wizard"),
            "the error must name what it could not install: {err:#}"
        );
        assert_eq!(std::fs::read(&dest).expect("read dest"), b"old binary");
        std::fs::set_permissions(&protected, std::fs::Permissions::from_mode(0o700))
            .expect("chmod back");
    }

    #[cfg(unix)]
    fn running_as_root() -> bool {
        // SAFETY: `geteuid` takes nothing and cannot fail.
        unsafe { libc::geteuid() == 0 }
    }

    #[cfg(unix)]
    #[test]
    fn an_executable_can_be_replaced_while_it_is_running() {
        use std::time::Duration;

        // The property the module is named for. The child is still executing
        // the old file when the swap lands; on Unix it keeps its inode and
        // finishes normally, while the name now resolves to the new contents.
        let dir = tempfile::tempdir().expect("tempdir");
        // The shebang is resolved rather than written as `#!/bin/sh`: this
        // test execs the script, and Termux has no `/bin`.
        let shebang = crate::platform::shell::shebang();
        let running = dir.path().join("running");
        std::fs::write(&running, format!("{shebang}\nsleep 1\nexit 7\n")).expect("write script");
        set_executable(&running).expect("chmod");
        let replacement = dir.path().join("replacement");
        let replacement_body = format!("{shebang}\nexit 0\n");
        std::fs::write(&replacement, &replacement_body).expect("write replacement");

        let mut child = std::process::Command::new(&running).spawn().expect("spawn");
        // Far enough in that the kernel has the old image open.
        std::thread::sleep(Duration::from_millis(200));
        install_executable(&replacement, &running, "prev").expect("install over a running file");

        let status = child.wait().expect("wait");
        assert_eq!(
            status.code(),
            Some(7),
            "the running process must finish from its own image"
        );
        assert_eq!(
            std::fs::read_to_string(&running).expect("read"),
            replacement_body
        );
    }

    #[cfg(unix)]
    #[test]
    fn set_executable_and_is_executable_agree() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tool");
        std::fs::write(&path, b"#!/bin/sh\n").expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        assert!(!is_executable(&path));

        set_executable(&path).expect("chmod");
        assert!(is_executable(&path));
        assert_eq!(
            std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777,
            0o755
        );

        // And back: a file that is going to be *read* by an interpreter has to
        // lose the bit an earlier tool of the same name left on it.
        clear_executable(&path).expect("chmod");
        assert!(!is_executable(&path));
        assert_eq!(
            std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777,
            0o644
        );

        // A directory with the execute bit set is not an executable file.
        assert!(!is_executable(dir.path()));
        assert!(!is_executable(&dir.path().join("absent")));
    }
}
