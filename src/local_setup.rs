//! Self-provisioning for the local llama.cpp stack.
//!
//! The default install lays down no model and no runtime; picking "Local" in
//! onboarding (or writing a config by hand) names a GGUF tier and expects
//! Wizard to make it real. This module does that on demand:
//!
//! - [`download_gguf`] fetches a known tier's GGUF into place (resuming a
//!   partial download from an earlier, interrupted run), and
//! - [`install_llama_server`] installs llama.cpp's `llama-server` from the
//!   official GitHub releases into `~/.wizard/llama.cpp`, linked from
//!   `~/.wizard/bin/llama-server` — the same layout `install.sh` uses with
//!   `WIZARD_LOCAL=1`.
//!
//! Both report through the same [`Progress`] callback the server lifecycle
//! uses, so the work surfaces in the existing spinner.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;

use crate::config::Config;
use crate::hardware::GgufModel;
use crate::server::Progress;

/// GitHub repo serving prebuilt llama.cpp releases.
const LLAMACPP_REPO: &str = "ggml-org/llama.cpp";

/// `~/.wizard/models/` — where downloaded GGUFs live.
pub fn models_dir() -> Result<PathBuf> {
    Ok(Config::wizard_dir()?.join("models"))
}

/// `~/.wizard/bin/` — symlinks Wizard manages (llama-server).
pub fn bin_dir() -> Result<PathBuf> {
    Ok(Config::wizard_dir()?.join("bin"))
}

// ---------------------------------------------------------------------------
// GGUF download
// ---------------------------------------------------------------------------

/// Download `tier` to `dest`, resuming `dest.partial` if a previous attempt
/// was interrupted. Reports progress as whole-percent steps.
pub async fn download_gguf(tier: &GgufModel, dest: &Path, progress: Progress<'_>) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let partial = dest.with_extension("gguf.partial");
    let mut offset = std::fs::metadata(&partial).map(|m| m.len()).unwrap_or(0);

    progress(&format!(
        "downloading {} ({}) — several GB, one-time…",
        tier.file, tier.name
    ));

    let http = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(20))
        .build()
        .context("building HTTP client")?;
    let mut request = http.get(tier.url);
    if offset > 0 {
        request = request.header(reqwest::header::RANGE, format!("bytes={offset}-"));
    }
    let response = request
        .send()
        .await
        .with_context(|| format!("requesting {}", tier.url))?;

    // 206 resumes; a plain 200 means the server ignored the range, so the
    // partial file is dead weight and the download restarts from scratch.
    let resumed = response.status() == reqwest::StatusCode::PARTIAL_CONTENT;
    if !resumed {
        if !response.status().is_success() {
            bail!(
                "downloading {} failed: HTTP {}",
                tier.url,
                response.status()
            );
        }
        offset = 0;
    }
    let total = response
        .content_length()
        .map(|remaining| remaining + offset);

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(resumed)
        .write(true)
        .truncate(!resumed)
        .open(&partial)
        .with_context(|| format!("opening {}", partial.display()))?;

    let mut written = offset;
    let mut last_percent = 0u64;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("reading from {}", tier.url))?;
        std::io::Write::write_all(&mut file, &chunk)
            .with_context(|| format!("writing {}", partial.display()))?;
        written += chunk.len() as u64;
        if let Some(total) = total.filter(|total| *total > 0) {
            let percent = written * 100 / total;
            if percent > last_percent {
                last_percent = percent;
                progress(&format!(
                    "downloading {} — {percent}% of {:.1} GB",
                    tier.file,
                    total as f64 / 1e9
                ));
            }
        }
    }
    drop(file);

    if let Some(total) = total
        && written < total
    {
        bail!(
            "download of {} ended early ({written} of {total} bytes) — re-run to resume",
            tier.file
        );
    }
    std::fs::rename(&partial, dest)
        .with_context(|| format!("moving {} into place", partial.display()))?;
    progress(&format!("saved {}", dest.display()));
    Ok(())
}

// ---------------------------------------------------------------------------
// llama-server install
// ---------------------------------------------------------------------------

/// Install `llama-server` from the official llama.cpp releases: pick a
/// release asset for this machine (Vulkan build when a GPU and loader are
/// present, CPU otherwise), extract the whole release tree to
/// `~/.wizard/llama.cpp` (the binary resolves its shared libraries via an
/// `$ORIGIN` runpath, so the `.so` files must stay beside it), and link it
/// from `~/.wizard/bin/llama-server`. Returns the linked path.
pub async fn install_llama_server(progress: Progress<'_>) -> Result<PathBuf> {
    progress("llama-server not found — installing llama.cpp (official releases)…");

    let http = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(20))
        .user_agent("wizard")
        .build()
        .context("building HTTP client")?;

    let mut last_error = anyhow::anyhow!("no llama.cpp release asset matched this machine");
    for variant in asset_variants() {
        let Some(url) = asset_url(&http, &variant).await else {
            continue;
        };
        match install_from_asset(&http, &url, progress).await {
            Ok(path) => return Ok(path),
            Err(error) => {
                progress(&format!(
                    "the {variant} build did not work — trying the next variant"
                ));
                last_error = error;
            }
        }
    }
    Err(last_error.context(
        "could not install a prebuilt llama-server — install llama.cpp yourself \
         (https://github.com/ggml-org/llama.cpp) and re-run",
    ))
}

/// Candidate release-asset variants, preferred first. llama.cpp ships no
/// Linux CUDA asset; Vulkan is the prebuilt GPU backend (it falls back to CPU
/// at runtime), so try it when a GPU and a Vulkan loader are present.
fn asset_variants() -> Vec<String> {
    let suffix = if std::env::consts::ARCH == "aarch64" {
        "arm64"
    } else {
        "x64"
    };
    let mut variants = Vec::new();
    if gpu_detected() && vulkan_loader_present() {
        variants.push(format!("ubuntu-vulkan-{suffix}"));
    }
    variants.push(format!("ubuntu-{suffix}"));
    variants
}

fn gpu_detected() -> bool {
    crate::hardware::detect_memory().is_some_and(|d| d.source.starts_with("GPU VRAM"))
}

fn vulkan_loader_present() -> bool {
    if crate::server::on_path("vulkaninfo") {
        return true;
    }
    std::process::Command::new("ldconfig")
        .arg("-p")
        .output()
        .ok()
        .is_some_and(|out| String::from_utf8_lossy(&out.stdout).contains("libvulkan.so"))
}

/// Find the newest release asset URL for `variant`, scanning the releases API
/// (the most recent tag can still be mid-upload and missing some platforms).
async fn asset_url(http: &reqwest::Client, variant: &str) -> Option<String> {
    let body = http
        .get(format!(
            "https://api.github.com/repos/{LLAMACPP_REPO}/releases?per_page=8"
        ))
        .send()
        .await
        .ok()?
        .text()
        .await
        .ok()?;
    let needle = format!("-bin-{variant}.tar.gz");
    // The API body quotes each asset's browser_download_url; the first match
    // in document order is the newest release carrying this variant.
    body.split('"')
        .find(|token| {
            token.starts_with("https://") && token.contains("/llama-b") && token.ends_with(&needle)
        })
        .map(str::to_string)
}

/// Download and unpack one release asset, verify the binary runs, and move it
/// into place. Errors at any step let the caller try the next variant.
async fn install_from_asset(
    http: &reqwest::Client,
    url: &str,
    progress: Progress<'_>,
) -> Result<PathBuf> {
    let file_name = url.rsplit('/').next().unwrap_or("llamacpp.tar.gz");
    progress(&format!("downloading {file_name}…"));

    let work = Config::wizard_dir()?.join("tmp-llamacpp");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).with_context(|| format!("creating {}", work.display()))?;
    let result = install_from_asset_in(http, url, &work, progress).await;
    let _ = std::fs::remove_dir_all(&work);
    result
}

async fn install_from_asset_in(
    http: &reqwest::Client,
    url: &str,
    work: &Path,
    progress: Progress<'_>,
) -> Result<PathBuf> {
    let archive = work.join("llamacpp.tar.gz");
    let bytes = http
        .get(url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .with_context(|| format!("downloading {url}"))?
        .bytes()
        .await
        .with_context(|| format!("reading {url}"))?;
    std::fs::write(&archive, &bytes).with_context(|| format!("writing {}", archive.display()))?;

    let extracted = work.join("extracted");
    std::fs::create_dir_all(&extracted)?;
    let status = tokio::process::Command::new("tar")
        .arg("-xzf")
        .arg(&archive)
        .arg("-C")
        .arg(&extracted)
        .status()
        .await
        .context("running tar (is it installed?)")?;
    if !status.success() {
        bail!("tar failed to extract {}", archive.display());
    }

    let binary = find_file(&extracted, "llama-server")
        .with_context(|| format!("no llama-server inside {url}"))?;
    make_executable(&binary)?;
    // Sanity check before keeping it — a Vulkan build without a usable loader
    // (or a glibc mismatch) fails here.
    let runs = tokio::process::Command::new(&binary)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false);
    if !runs {
        bail!("the extracted llama-server does not run on this system");
    }

    // Keep the whole release tree next to the binary (shared libraries).
    let dest = Config::wizard_dir()?.join("llama.cpp");
    let source_tree = binary.parent().unwrap_or(&extracted).to_path_buf();
    let _ = std::fs::remove_dir_all(&dest);
    copy_tree(&source_tree, &dest)?;

    let bin = bin_dir()?;
    std::fs::create_dir_all(&bin).with_context(|| format!("creating {}", bin.display()))?;
    let link = bin.join("llama-server");
    let target = dest.join("llama-server");
    let _ = std::fs::remove_file(&link);
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &link)
        .with_context(|| format!("linking {}", link.display()))?;
    #[cfg(not(unix))]
    std::fs::copy(&target, &link).with_context(|| format!("copying to {}", link.display()))?;

    progress(&format!("installed llama-server to {}", dest.display()));
    Ok(link)
}

/// Depth-first search for a regular file named `name`.
fn find_file(dir: &Path, name: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut subdirs = Vec::new();
    for entry in entries.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        if path.is_file() && path.file_name().is_some_and(|n| n == name) {
            return Some(path);
        }
        if path.is_dir() {
            subdirs.push(path);
        }
    }
    subdirs.into_iter().find_map(|sub| find_file(&sub, name))
}

fn make_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("chmod {}", path.display()))?;
    }
    Ok(())
}

/// Recursively copy `src` into `dest` (created fresh), preserving the flat
/// layout llama.cpp releases use. Symlinks are followed.
fn copy_tree(src: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest).with_context(|| format!("creating {}", dest.display()))?;
    for entry in std::fs::read_dir(src).with_context(|| format!("reading {}", src.display()))? {
        let entry = entry?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        if from.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)
                .with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
            if is_executable_file(&from) {
                make_executable(&to)?;
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable_file(_path: &Path) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_variants_end_with_a_cpu_fallback() {
        let variants = asset_variants();
        assert!(!variants.is_empty());
        let last = variants.last().unwrap();
        assert!(
            last == "ubuntu-x64" || last == "ubuntu-arm64",
            "unexpected fallback variant {last}"
        );
        // Vulkan, when offered, is tried before the CPU build.
        if variants.len() == 2 {
            assert!(variants[0].contains("vulkan"));
        }
    }

    #[test]
    fn asset_url_scan_finds_the_first_matching_download_url() {
        // Mirrors the GitHub API body shape: quoted browser_download_urls.
        let body = r#"
            {"browser_download_url": "https://github.com/ggml-org/llama.cpp/releases/download/b1000/llama-b1000-bin-macos-arm64.tar.gz"},
            {"browser_download_url": "https://github.com/ggml-org/llama.cpp/releases/download/b1000/llama-b1000-bin-ubuntu-x64.tar.gz"},
            {"browser_download_url": "https://github.com/ggml-org/llama.cpp/releases/download/b999/llama-b999-bin-ubuntu-x64.tar.gz"}
        "#;
        let needle = "-bin-ubuntu-x64.tar.gz";
        let url = body
            .split('"')
            .find(|token| {
                token.starts_with("https://")
                    && token.contains("/llama-b")
                    && token.ends_with(needle)
            })
            .unwrap();
        assert!(url.contains("b1000"), "newest release wins: {url}");
        assert!(url.ends_with("llama-b1000-bin-ubuntu-x64.tar.gz"));
    }

    #[test]
    fn find_file_searches_nested_directories() {
        let dir = tempfile::tempdir().expect("tempdir");
        let nested = dir.path().join("build/bin");
        std::fs::create_dir_all(&nested).expect("mkdir");
        std::fs::write(nested.join("llama-server"), b"x").expect("write");
        std::fs::write(dir.path().join("llama-cli"), b"x").expect("write");
        let found = find_file(dir.path(), "llama-server").expect("found");
        assert!(found.ends_with("build/bin/llama-server"));
        assert!(find_file(dir.path(), "missing").is_none());
    }

    #[test]
    fn copy_tree_copies_files_and_preserves_executable_bits() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("src");
        std::fs::create_dir_all(src.join("sub")).expect("mkdir");
        std::fs::write(src.join("libx.so"), b"so").expect("write");
        std::fs::write(src.join("sub/data"), b"d").expect("write");
        std::fs::write(src.join("llama-server"), b"bin").expect("write");
        make_executable(&src.join("llama-server")).expect("chmod");

        let dest = dir.path().join("dest");
        copy_tree(&src, &dest).expect("copy");
        assert!(dest.join("libx.so").is_file());
        assert!(dest.join("sub/data").is_file());
        assert!(is_executable_file(&dest.join("llama-server")));
    }
}
