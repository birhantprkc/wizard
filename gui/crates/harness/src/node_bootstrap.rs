//! Node.js for Pi on a machine that has none.
//!
//! pi.dev's installer needs Node 22.19+ and npm. When they are missing it
//! offers to install them, but only by asking on `/dev/tty`, and an app
//! launched from the Dock or a desktop launcher has no terminal, so the
//! installer printed "No terminal detected; install Node.js …" and exited 1.
//! That was every Mac without Homebrew node. The GUI does the step itself:
//! the official build from nodejs.org, checked against its SHASUMS256.txt,
//! unpacked into the same `pi-node/current` the installer would have used.

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::StreamExt;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::install_progress::Progress;
use crate::{CancellationToken, HarnessError};

const DIST: &str = "https://nodejs.org/dist/latest-v22.x";
const STALL_TIMEOUT: Duration = Duration::from_secs(60);

/// pi.dev/install.sh's floor.
pub(crate) fn new_enough(version: &semver::Version) -> bool {
    *version >= semver::Version::new(22, 19, 0)
}

/// The standalone Node's bin directory, when one is installed.
pub(crate) fn standalone_bin() -> Option<PathBuf> {
    crate::gui_path::pi_node_root()
        .map(|root| root.join("current").join("bin"))
        .filter(|bin| bin.join("node").is_file())
}

/// Whether Pi's installer will find a Node it accepts, and npm next to it.
pub(crate) fn has_usable_node() -> bool {
    if standalone_bin().is_some() {
        return true;
    }
    let Some(node) = crate::executable::find_on_paths("node", Vec::new()) else {
        return false;
    };
    crate::executable::binary_version(&node).is_some_and(|v| new_enough(&v))
        && crate::adapter_install::find_npm().is_some()
}

/// nodejs.org's names for this machine.
fn platform(os: &str, arch: &str) -> Option<(&'static str, &'static str)> {
    let os = match os {
        "macos" => "darwin",
        "linux" => "linux",
        _ => return None,
    };
    let arch = match arch {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        _ => return None,
    };
    Some((os, arch))
}

/// The `.tar.gz` for `os`/`arch` in a SHASUMS256.txt, with its checksum.
/// gzip rather than xz: every macOS and Linux `tar` reads it without an extra
/// decompressor.
fn pick(shasums: &str, os: &str, arch: &str) -> Option<(String, String)> {
    let (os, arch) = platform(os, arch)?;
    let suffix = format!("-{os}-{arch}.tar.gz");
    shasums.lines().find_map(|line| {
        let (sum, file) = line.split_once(char::is_whitespace)?;
        let file = file.trim();
        (file.starts_with("node-v") && file.ends_with(&suffix) && sum.len() == 64)
            .then(|| (file.to_string(), sum.to_ascii_lowercase()))
    })
}

fn failed(detail: impl std::fmt::Display) -> HarnessError {
    HarnessError::Install(format!("Downloading Node.js failed: {detail}"))
}

/// Install Node.js under `pi-node/` and return its bin directory.
pub(crate) async fn install(
    progress: &Progress,
    cancel: &CancellationToken,
) -> Result<PathBuf, HarnessError> {
    let root = crate::gui_path::pi_node_root()
        .ok_or_else(|| failed("HOME is not set, so there is nowhere to put it"))?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .build()
        .map_err(failed)?;
    let shasums = client
        .get(format!("{DIST}/SHASUMS256.txt"))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(failed)?
        .text()
        .await
        .map_err(failed)?;
    let (file, sum) =
        pick(&shasums, std::env::consts::OS, std::env::consts::ARCH).ok_or_else(|| {
            failed(format!(
                "nodejs.org has no build for {}-{}",
                std::env::consts::OS,
                std::env::consts::ARCH
            ))
        })?;
    std::fs::create_dir_all(&root)?;
    let archive = root.join(format!(".{file}.{}.part", std::process::id()));
    let result = async {
        download(
            &client,
            &format!("{DIST}/{file}"),
            &archive,
            &sum,
            progress,
            cancel,
        )
        .await?;
        progress.line(&format!("Unpacking {}", file.trim_end_matches(".tar.gz")));
        unpack(&archive, &root, file.trim_end_matches(".tar.gz")).await
    }
    .await;
    let _ = std::fs::remove_file(&archive);
    result?;
    standalone_bin().ok_or_else(|| failed("the unpacked archive has no bin/node"))
}

async fn download(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    sha256: &str,
    progress: &Progress,
    cancel: &CancellationToken,
) -> Result<(), HarnessError> {
    let response = client
        .get(url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(failed)?;
    let total = response.content_length();
    let mut body = response.bytes_stream();
    let mut file = tokio::fs::File::create(dest).await?;
    let mut digest = Sha256::new();
    let mut received = 0_u64;
    progress.bytes(0, total);
    loop {
        let chunk = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(HarnessError::Install("installation cancelled".into())),
            chunk = tokio::time::timeout(STALL_TIMEOUT, body.next()) => chunk.map_err(|_| {
                failed(format!("no data for {}s, check your network", STALL_TIMEOUT.as_secs()))
            })?,
        };
        match chunk {
            Some(Ok(bytes)) => {
                received += bytes.len() as u64;
                digest.update(&bytes);
                file.write_all(&bytes).await?;
                progress.bytes(received, total);
            }
            Some(Err(e)) => return Err(failed(e)),
            None => break,
        }
    }
    file.flush().await?;
    let actual = format!("{:x}", digest.finalize());
    if actual != sha256 {
        return Err(failed(format!(
            "checksum mismatch for {url} (expected {sha256}, got {actual})"
        )));
    }
    Ok(())
}

/// Unpack into `root/<name>` and point `root/current` at it, swapping the
/// link in one rename so a Pi that is running keeps a whole Node.
async fn unpack(archive: &Path, root: &Path, name: &str) -> Result<(), HarnessError> {
    let staging = root.join(format!(".unpack-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;
    let status = tokio::process::Command::new("tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(&staging)
        .stdin(std::process::Stdio::null())
        .status()
        .await?;
    if !status.success() {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(failed(format!("tar exited with {status}")));
    }
    let dest = root.join(name);
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::rename(staging.join(name), &dest)?;
    let _ = std::fs::remove_dir_all(&staging);
    #[cfg(unix)]
    {
        let link = root.join(format!(".current-{}", std::process::id()));
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&dest, &link)?;
        std::fs::rename(&link, root.join("current"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHASUMS: &str = "\
aaaa000000000000000000000000000000000000000000000000000000000001  node-v22.23.3-darwin-arm64.tar.gz
aaaa000000000000000000000000000000000000000000000000000000000002  node-v22.23.3-darwin-arm64.tar.xz
aaaa000000000000000000000000000000000000000000000000000000000003  node-v22.23.3-darwin-x64.tar.gz
aaaa000000000000000000000000000000000000000000000000000000000004  node-v22.23.3-linux-arm64.tar.gz
AAAA000000000000000000000000000000000000000000000000000000000005  node-v22.23.3-linux-x64.tar.gz
aaaa000000000000000000000000000000000000000000000000000000000006  node-v22.23.3-headers.tar.gz
";

    #[test]
    fn picks_the_gzip_build_for_each_machine() {
        let pick =
            |os, arch| pick(SHASUMS, os, arch).map(|(file, sum)| (file, sum[60..].to_string()));
        assert_eq!(
            pick("macos", "aarch64"),
            Some(("node-v22.23.3-darwin-arm64.tar.gz".into(), "0001".into()))
        );
        assert_eq!(
            pick("macos", "x86_64"),
            Some(("node-v22.23.3-darwin-x64.tar.gz".into(), "0003".into()))
        );
        // Checksums compare lowercase.
        assert_eq!(
            pick("linux", "x86_64"),
            Some(("node-v22.23.3-linux-x64.tar.gz".into(), "0005".into()))
        );
        assert_eq!(pick("windows", "x86_64"), None);
        assert_eq!(pick("linux", "riscv64"), None);
    }

    #[test]
    fn the_floor_is_pis() {
        let v = |s| semver::Version::parse(s).unwrap();
        assert!(!new_enough(&v("20.19.0")));
        assert!(!new_enough(&v("22.18.9")));
        assert!(new_enough(&v("22.19.0")));
        assert!(new_enough(&v("24.1.0")));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unpack_replaces_current_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let name = "node-v22.23.3-darwin-arm64";
        std::fs::create_dir_all(src.join(name).join("bin")).unwrap();
        std::fs::write(src.join(name).join("bin/node"), "new").unwrap();
        let archive = dir.path().join("node.tar.gz");
        let ok = std::process::Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(&src)
            .arg(name)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let root = dir.path().join("pi-node");
        std::fs::create_dir_all(root.join("node-v22.19.0-darwin-arm64/bin")).unwrap();
        std::os::unix::fs::symlink(
            root.join("node-v22.19.0-darwin-arm64"),
            root.join("current"),
        )
        .unwrap();
        unpack(&archive, &root, name).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("current/bin/node")).unwrap(),
            "new"
        );
        assert!(
            !root
                .join(format!(".unpack-{}", std::process::id()))
                .exists()
        );
    }
}
