//! SSH devices: other machines reached with this device's own ssh setup (keys,
//! agent, `~/.ssh/config`). Setup copies this build of the engine and Wizard
//! to the remote, can share this device's Wizard sign-in, and starts the
//! remote engine; `zeron ssh <host>` then opens a window that drives that
//! engine through an ssh tunnel to its loopback IPC port.
//!
//! Every ssh call runs with `BatchMode=yes`: a GUI has no terminal to answer
//! a password or host-key prompt, so those fail fast with a hint instead.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

// Shared with the engine, which keeps these hosts connected (ssh_peers).
pub use zeron_engine::ssh_peers::{FILE_NAME, REMOTE_IPC_PORT, valid_host};
use zeron_engine::ssh_peers::{SSH_OPTIONS, remote_start_script as start_script};

/// UI settings that name this device's chats, spaces, or window placement.
/// A window driving another machine's engine starts without them.
const DEVICE_BOUND_SETTINGS: &[&str] = &[
    "windowGeometry",
    "lastSpaceId",
    "lastProjectActionBySpaceId",
    "openTabs",
    "spaceFilter",
    "sidebarSectionsByProfile",
    "sidebarPinnedSessionIdsByProfile",
    "tabOrder",
    "spaceOrder",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SshDevice {
    pub host: String,
    pub added_at: DateTime<Utc>,
    /// Outcome of the most recent setup; `None` until one finishes.
    #[serde(default)]
    pub last_setup_ok: Option<bool>,
    /// `uname -sm` of the remote, e.g. `Linux x86_64`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_platform: Option<String>,
}

pub fn path(data_dir: &Path) -> PathBuf {
    data_dir.join(FILE_NAME)
}

/// The saved hosts; empty when the file is missing or unreadable.
pub fn load(data_dir: &Path) -> Vec<SshDevice> {
    std::fs::read_to_string(path(data_dir))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

pub fn save(data_dir: &Path, devices: &[SshDevice]) -> std::io::Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let target = path(data_dir);
    let tmp = target.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(devices)?)?;
    std::fs::rename(tmp, target)
}

/// Insert `host`, or update the existing entry's setup result in place.
pub fn record(
    devices: &mut Vec<SshDevice>,
    host: &str,
    setup_ok: Option<bool>,
    remote_platform: Option<String>,
) {
    if let Some(device) = devices.iter_mut().find(|d| d.host == host) {
        if setup_ok.is_some() {
            device.last_setup_ok = setup_ok;
        }
        if remote_platform.is_some() {
            device.remote_platform = remote_platform;
        }
    } else {
        devices.push(SshDevice {
            host: host.to_string(),
            added_at: Utc::now(),
            last_setup_ok: setup_ok,
            remote_platform,
        });
    }
}

static REMOTE_HOST: OnceLock<String> = OnceLock::new();

/// Mark this process as a window driving `host`'s engine over ssh.
pub fn set_remote_host(host: String) {
    let _ = REMOTE_HOST.set(host);
}

/// The ssh host this window drives, when it is not the local engine.
pub fn remote_host() -> Option<&'static str> {
    REMOTE_HOST.get().map(String::as_str)
}

/// A directory-name form of `host`.
pub fn sanitize_host(host: &str) -> String {
    let cleaned: String = host
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('.').to_string();
    if cleaned.is_empty() {
        "host".into()
    } else {
        cleaned
    }
}

// ---------------------------------------------------------------------------
// ~/.ssh/config
// ---------------------------------------------------------------------------

/// Host aliases and `Include` patterns declared in one ssh config file.
/// Wildcard and negated `Host` patterns are not destinations and are skipped.
pub fn parse_ssh_config(text: &str) -> (Vec<String>, Vec<String>) {
    let mut hosts = Vec::new();
    let mut includes = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (keyword, rest) = match line.find(|c: char| c.is_whitespace() || c == '=') {
            Some(ix) => (
                &line[..ix],
                line[ix..].trim_start_matches(|c: char| c.is_whitespace() || c == '='),
            ),
            None => (line, ""),
        };
        let tokens = config_tokens(rest);
        if keyword.eq_ignore_ascii_case("host") {
            for token in tokens {
                if !token.contains(['*', '?', '!']) && valid_host(&token) {
                    hosts.push(token);
                }
            }
        } else if keyword.eq_ignore_ascii_case("include") {
            includes.extend(tokens);
        }
    }
    (hosts, includes)
}

/// Whitespace-separated tokens, honoring double quotes; a trailing comment
/// ends the line.
fn config_tokens(rest: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for c in rest.chars() {
        match c {
            '"' => quoted = !quoted,
            '#' if !quoted && current.is_empty() => break,
            c if c.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// `*`/`?` glob match of one path component.
fn wildcard_match(pattern: &str, name: &str) -> bool {
    fn go(p: &[char], n: &[char]) -> bool {
        match p.split_first() {
            None => n.is_empty(),
            Some(('*', rest)) => (0..=n.len()).any(|i| go(rest, &n[i..])),
            Some(('?', rest)) => !n.is_empty() && go(rest, &n[1..]),
            Some((c, rest)) => n.first() == Some(c) && go(rest, &n[1..]),
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    go(&p, &n)
}

/// Files an `Include` pattern names: `~` expands to home, relative paths are
/// relative to `~/.ssh`, and a wildcard is allowed in the final component.
fn resolve_include(pattern: &str, ssh_dir: &Path, home: &Path) -> Vec<PathBuf> {
    let expanded = if let Some(rest) = pattern.strip_prefix("~/") {
        home.join(rest)
    } else if Path::new(pattern).is_absolute() {
        PathBuf::from(pattern)
    } else {
        ssh_dir.join(pattern)
    };
    let name = expanded
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if !name.contains(['*', '?']) {
        return vec![expanded];
    }
    let Some(dir) = expanded.parent() else {
        return Vec::new();
    };
    let mut matches: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| wildcard_match(&name, &entry.file_name().to_string_lossy()))
        .map(|entry| entry.path())
        .collect();
    matches.sort();
    matches
}

fn collect_hosts(file: &Path, ssh_dir: &Path, home: &Path, depth: usize, out: &mut Vec<String>) {
    if depth > 4 {
        return;
    }
    let Ok(text) = std::fs::read_to_string(file) else {
        return;
    };
    let (hosts, includes) = parse_ssh_config(&text);
    for host in hosts {
        if !out.contains(&host) {
            out.push(host);
        }
    }
    for pattern in includes {
        for included in resolve_include(&pattern, ssh_dir, home) {
            collect_hosts(&included, ssh_dir, home, depth + 1, out);
        }
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// Host aliases from `~/.ssh/config` (and the files it includes), in file
/// order. These are the suggestions offered when adding a device.
pub fn ssh_config_hosts() -> Vec<String> {
    let Some(home) = home_dir() else {
        return Vec::new();
    };
    let ssh_dir = home.join(".ssh");
    let mut hosts = Vec::new();
    collect_hosts(&ssh_dir.join("config"), &ssh_dir, &home, 0, &mut hosts);
    hosts
}

// ---------------------------------------------------------------------------
// Per-host UI data dir
// ---------------------------------------------------------------------------

/// Where a window driving `host` keeps its own UI settings, so its tabs and
/// window placement never overwrite the main window's.
pub fn remote_ui_dir(main_data_dir: &Path, host: &str) -> PathBuf {
    main_data_dir.join("ssh").join(sanitize_host(host))
}

/// The main window's UI settings minus everything that names this device's
/// chats or spaces, with onboarding already done.
pub fn seeded_settings(mut main: serde_json::Value) -> serde_json::Value {
    if !main.is_object() {
        main = serde_json::json!({});
    }
    let object = main.as_object_mut().expect("object");
    for key in DEVICE_BOUND_SETTINGS {
        object.remove(*key);
    }
    object.insert("onboardingCompleted".into(), serde_json::Value::Bool(true));
    main
}

/// Create the per-host UI dir on first connect, seeded from the main window's
/// preferences (theme, fonts, compact mode, …). Later connects keep whatever
/// that window saved.
pub fn seed_remote_ui_dir(main_data_dir: &Path, host: &str) -> std::io::Result<PathBuf> {
    let dir = remote_ui_dir(main_data_dir, host);
    std::fs::create_dir_all(&dir)?;
    let settings = dir.join("ui-settings.json");
    if !settings.exists() {
        let main = std::fs::read_to_string(main_data_dir.join("ui-settings.json"))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_else(|| serde_json::json!({}));
        std::fs::write(
            &settings,
            serde_json::to_vec_pretty(&seeded_settings(main))?,
        )?;
        let defaults = main_data_dir.join("composer-defaults.json");
        if defaults.exists() {
            let _ = std::fs::copy(defaults, dir.join("composer-defaults.json"));
        }
    }
    Ok(dir)
}

// ---------------------------------------------------------------------------
// Platform
// ---------------------------------------------------------------------------

/// `uname -sm` → (`std::env::consts::OS`, `ARCH`) vocabulary.
pub fn normalize_platform(uname_sm: &str) -> Option<(String, String)> {
    let mut parts = uname_sm.split_whitespace();
    let os = match parts.next()? {
        "Linux" => "linux",
        "Darwin" => "macos",
        other => return Some((other.to_lowercase(), parts.next()?.to_string())),
    };
    let arch = match parts.next()? {
        "x86_64" | "amd64" => "x86_64",
        "aarch64" | "arm64" => "aarch64",
        other => other,
    };
    Some((os.to_string(), arch.to_string()))
}

fn same_platform_as_local(uname_sm: &str) -> bool {
    normalize_platform(uname_sm)
        .is_some_and(|(os, arch)| os == std::env::consts::OS && arch == std::env::consts::ARCH)
}

fn local_platform_label() -> String {
    format!("{} {}", std::env::consts::OS, std::env::consts::ARCH)
}

fn file_sha256(path: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read as _;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// The wizard binary this device's shell would run.
fn local_wizard() -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    if let Some(home) = home_dir() {
        dirs.push(home.join(".local/bin"));
        dirs.push(home.join(".cargo/bin"));
    }
    dirs.push("/usr/local/bin".into());
    dirs.push("/opt/homebrew/bin".into());
    dirs.into_iter()
        .map(|dir| dir.join("wizard"))
        .find(|candidate| candidate.is_file())
}

// ---------------------------------------------------------------------------
// ssh plumbing
// ---------------------------------------------------------------------------

fn ssh(host: &str) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("ssh");
    cmd.args(SSH_OPTIONS)
        .arg(host)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd
}

/// Turn a failed ssh exit into the message the user sees.
fn ssh_failure(host: &str, code: Option<i32>, stderr: &[u8]) -> anyhow::Error {
    let stderr = String::from_utf8_lossy(stderr);
    let detail = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .last()
        .unwrap_or("")
        .to_string();
    if code == Some(255) {
        if stderr.contains("Permission denied") {
            return anyhow!(
                "{host} didn't accept this device's SSH keys. Add your public key there \
                 (ssh-copy-id {host}) or load it into ssh-agent, then try again."
            );
        }
        if stderr.contains("Host key verification failed") {
            return anyhow!(
                "{host}'s host key isn't trusted yet. Run `ssh {host}` once in a terminal \
                 to accept it, then try again."
            );
        }
        if stderr.contains("Could not resolve hostname") {
            return anyhow!("Couldn't find {host}. Check the name or your ~/.ssh/config.");
        }
        return anyhow!(
            "Couldn't reach {host}{}",
            if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            }
        );
    }
    if detail.is_empty() {
        anyhow!("command on {host} failed (exit {})", code.unwrap_or(-1))
    } else {
        anyhow!("{detail}")
    }
}

/// Run a POSIX `sh` script on `host` (fed on stdin, so the remote login
/// shell never parses it) and return its stdout.
async fn run_script(host: &str, script: &str) -> anyhow::Result<String> {
    let mut child = ssh(host)
        .args(["sh", "-s"])
        .stdin(Stdio::piped())
        .spawn()
        .context("couldn't run ssh — is OpenSSH installed?")?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let script = script.as_bytes().to_vec();
    let write = async move {
        stdin.write_all(&script).await?;
        stdin.shutdown().await
    };
    let (_, output) = tokio::join!(write, child.wait_with_output());
    let output = output?;
    if !output.status.success() {
        return Err(ssh_failure(host, output.status.code(), &output.stderr));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Run `command` on `host` with `input` on its stdin.
async fn run_with_input(host: &str, command: &str, input: Vec<u8>) -> anyhow::Result<String> {
    let mut child = ssh(host)
        .arg(command)
        .stdin(Stdio::piped())
        .spawn()
        .context("couldn't run ssh — is OpenSSH installed?")?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let write = async move {
        stdin.write_all(&input).await?;
        stdin.shutdown().await
    };
    let (_, output) = tokio::join!(write, child.wait_with_output());
    let output = output?;
    if !output.status.success() {
        return Err(ssh_failure(host, output.status.code(), &output.stderr));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Stream `local` to `~/<remote_dir>/<name>` on `host` (compressed), landing
/// it atomically with mode 755.
async fn upload(
    host: &str,
    local: &Path,
    remote_dir: &str,
    name: &str,
    progress: &(dyn Fn(SetupProgress) + Send + Sync),
    label: &str,
) -> anyhow::Result<()> {
    let total = std::fs::metadata(local)
        .with_context(|| format!("reading {}", local.display()))?
        .len();
    let part = format!("{remote_dir}/{name}.part");
    let command = format!(
        "sh -c 'mkdir -p {remote_dir} && cat > {part} && chmod 755 {part} && mv -f {part} {remote_dir}/{name}'"
    );
    let mut child = tokio::process::Command::new("ssh")
        .arg("-C")
        .args(SSH_OPTIONS)
        .arg(host)
        .arg(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("couldn't run ssh — is OpenSSH installed?")?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let mut file = tokio::fs::File::open(local).await?;
    let label = label.to_string();
    let write = async {
        let mut buf = vec![0u8; 1 << 20];
        let mut sent: u64 = 0;
        let mut reported = 0;
        loop {
            let n = file.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            stdin.write_all(&buf[..n]).await?;
            sent += n as u64;
            let pct = (sent * 100 / total.max(1)) as u32;
            if pct >= reported + 10 {
                reported = pct - pct % 10;
                progress(SetupProgress::Update(format!("{label} — {reported}%")));
            }
        }
        stdin.shutdown().await?;
        drop(stdin);
        Ok::<_, std::io::Error>(())
    };
    let (written, output) = tokio::join!(write, child.wait_with_output());
    let output = output?;
    if !output.status.success() {
        return Err(ssh_failure(host, output.status.code(), &output.stderr));
    }
    written.context("upload interrupted")?;
    Ok(())
}

fn key_values(output: &str) -> std::collections::HashMap<String, String> {
    output
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

const PROBE_SCRIPT: &str = r#"
printf 'platform=%s\n' "$(uname -sm 2>/dev/null)"
Z="$HOME/.zeron/app/current/zeron"
if [ -x "$Z" ]; then
  printf 'zeron_sha=%s\n' "$( (sha256sum "$Z" 2>/dev/null || shasum -a 256 "$Z" 2>/dev/null) | cut -d' ' -f1)"
fi
W=""
for d in "$HOME/.local/bin" "$HOME/.cargo/bin" /usr/local/bin /opt/homebrew/bin; do
  if [ -x "$d/wizard" ]; then W="$d/wizard"; break; fi
done
[ -n "$W" ] || W="$(command -v wizard 2>/dev/null || true)"
if [ -n "$W" ]; then
  printf 'wizard=%s\n' "$W"
  printf 'wizard_version=%s\n' "$("$W" --version 2>/dev/null | head -n 1)"
  printf 'wizard_sha=%s\n' "$( (sha256sum "$W" 2>/dev/null || shasum -a 256 "$W" 2>/dev/null) | cut -d' ' -f1)"
fi
if (bash -c '</dev/tcp/127.0.0.1/27654') >/dev/null 2>&1 || nc -z 127.0.0.1 27654 >/dev/null 2>&1; then
  echo engine=up
fi
"#;

// ---------------------------------------------------------------------------
// Setup
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupProgress {
    /// A new step began.
    Step(String),
    /// Replace the current step's text (upload percentages).
    Update(String),
}

#[derive(Debug, Clone, Copy)]
pub struct SetupOptions {
    /// Copy this device's Wizard provider sign-in to the remote.
    pub share_wizard_sign_in: bool,
}

#[derive(Debug, Clone)]
pub struct SetupOutcome {
    /// `uname -sm` of the remote.
    pub platform: String,
}

/// Prepare `host` to run Wizard for this device: probe it, install or refresh
/// the engine and Wizard, optionally share this device's Wizard sign-in, and
/// start the engine. Must run on tokio.
pub async fn setup(
    host: String,
    options: SetupOptions,
    progress: impl Fn(SetupProgress) + Send + Sync + 'static,
) -> anyhow::Result<SetupOutcome> {
    if !valid_host(&host) {
        bail!("`{host}` isn't a valid ssh host");
    }
    let progress: &(dyn Fn(SetupProgress) + Send + Sync) = &progress;
    let step = |text: String| progress(SetupProgress::Step(text));

    step(format!("Connecting to {host} with this device's SSH keys"));
    let probe = key_values(&run_script(&host, PROBE_SCRIPT).await?);
    let platform = probe
        .get("platform")
        .cloned()
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "unknown platform".into());
    progress(SetupProgress::Update(format!(
        "Connected to {host} ({platform})"
    )));
    let compatible = same_platform_as_local(&platform);

    // Engine: this exact build, so the remote has the same harness support.
    step("Checking the Wizard GUI engine".into());
    let exe = std::env::current_exe().context("locating this app's executable")?;
    let exe_for_hash = exe.clone();
    let local_sha = tokio::task::spawn_blocking(move || file_sha256(&exe_for_hash))
        .await?
        .context("hashing this app's executable")?;
    let remote_sha = probe.get("zeron_sha").cloned().unwrap_or_default();
    let mut installed_engine = false;
    if remote_sha == local_sha {
        progress(SetupProgress::Update(
            "The Wizard GUI engine is up to date".into(),
        ));
    } else if compatible {
        let dir = format!(
            ".zeron/app/{}-wizard-{}",
            env!("CARGO_PKG_VERSION"),
            &local_sha[..8]
        );
        let label = "Copying the Wizard GUI engine";
        progress(SetupProgress::Update(label.into()));
        upload(&host, &exe, &dir, "zeron", progress, label).await?;
        run_script(
            &host,
            &format!(
                "mkdir -p \"$HOME/.local/bin\" && ln -sfn \"$HOME/{dir}\" \"$HOME/.zeron/app/current\" \
                 && ln -sfn \"$HOME/.zeron/app/current/zeron\" \"$HOME/.local/bin/zeron\"\n"
            ),
        )
        .await?;
        if let Err(err) =
            run_script(&host, "\"$HOME/.local/bin/zeron\" --version >/dev/null\n").await
        {
            bail!(
                "The engine copied to {host} can't run there ({err:#}). It needs glibc plus \
                 libxcb and libxkbcommon — install those (on NixOS, enable nix-ld with them), \
                 then set it up again."
            );
        }
        progress(SetupProgress::Update(
            "Installed the Wizard GUI engine".into(),
        ));
        installed_engine = true;
    } else if !remote_sha.is_empty() {
        progress(SetupProgress::Update(format!(
            "Using the engine already on {host} (it runs {platform}; this device runs {}, \
             so this build can't be copied)",
            local_platform_label()
        )));
    } else {
        bail!(
            "{host} runs {platform}, but this device runs {}. Install the Wizard GUI \
             there with a matching build, then set it up again.",
            local_platform_label()
        );
    }

    // Wizard itself. A copy in ~/.local/bin is ours to keep in step with
    // this device's build (its ACP features, e.g. the model picker); one
    // installed elsewhere belongs to the system and is left alone.
    step("Checking Wizard".into());
    let local_wizard_build = match local_wizard().filter(|_| compatible) {
        Some(path) => {
            let for_hash = path.clone();
            tokio::task::spawn_blocking(move || file_sha256(&for_hash))
                .await
                .ok()
                .and_then(Result::ok)
                .map(|sha| (path, sha))
        }
        None => None,
    };
    let remote_wizard = probe.get("wizard").cloned().unwrap_or_default();
    let user_managed = remote_wizard.is_empty() || remote_wizard.ends_with("/.local/bin/wizard");
    let stale = local_wizard_build
        .as_ref()
        .is_some_and(|(_, sha)| probe.get("wizard_sha").is_none_or(|remote| remote != sha));
    if !remote_wizard.is_empty() && user_managed && stale {
        let (local, _) = local_wizard_build
            .as_ref()
            .expect("stale implies a local build");
        let label = "Updating Wizard to this device's build";
        progress(SetupProgress::Update(label.into()));
        upload(&host, local, ".local/bin", "wizard", progress, label).await?;
        run_script(&host, "\"$HOME/.local/bin/wizard\" --version >/dev/null\n")
            .await
            .context("the updated Wizard doesn't run there")?;
        progress(SetupProgress::Update("Updated Wizard".into()));
    } else if let Some(version) = probe.get("wizard_version").filter(|v| !v.is_empty()) {
        progress(SetupProgress::Update(format!("Found {version}")));
    } else if probe.contains_key("wizard") {
        progress(SetupProgress::Update("Found Wizard".into()));
    } else {
        // Prefer this device's own build (same features, no download); fall
        // back to the official installer when it can't be copied or run there.
        let mut copied = false;
        if let Some(local) = local_wizard().filter(|_| compatible) {
            let label = "Copying Wizard from this device";
            progress(SetupProgress::Update(label.into()));
            upload(&host, &local, ".local/bin", "wizard", progress, label).await?;
            copied = run_script(&host, "\"$HOME/.local/bin/wizard\" --version >/dev/null\n")
                .await
                .is_ok();
            if !copied {
                let _ = run_script(&host, "rm -f \"$HOME/.local/bin/wizard\"\n").await;
            }
        }
        if !copied {
            progress(SetupProgress::Update("Installing Wizard".into()));
            run_script(
                &host,
                "curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh \
                 | WIZARD_INSTALL_DIR=\"$HOME/.local/bin\" bash >/dev/null\n",
            )
            .await
            .context("installing Wizard")?;
        }
        progress(SetupProgress::Update("Installed Wizard".into()));
    }

    // Sign-in: the provider credentials Wizard uses on this device.
    if options.share_wizard_sign_in {
        step("Sharing this device's Wizard sign-in".into());
        let bundle = tokio::task::spawn_blocking(|| {
            zeron_engine::wizard_auth::bundle_from(
                zeron_engine::wizard_auth::CredentialSource::Wizard,
            )
        })
        .await?;
        match bundle {
            Ok(bundle) => {
                let payload = serde_json::to_vec(&bundle)?;
                run_with_input(&host, ".local/bin/zeron wizard-auth apply", payload)
                    .await
                    .context("sharing the Wizard sign-in")?;
                progress(SetupProgress::Update(
                    "Shared this device's Wizard sign-in".into(),
                ));
            }
            Err(err) => {
                tracing::info!(error = %err, "no Wizard sign-in to share");
                progress(SetupProgress::Update(format!(
                    "No Wizard sign-in on this device to share — sign in on {host} later"
                )));
            }
        }
    }

    step(format!("Starting the engine on {host}"));
    let started = key_values(&run_script(&host, &start_script(installed_engine)).await?);
    progress(SetupProgress::Update(
        match started.get("engine").map(String::as_str) {
            Some("systemd") => format!("Engine running on {host} as a service"),
            Some("background") => format!("Engine running on {host}"),
            _ => format!("Engine already running on {host}"),
        },
    ));
    Ok(SetupOutcome { platform })
}

// ---------------------------------------------------------------------------
// Connecting a window (`zeron ssh <host>`)
// ---------------------------------------------------------------------------

/// Blocking: make sure the remote engine is up (starting it if needed).
fn ensure_remote_engine_blocking(host: &str) -> anyhow::Result<()> {
    use std::io::Write as _;
    let mut child = std::process::Command::new("ssh")
        .args(SSH_OPTIONS)
        .arg(host)
        .args(["sh", "-s"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("couldn't run ssh — is OpenSSH installed?")?;
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(start_script(false).as_bytes())?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(ssh_failure(host, output.status.code(), &output.stderr));
    }
    Ok(())
}

fn free_local_port() -> std::io::Result<u16> {
    Ok(std::net::TcpListener::bind(("127.0.0.1", 0))?
        .local_addr()?
        .port())
}

fn spawn_tunnel(host: &str, port: u16) -> std::io::Result<std::process::Child> {
    let mut cmd = std::process::Command::new("ssh");
    cmd.arg("-N")
        .args(SSH_OPTIONS)
        .args(["-o", "ExitOnForwardFailure=yes"])
        .arg("-L")
        .arg(format!("{port}:127.0.0.1:{REMOTE_IPC_PORT}"))
        .arg(host)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    // The tunnel must not outlive the window, even if the app is killed.
    #[cfg(target_os = "linux")]
    unsafe {
        use std::os::unix::process::CommandExt as _;
        cmd.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn()
}

/// Forward a running tunnel's stderr to the log so its pipe never fills.
fn drain_stderr(child: &mut std::process::Child, host: &str) {
    use std::io::BufRead as _;
    let Some(stderr) = child.stderr.take() else {
        return;
    };
    let host = host.to_string();
    let _ = std::thread::Builder::new()
        .name("ssh-tunnel-stderr".into())
        .spawn(move || {
            for line in std::io::BufReader::new(stderr)
                .lines()
                .map_while(Result::ok)
            {
                tracing::warn!(%host, "ssh: {line}");
            }
        });
}

fn wait_for_port(port: u16, deadline: std::time::Instant) -> bool {
    while std::time::Instant::now() < deadline {
        if std::net::TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
            Duration::from_millis(300),
        )
        .is_ok()
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    false
}

/// A live ssh port forward to a remote engine. A supervisor thread re-opens
/// it on the same local port whenever ssh exits (sleep, Wi-Fi changes), so
/// the window's engine connection can recover; dropping it closes the tunnel.
pub struct SshTunnel {
    pub host: String,
    pub local_port: u16,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    child: std::sync::Arc<std::sync::Mutex<Option<std::process::Child>>>,
}

impl Drop for SshTunnel {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(mut child) = self.child.lock().ok().and_then(|mut c| c.take()) {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Ensure `host`'s engine is running and forward a free local port to it.
/// Blocking; call before the UI starts.
pub fn open_tunnel(host: &str) -> anyhow::Result<SshTunnel> {
    if !valid_host(host) {
        bail!("`{host}` isn't a valid ssh host");
    }
    ensure_remote_engine_blocking(host)?;
    let port = free_local_port()?;
    let mut first = spawn_tunnel(host, port).context("couldn't run ssh — is OpenSSH installed?")?;
    if !wait_for_port(port, std::time::Instant::now() + Duration::from_secs(15)) {
        let _ = first.kill();
        let output = first.wait_with_output()?;
        return Err(ssh_failure(host, output.status.code(), &output.stderr)
            .context(format!("couldn't open a tunnel to {host}")));
    }
    drain_stderr(&mut first, host);
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let child = std::sync::Arc::new(std::sync::Mutex::new(Some(first)));
    let (thread_stop, thread_child, thread_host) = (stop.clone(), child.clone(), host.to_string());
    std::thread::Builder::new()
        .name("ssh-tunnel".into())
        .spawn(move || {
            let mut backoff = Duration::from_secs(1);
            loop {
                std::thread::sleep(Duration::from_millis(500));
                if thread_stop.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                let exited = match thread_child.lock() {
                    Ok(mut guard) => match guard.as_mut().map(|c| c.try_wait()) {
                        Some(Ok(Some(_))) | None => {
                            *guard = None;
                            true
                        }
                        _ => false,
                    },
                    Err(_) => return,
                };
                if !exited {
                    backoff = Duration::from_secs(1);
                    continue;
                }
                tracing::warn!(host = %thread_host, "ssh tunnel closed; reopening");
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(30));
                if thread_stop.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                let _ = ensure_remote_engine_blocking(&thread_host);
                match spawn_tunnel(&thread_host, port) {
                    Ok(mut next) => {
                        drain_stderr(&mut next, &thread_host);
                        if let Ok(mut guard) = thread_child.lock() {
                            *guard = Some(next);
                        }
                    }
                    Err(err) => tracing::warn!(error = %err, "couldn't reopen the ssh tunnel"),
                }
            }
        })?;
    Ok(SshTunnel {
        host: host.to_string(),
        local_port: port,
        stop,
        child,
    })
}

/// Open a window for `host` in a new process (`<this exe> ssh <host>`), its
/// output going to `{data_dir}/logs/ssh-<host>.log`.
pub fn launch_window(data_dir: &Path, host: &str) -> anyhow::Result<()> {
    if !valid_host(host) {
        bail!("`{host}` isn't a valid ssh host");
    }
    let exe = std::env::current_exe().context("locating this app's executable")?;
    let logs = data_dir.join("logs");
    std::fs::create_dir_all(&logs)?;
    let log = std::fs::File::create(logs.join(format!("ssh-{}.log", sanitize_host(host))))?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("ssh")
        .arg(host)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // Its own session: closing this window never takes the other down.
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().context("couldn't start the window")?;
    // Reap it when the window closes, so no zombie outlives it.
    std::thread::Builder::new()
        .name(format!("ssh-window-{}", sanitize_host(host)))
        .spawn(move || {
            let _ = child.wait();
        })
        .context("couldn't watch the window process")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hosts_and_includes() {
        let text = "\
# comment
Host nixos nixos-wifi
  HostName 10.0.0.2
Host *.internal !bastion
Host=\"quoted-box\"
host   ncshare   # trailing comment
Include config.d/*  ~/.ssh/extra
Match host foo
  User bar
";
        let (hosts, includes) = parse_ssh_config(text);
        assert_eq!(hosts, ["nixos", "nixos-wifi", "quoted-box", "ncshare"]);
        assert_eq!(includes, ["config.d/*", "~/.ssh/extra"]);
    }

    #[test]
    fn collects_hosts_through_includes() {
        let home = tempfile::tempdir().unwrap();
        let ssh_dir = home.path().join(".ssh");
        std::fs::create_dir_all(ssh_dir.join("config.d")).unwrap();
        std::fs::write(ssh_dir.join("config"), "Include config.d/*\nHost main\n").unwrap();
        std::fs::write(ssh_dir.join("config.d/a"), "Host alpha\nHost main\n").unwrap();
        std::fs::write(ssh_dir.join("config.d/b"), "Host beta\n").unwrap();
        let mut hosts = Vec::new();
        collect_hosts(
            &ssh_dir.join("config"),
            &ssh_dir,
            home.path(),
            0,
            &mut hosts,
        );
        assert_eq!(hosts, ["main", "alpha", "beta"]);
    }

    #[test]
    fn wildcards() {
        assert!(wildcard_match("*", "anything"));
        assert!(wildcard_match("*.conf", "work.conf"));
        assert!(!wildcard_match("*.conf", "work.conf.bak"));
        assert!(wildcard_match("h?st", "host"));
    }

    #[test]
    fn host_validation_rejects_options_and_shell_syntax() {
        assert!(valid_host("nixos"));
        assert!(valid_host("teddy@10.0.0.2"));
        assert!(valid_host("box.example.com"));
        assert!(!valid_host(""));
        assert!(!valid_host("-oProxyCommand=evil"));
        assert!(!valid_host("host; rm -rf /"));
        assert!(!valid_host("a b"));
        assert!(!valid_host("$(id)"));
    }

    #[test]
    fn sanitizes_hosts_for_directories() {
        assert_eq!(sanitize_host("nixos"), "nixos");
        assert_eq!(sanitize_host("teddy@10.0.0.2"), "teddy_10.0.0.2");
        assert_eq!(sanitize_host("[::1]:22"), "___1__22");
        assert_eq!(sanitize_host(".."), "host");
        assert_eq!(sanitize_host("../../etc"), "_.._etc");
    }

    #[test]
    fn seeding_strips_device_bound_state() {
        let main = serde_json::json!({
            "windowGeometry": {"x": 1},
            "openTabs": ["chat-1"],
            "lastSpaceId": "space-1",
            "tabOrder": {"a": ["b"]},
            "sidebarPinnedSessionIdsByProfile": {"p": ["chat-1"]},
            "sidebarWidth": 280.0,
            "transcriptCompactMode": true,
            "onboardingCompleted": false,
        });
        let seeded = seeded_settings(main);
        let object = seeded.as_object().unwrap();
        for key in DEVICE_BOUND_SETTINGS {
            assert!(!object.contains_key(*key), "{key} survived");
        }
        assert_eq!(object["sidebarWidth"], 280.0);
        assert_eq!(object["transcriptCompactMode"], true);
        assert_eq!(object["onboardingCompleted"], true);
        assert_eq!(
            seeded_settings(serde_json::json!(null))["onboardingCompleted"],
            true
        );
    }

    #[test]
    fn seeds_once_and_keeps_later_changes() {
        let main = tempfile::tempdir().unwrap();
        std::fs::write(
            main.path().join("ui-settings.json"),
            r#"{"openTabs":["x"],"sidebarWidth":300}"#,
        )
        .unwrap();
        let dir = seed_remote_ui_dir(main.path(), "user@box").unwrap();
        assert_eq!(dir, main.path().join("ssh").join("user_box"));
        let seeded: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("ui-settings.json")).unwrap())
                .unwrap();
        assert!(seeded.get("openTabs").is_none());
        assert_eq!(seeded["sidebarWidth"], 300);
        std::fs::write(dir.join("ui-settings.json"), r#"{"sidebarWidth":200}"#).unwrap();
        seed_remote_ui_dir(main.path(), "user@box").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("ui-settings.json")).unwrap(),
            r#"{"sidebarWidth":200}"#
        );
    }

    #[test]
    fn device_list_round_trips_and_records() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path()).is_empty());
        let mut devices = Vec::new();
        record(&mut devices, "nixos", None, None);
        record(
            &mut devices,
            "nixos",
            Some(true),
            Some("Linux x86_64".into()),
        );
        record(&mut devices, "box", Some(false), None);
        save(dir.path(), &devices).unwrap();
        let loaded = load(dir.path());
        assert_eq!(loaded, devices);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].last_setup_ok, Some(true));
        assert_eq!(loaded[0].remote_platform.as_deref(), Some("Linux x86_64"));
    }

    #[test]
    fn platform_normalization() {
        assert_eq!(
            normalize_platform("Linux x86_64"),
            Some(("linux".into(), "x86_64".into()))
        );
        assert_eq!(
            normalize_platform("Darwin arm64"),
            Some(("macos".into(), "aarch64".into()))
        );
        assert_eq!(normalize_platform(""), None);
    }

    #[test]
    fn failures_explain_the_fix() {
        let err = ssh_failure(
            "box",
            Some(255),
            b"teddy@box: Permission denied (publickey).\n",
        );
        assert!(err.to_string().contains("ssh-copy-id box"));
        let err = ssh_failure("box", Some(255), b"Host key verification failed.\n");
        assert!(err.to_string().contains("ssh box"));
        let err = ssh_failure("box", Some(3), b"line one\nthe engine isn't installed\n");
        assert_eq!(err.to_string(), "the engine isn't installed");
    }
}
