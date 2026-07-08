//! Self-update: the `wizard update` command and the passive startup check.
//!
//! Releases are published on GitHub as `wizard-<target>.tar.gz` (each tarball
//! holding a single `wizard` binary) with a companion `checksums.txt`. This
//! module picks the right asset for the machine (mirroring `install.sh`),
//! downloads it, verifies its sha256, and swaps it in atomically via a rename
//! in the same directory as the running executable — renaming over a running
//! binary is fine on Unix, and the displaced binary is kept as `<name>.bak`
//! for `--rollback`.
//!
//! The passive check ([`maybe_check_on_startup`]) is a courtesy notice by
//! default and only ever installs anything when `[update].auto` is set. It is
//! fire-and-forget so it never delays the TUI, and swallows every error.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};

use crate::config::{Config, UpdateConfig};

/// Default GitHub repo serving Wizard releases (overridable via `[update].repo`
/// so a fork can point elsewhere).
const DEFAULT_REPO: &str = "teddytennant/wizard";

/// HTTP timeout for the passive startup check — short so a hung network can
/// never leave the fire-and-forget task lingering.
const CHECK_TIMEOUT: Duration = Duration::from_secs(3);

/// HTTP timeout for the interactive `wizard update` command, which is allowed
/// to wait a little longer than the passive check.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// The compiled version of this binary (`CARGO_PKG_VERSION`). Always a full
/// three-component semver — release tags and self-update comparison depend on
/// it parsing.
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The version as shown to the user (`wizard --version`, the welcome banner):
/// a trailing `.0` patch is dropped, so `0.7.0` reads as `0.7` while
/// `0.7.1` stays `0.7.1`. Cosmetic only — never used for version comparison.
pub fn display_version() -> &'static str {
    let version = current_version();
    version.strip_suffix(".0").unwrap_or(version)
}

/// User-Agent the GitHub API requires (`wizard/<version>`).
fn user_agent() -> String {
    format!("wizard/{}", current_version())
}

/// Strip a single leading `v` (`v0.5.0` → `0.5.0`) for semver parsing.
fn strip_v(tag: &str) -> &str {
    tag.strip_prefix('v').unwrap_or(tag)
}

/// True when `latest` parses to a strictly greater semver than `current`.
/// Unparseable versions compare `false`, so a garbled tag degrades to "no
/// update" rather than an error.
fn is_newer(latest: &str, current: &str) -> bool {
    match (
        semver::Version::parse(strip_v(latest)),
        semver::Version::parse(strip_v(current)),
    ) {
        (Ok(l), Ok(c)) => l > c,
        _ => false,
    }
}

/// Ensure a user-supplied tag carries the leading `v` the release tags use.
fn normalize_tag(tag: &str) -> String {
    let trimmed = tag.trim();
    if trimmed.starts_with('v') {
        trimmed.to_string()
    } else {
        format!("v{trimmed}")
    }
}

/// Normalize `std::env::consts::ARCH` to the release naming (`x86_64` /
/// `aarch64`). `None` for architectures we publish no asset for.
fn normalize_arch(arch: &str) -> Option<&'static str> {
    match arch {
        "x86_64" | "amd64" => Some("x86_64"),
        "aarch64" | "arm64" => Some("aarch64"),
        _ => None,
    }
}

/// NixOS lacks the FHS dynamic loader the glibc (gnu) binary needs, so it must
/// prefer the static musl build. Detected the same way `install.sh` does.
fn is_nixos() -> bool {
    Path::new("/etc/NIXOS").exists() || Path::new("/run/current-system").exists()
}

/// Release-asset file names to try, most-preferred first — the pure decision
/// behind [`asset_candidates`], factored out so it is unit-testable. Mirrors
/// `install.sh`: macOS → the per-arch Darwin build; NixOS Linux → musl then
/// gnu (no FHS loader for the gnu build); other Linux → gnu then musl.
fn asset_candidates_for(os: &str, arch: &str, nixos: bool) -> Vec<String> {
    if os == "macos" {
        return vec![format!("wizard-{arch}-apple-darwin.tar.gz")];
    }
    let gnu = format!("wizard-{arch}-unknown-linux-gnu.tar.gz");
    let musl = format!("wizard-{arch}-unknown-linux-musl.tar.gz");
    if nixos {
        vec![musl, gnu]
    } else {
        vec![gnu, musl]
    }
}

/// The release-asset candidates for this machine, or an error on an
/// architecture we ship no binary for.
fn asset_candidates() -> Result<Vec<String>> {
    let arch = normalize_arch(std::env::consts::ARCH).ok_or_else(|| {
        anyhow!(
            "no prebuilt wizard release for this CPU architecture ({})",
            std::env::consts::ARCH
        )
    })?;
    Ok(asset_candidates_for(std::env::consts::OS, arch, is_nixos()))
}

/// Extract the expected sha256 hex for `asset` from a `checksums.txt` body
/// (`sha256sum` format: `<hex>  <name>`, optionally `*`-prefixed in binary
/// mode). `None` when the asset has no entry. Malformed lines are skipped.
fn parse_checksums(text: &str, asset: &str) -> Option<String> {
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let (Some(hex), Some(name)) = (parts.next(), parts.next()) else {
            continue;
        };
        let name = name.strip_prefix('*').unwrap_or(name);
        if name == asset {
            return Some(hex.to_ascii_lowercase());
        }
    }
    None
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

/// sha256 of a byte slice, lowercase hex.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_lower(&hasher.finalize())
}

/// The running executable, resolved through any symlinks so the rename lands
/// on the real file rather than a link.
fn current_exe_canonical() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("locating the current executable")?;
    exe.canonicalize()
        .with_context(|| format!("canonicalizing {}", exe.display()))
}

/// The rollback backup path for an executable (`<name>.bak`).
fn backup_path(exe: &Path) -> Result<PathBuf> {
    let file_name = exe
        .file_name()
        .and_then(|n| n.to_str())
        .context("the current executable has no file name")?;
    Ok(exe.with_file_name(format!("{file_name}.bak")))
}

/// Whether `dir` is writable, probed by creating (and removing) a temp file —
/// more reliable across ownership/ACL combinations than inspecting metadata.
fn dir_is_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".wizard-update-probe-{}", std::process::id()));
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Both stdin and stdout are a terminal — the only context in which it is safe
/// to escalate with `sudo` (a human is present to answer the prompt).
fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("chmod 0755 {}", path.display()))
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

/// Query the GitHub releases API for the newest tag (`tag_name`). Network and
/// rate-limit failures return `Err` so callers can degrade gracefully.
async fn fetch_latest_tag(repo: &str, timeout: Duration) -> Result<String> {
    let client = reqwest::Client::builder()
        .user_agent(user_agent())
        .timeout(timeout)
        .build()
        .context("building HTTP client")?;
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let body: serde_json::Value = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .with_context(|| format!("querying {url}"))?
        .json()
        .await
        .context("parsing the GitHub releases API response")?;
    body.get("tag_name")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .context("the GitHub releases API response had no tag_name")
}

/// GET a URL as text (used for `checksums.txt`).
async fn fetch_text(client: &reqwest::Client, url: &str) -> Result<String> {
    client
        .get(url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .with_context(|| format!("fetching {url}"))?
        .text()
        .await
        .with_context(|| format!("reading {url}"))
}

/// Stream a URL to `dest`.
async fn download_to(client: &reqwest::Client, url: &str, dest: &Path) -> Result<()> {
    let response = client
        .get(url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .with_context(|| format!("downloading {url}"))?;
    let mut out =
        std::fs::File::create(dest).with_context(|| format!("writing {}", dest.display()))?;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("reading {url}"))?;
        std::io::Write::write_all(&mut out, &chunk)
            .with_context(|| format!("writing {}", dest.display()))?;
    }
    std::io::Write::flush(&mut out).with_context(|| format!("writing {}", dest.display()))?;
    Ok(())
}

/// Extract the single `wizard` file from a gzip+tar `tarball` to `dest`.
fn extract_wizard(tarball: &Path, dest: &Path) -> Result<()> {
    let file =
        std::fs::File::open(tarball).with_context(|| format!("opening {}", tarball.display()))?;
    let gz = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(gz);
    for entry in archive.entries().context("reading the release tarball")? {
        let mut entry = entry.context("reading a release tarball entry")?;
        let path = entry
            .path()
            .context("a release tarball entry had a bad path")?;
        if path.file_name().and_then(|n| n.to_str()) == Some("wizard") {
            let mut out = std::fs::File::create(dest)
                .with_context(|| format!("writing {}", dest.display()))?;
            std::io::copy(&mut entry, &mut out)
                .with_context(|| format!("unpacking wizard to {}", dest.display()))?;
            return Ok(());
        }
    }
    bail!("the release tarball contained no `wizard` file");
}

/// Move `staged` into place at `dest_exe`, backing the current binary up to
/// `<name>.bak` first. When `writable`, `staged` lives in `dest_exe`'s own
/// directory and the swap is an atomic rename; otherwise (a protected dir like
/// `/usr/local/bin`) escalate via `sudo` when a terminal is present, else print
/// the manual command and error. `staged` is cleaned up on every path except
/// the last one, where it is intentionally left for the printed command.
fn install_over(staged: &Path, dest_exe: &Path, writable: bool) -> Result<()> {
    let backup = backup_path(dest_exe)?;

    if writable {
        let _ = std::fs::remove_file(&backup);
        if let Err(err) = std::fs::copy(dest_exe, &backup) {
            let _ = std::fs::remove_file(staged);
            return Err(anyhow!(err).context(format!(
                "backing up {} to {}",
                dest_exe.display(),
                backup.display()
            )));
        }
        // Renaming over a running executable is fine on Unix (the inode lives
        // on), and this is atomic because staged sits in the same directory.
        std::fs::rename(staged, dest_exe).map_err(|err| {
            let _ = std::fs::remove_file(staged);
            anyhow!(err).context(format!(
                "installing the new binary to {}",
                dest_exe.display()
            ))
        })?;
        Ok(())
    } else if interactive() {
        let status = std::process::Command::new("sudo")
            .arg("install")
            .arg("-m755")
            .arg(staged)
            .arg(dest_exe)
            .status()
            .with_context(|| format!("running sudo install for {}", dest_exe.display()))?;
        let _ = std::fs::remove_file(staged);
        if !status.success() {
            bail!("sudo install to {} failed", dest_exe.display());
        }
        Ok(())
    } else {
        // Leave the staged binary in place so the printed command works.
        bail!(
            "cannot write {} and no terminal to escalate — install manually:\n  \
             sudo install -m755 {} {}",
            dest_exe.display(),
            staged.display(),
            dest_exe.display()
        );
    }
}

/// Run `binary --version` as a sanity check: does it actually execute on this
/// system? Catches a libc/dynamic-loader mismatch — e.g. a prebuilt glibc or
/// musl release binary on NixOS, or an old glibc host — before it replaces a
/// working install with a dud. Mirrors the same guard in `install.sh`.
fn binary_runs(binary: &Path) -> bool {
    std::process::Command::new(binary)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Download the release for `tag` and swap it in at `dest_exe`. Walks the asset
/// candidates in order and installs the first that downloads, verifies, unpacks,
/// and — critically — actually runs on this machine. A checksum mismatch is a
/// hard error (abort, don't silently try another asset); a missing/entry-less
/// `checksums.txt` is a warning that proceeds unverified. The live binary is
/// only ever touched once a candidate passes every check, so a platform with no
/// runnable prebuilt (e.g. NixOS, which needs the Nix flake) fails cleanly with
/// the current binary left in place.
async fn download_and_install(repo: &str, tag: &str, dest_exe: &Path) -> Result<()> {
    let candidates = asset_candidates()?;
    let dest_dir = dest_exe
        .parent()
        .context("the current executable has no parent directory")?;

    // Stage in `dest_dir` when it is writable, so the final swap is an atomic
    // rename on one filesystem. When it is not (e.g. `/usr/local/bin`), stage
    // in the system temp dir instead and finish the move with `sudo install`
    // — otherwise we could not even download here.
    let writable = dir_is_writable(dest_dir);
    let scratch = if writable {
        dest_dir.to_path_buf()
    } else {
        std::env::temp_dir()
    };

    let client = reqwest::Client::builder()
        .user_agent(user_agent())
        .connect_timeout(Duration::from_secs(20))
        .build()
        .context("building HTTP client")?;

    let base = format!("https://github.com/{repo}/releases/download/{tag}");
    // Published once per release, alongside every asset. Best-effort: absence
    // downgrades to an unverified install rather than failing.
    let checksums = fetch_text(&client, &format!("{base}/checksums.txt"))
        .await
        .ok();

    let pid = std::process::id();
    let mut unrunnable: Vec<String> = Vec::new();
    let mut last_err = anyhow!("no release asset for {tag} could be downloaded");

    for asset in &candidates {
        // 1. Download. A 404 (some platforms publish only musl or only gnu)
        //    just moves on to the next candidate.
        let tarball = scratch.join(format!(".{asset}.{pid}.part"));
        if let Err(err) = download_to(&client, &format!("{base}/{asset}"), &tarball).await {
            let _ = std::fs::remove_file(&tarball);
            last_err = err;
            continue;
        }

        // 2. Verify. A mismatch means corruption or tampering — abort the whole
        //    update rather than reaching for a different asset.
        match checksums.as_deref().and_then(|t| parse_checksums(t, asset)) {
            Some(expected) => {
                let data = std::fs::read(&tarball)
                    .with_context(|| format!("reading {}", tarball.display()))?;
                if sha256_hex(&data) != expected {
                    let _ = std::fs::remove_file(&tarball);
                    bail!(
                        "checksum mismatch for {asset} — expected {expected}, got {}; \
                         aborting update",
                        sha256_hex(&data)
                    );
                }
            }
            None if checksums.is_none() => {
                eprintln!(
                    "warning: could not fetch checksums.txt — installing without verification"
                )
            }
            None => eprintln!(
                "warning: checksums.txt has no entry for {asset} — installing without verification"
            ),
        }

        // 3. Unpack + chmod.
        let staged = scratch.join(format!(".wizard.update.{pid}"));
        let _ = std::fs::remove_file(&staged);
        let extracted = extract_wizard(&tarball, &staged).and_then(|()| set_executable(&staged));
        let _ = std::fs::remove_file(&tarball);
        if let Err(err) = extracted {
            let _ = std::fs::remove_file(&staged);
            last_err = err;
            continue;
        }

        // 4. Sanity check — the binary must run here before we replace a working
        //    one with it.
        if !binary_runs(&staged) {
            let _ = std::fs::remove_file(&staged);
            unrunnable.push(asset.clone());
            last_err = anyhow!("the binary from {asset} does not run on this system");
            continue;
        }

        // 5. Swap it in (backs the current binary up to `<name>.bak` first).
        return install_over(&staged, dest_exe, writable);
    }

    if unrunnable.is_empty() {
        Err(last_err)
    } else {
        Err(last_err.context(format!(
            "no prebuilt wizard binary runs on this system (tried {}); the current binary \
             is unchanged. On NixOS, install via the Nix flake (see the README) rather than \
             `wizard update`.",
            unrunnable.join(", ")
        )))
    }
}

/// Restore the pre-update binary from `<name>.bak`.
fn rollback_binary(dest_exe: &Path) -> Result<i32> {
    let backup = backup_path(dest_exe)?;
    if !backup.exists() {
        bail!(
            "no backup at {} — nothing to roll back to",
            backup.display()
        );
    }
    let dest_dir = dest_exe
        .parent()
        .context("the current executable has no parent directory")?;

    if dir_is_writable(dest_dir) {
        std::fs::rename(&backup, dest_exe).with_context(|| {
            format!("restoring {} from {}", dest_exe.display(), backup.display())
        })?;
    } else if interactive() {
        let status = std::process::Command::new("sudo")
            .arg("install")
            .arg("-m755")
            .arg(&backup)
            .arg(dest_exe)
            .status()
            .with_context(|| format!("running sudo install for {}", dest_exe.display()))?;
        if !status.success() {
            bail!("sudo install to {} failed", dest_exe.display());
        }
        let _ = std::fs::remove_file(&backup);
    } else {
        bail!(
            "cannot write {} and no terminal to escalate — restore manually:\n  \
             sudo install -m755 {} {}",
            dest_exe.display(),
            backup.display(),
            dest_exe.display()
        );
    }
    println!("rolled back to the previous binary — restart wizard to use it.");
    Ok(0)
}

/// The `wizard update` command handler. Returns the process exit code.
pub async fn run(check: bool, to: Option<String>, force: bool, rollback: bool) -> Result<i32> {
    let dest_exe = current_exe_canonical()?;

    if rollback {
        return rollback_binary(&dest_exe);
    }

    let repo = DEFAULT_REPO;
    let current = current_version();

    let tag = match to {
        Some(tag) => normalize_tag(&tag),
        None => fetch_latest_tag(repo, COMMAND_TIMEOUT)
            .await
            .context("could not determine the latest release from GitHub")?,
    };
    let newer = is_newer(&tag, current);

    if check {
        println!("current: v{current}");
        println!("latest:  {tag}");
        if newer {
            println!("update available — run `wizard update`");
        } else {
            println!("up to date");
        }
        return Ok(0);
    }

    if !newer && !force {
        println!("already up to date (v{current})");
        return Ok(0);
    }

    println!("downloading {tag}…");
    download_and_install(repo, &tag, &dest_exe)
        .await
        .with_context(|| format!("updating to {tag}"))?;
    println!("updated v{current} → {tag} — restart wizard to use it.");
    Ok(0)
}

// ---------------------------------------------------------------------------
// Passive startup check
// ---------------------------------------------------------------------------

/// Cache under `~/.wizard/update-check.json` that throttles the startup check
/// to `interval_hours`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct UpdateCache {
    last_check_unix: u64,
    latest_tag: String,
}

fn cache_path() -> Result<PathBuf> {
    Ok(Config::wizard_dir()?.join("update-check.json"))
}

fn read_cache() -> Option<UpdateCache> {
    let raw = std::fs::read_to_string(cache_path().ok()?).ok()?;
    serde_json::from_str(&raw).ok()
}

fn write_cache(cache: &UpdateCache) {
    if let Ok(path) = cache_path()
        && let Ok(json) = serde_json::to_string(cache)
    {
        let _ = std::fs::write(path, json);
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Fire-and-forget passive update check, spawned so it never delays the TUI.
/// Governed entirely by `[update]` config; swallows every error. When `auto`
/// is set and the binary is writable without escalation it installs in the
/// background (taking effect on the next launch); otherwise it prints a single
/// notice line when a newer release exists.
pub async fn maybe_check_on_startup(cfg: &UpdateConfig) {
    if !cfg.notify && !cfg.auto {
        return;
    }
    let cfg = cfg.clone();
    tokio::spawn(async move {
        let _ = check_and_maybe_apply(cfg).await;
    });
}

async fn check_and_maybe_apply(cfg: UpdateConfig) -> Result<()> {
    let interval_secs = cfg.interval_hours.saturating_mul(3600);
    let now = now_unix();
    let cached = read_cache();

    let due = match &cached {
        Some(c) => now.saturating_sub(c.last_check_unix) >= interval_secs,
        None => true,
    };

    let latest = if due {
        match fetch_latest_tag(&cfg.repo, CHECK_TIMEOUT).await {
            Ok(tag) => {
                write_cache(&UpdateCache {
                    last_check_unix: now,
                    latest_tag: tag.clone(),
                });
                tag
            }
            // Network / rate-limit hiccup: stay silent, try again next cadence.
            Err(_) => return Ok(()),
        }
    } else {
        match cached.and_then(|c| (!c.latest_tag.is_empty()).then_some(c.latest_tag)) {
            Some(tag) => tag,
            None => return Ok(()),
        }
    };

    let current = current_version();
    if !is_newer(&latest, current) {
        return Ok(());
    }

    // The `notify` line is surfaced synchronously from the refreshed cache by
    // `print_startup_notice`, *before* the TUI takes the screen — never from
    // this task. A `println!` into the alternate-screen, raw-mode TUI would be
    // invisible or corrupt the display, so the only action left here is the
    // opt-in auto-apply.
    if cfg.auto {
        // A background task must never invoke sudo, so only auto-apply when the
        // binary is writable without escalation. The swapped-in binary takes
        // effect on the next launch; this is intentionally silent for the same
        // alternate-screen reason.
        if let Ok(exe) = current_exe_canonical()
            && let Some(dir) = exe.parent()
            && dir_is_writable(dir)
        {
            let _ = download_and_install(&cfg.repo, &latest, &exe).await;
        }
    }
    Ok(())
}

/// The passive "update available" line for a cached `latest` tag, or `None`
/// when it is empty or not newer than `current`. Pure, so it is unit-testable.
fn notice_line(latest: &str, current: &str) -> Option<String> {
    if !latest.is_empty() && is_newer(latest, current) {
        Some(format!(
            "wizard {latest} available (you have v{current}) — run `wizard update`"
        ))
    } else {
        None
    }
}

/// Print the passive notice synchronously and from the cache only (no network),
/// so it lands cleanly on stdout *before* the TUI enters the alternate screen.
/// The background [`maybe_check_on_startup`] task refreshes that cache, so a
/// freshly published release is announced on the next launch. Gated on a real
/// terminal and on `[update].notify`.
pub fn print_startup_notice(cfg: &UpdateConfig) {
    if !cfg.notify || !std::io::stdout().is_terminal() {
        return;
    }
    if let Some(cache) = read_cache()
        && let Some(line) = notice_line(&cache.latest_tag, current_version())
    {
        println!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_newer_compares_semver_and_strips_v() {
        assert!(is_newer("v0.5.1", "0.5.0"));
        assert!(is_newer("0.6.0", "0.5.9"));
        assert!(is_newer("v1.0.0", "v0.9.9"));
        assert!(!is_newer("v0.5.0", "0.5.0"));
        assert!(!is_newer("v0.4.9", "0.5.0"));
        // Unparseable versions degrade to "no update".
        assert!(!is_newer("latest", "0.5.0"));
        assert!(!is_newer("v0.5.1", "not-a-version"));
    }

    #[test]
    fn display_version_drops_a_trailing_zero_patch_only() {
        assert_eq!("0.7.0".strip_suffix(".0").unwrap_or("0.7.0"), "0.7");
        assert_eq!("0.7.1".strip_suffix(".0").unwrap_or("0.7.1"), "0.7.1");
        assert_eq!("0.10.0".strip_suffix(".0").unwrap_or("0.10.0"), "0.10");
        // The compiled version stays a full, parseable semver for comparison.
        assert!(semver::Version::parse(current_version()).is_ok());
        // The display never adds a component the real version lacks.
        assert!(current_version().starts_with(display_version()));
    }

    #[test]
    fn normalize_tag_adds_leading_v() {
        assert_eq!(normalize_tag("0.5.0"), "v0.5.0");
        assert_eq!(normalize_tag("v0.5.0"), "v0.5.0");
        assert_eq!(normalize_tag("  0.5.0  "), "v0.5.0");
    }

    #[test]
    fn normalize_arch_maps_known_and_rejects_unknown() {
        assert_eq!(normalize_arch("x86_64"), Some("x86_64"));
        assert_eq!(normalize_arch("amd64"), Some("x86_64"));
        assert_eq!(normalize_arch("aarch64"), Some("aarch64"));
        assert_eq!(normalize_arch("arm64"), Some("aarch64"));
        assert_eq!(normalize_arch("riscv64"), None);
    }

    #[test]
    fn asset_candidates_macos_is_single_darwin_build() {
        assert_eq!(
            asset_candidates_for("macos", "aarch64", false),
            vec!["wizard-aarch64-apple-darwin.tar.gz".to_string()]
        );
        assert_eq!(
            asset_candidates_for("macos", "x86_64", true),
            vec!["wizard-x86_64-apple-darwin.tar.gz".to_string()]
        );
    }

    #[test]
    fn asset_candidates_nixos_prefers_musl_then_gnu() {
        assert_eq!(
            asset_candidates_for("linux", "x86_64", true),
            vec![
                "wizard-x86_64-unknown-linux-musl.tar.gz".to_string(),
                "wizard-x86_64-unknown-linux-gnu.tar.gz".to_string(),
            ]
        );
    }

    #[test]
    fn asset_candidates_plain_linux_prefers_gnu_then_musl() {
        assert_eq!(
            asset_candidates_for("linux", "aarch64", false),
            vec![
                "wizard-aarch64-unknown-linux-gnu.tar.gz".to_string(),
                "wizard-aarch64-unknown-linux-musl.tar.gz".to_string(),
            ]
        );
    }

    #[test]
    fn parse_checksums_finds_the_asset() {
        let text = "\
aaaa1111  wizard-x86_64-unknown-linux-gnu.tar.gz
bbbb2222  wizard-x86_64-unknown-linux-musl.tar.gz
cccc3333  wizard-aarch64-apple-darwin.tar.gz
";
        assert_eq!(
            parse_checksums(text, "wizard-x86_64-unknown-linux-musl.tar.gz"),
            Some("bbbb2222".to_string())
        );
        assert_eq!(
            parse_checksums(text, "wizard-aarch64-apple-darwin.tar.gz"),
            Some("cccc3333".to_string())
        );
        assert_eq!(parse_checksums(text, "wizard-missing.tar.gz"), None);
    }

    #[test]
    fn parse_checksums_handles_binary_star_prefix_and_junk_lines() {
        // A blank line, a one-field junk line, then a binary-mode (`*`) entry.
        let text =
            "\n# a comment line with only one field\nDEADBEEF *wizard-x86_64-apple-darwin.tar.gz\n";
        assert_eq!(
            parse_checksums(text, "wizard-x86_64-apple-darwin.tar.gz"),
            Some("deadbeef".to_string())
        );
    }

    #[test]
    fn notice_line_only_when_strictly_newer() {
        assert_eq!(
            notice_line("v0.6.0", "0.5.0"),
            Some("wizard v0.6.0 available (you have v0.5.0) — run `wizard update`".to_string())
        );
        // Same version, older "latest", and an empty cache all stay quiet.
        assert_eq!(notice_line("v0.5.0", "0.5.0"), None);
        assert_eq!(notice_line("v0.4.0", "0.5.0"), None);
        assert_eq!(notice_line("", "0.5.0"), None);
    }

    #[test]
    fn sha256_hex_matches_known_vector() {
        // sha256("") — the empty-input digest.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
