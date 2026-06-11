//! Lifecycle management for a local llama.cpp `llama-server`.
//!
//! When the active provider is `llamacpp` and nothing answers at its
//! `base_url`, Wizard can start `llama-server` itself: detached in its own
//! process group (it keeps serving after Wizard exits), logging to
//! `~/.wizard/llama-server.log`, with the PID recorded in
//! `~/.wizard/llama-server.pid` so `/server stop` kills exactly the process
//! Wizard started and never an unrelated one.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::config::{Config, ProviderConfig};

/// Context window passed to a spawned server (`--ctx-size`). Sized so the
/// agent's compaction threshold (48 kB of history ≈ 12k tokens) plus the
/// system prompt and tool specs fit comfortably.
const CTX_SIZE: u32 = 16_384;

/// How long to wait for a spawned server to finish loading its GGUF.
const READY_TIMEOUT: Duration = Duration::from_secs(60);

/// Poll cadence while waiting for readiness.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// How long to wait for a TCP connection on a single health probe.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// User-visible progress callback for the slow paths (spawn, model load).
pub type Progress<'a> = &'a (dyn Fn(&str) + Send + Sync);

/// What `GET {base_url}/health` says about the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// 200 — model loaded, ready for requests.
    Ready,
    /// 503 — process up, GGUF still loading.
    Loading,
    /// Nothing answering (or an unexpected status).
    Down,
}

/// Probe llama-server's native health endpoint once.
pub async fn probe(base_url: &str) -> Health {
    let Ok(http) = reqwest::Client::builder()
        .connect_timeout(PROBE_TIMEOUT)
        .build()
    else {
        return Health::Down;
    };
    let url = format!("{}/health", base_url.trim_end_matches('/'));
    match http.get(url).timeout(PROBE_TIMEOUT).send().await {
        Ok(response) if response.status().is_success() => Health::Ready,
        Ok(response) if response.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE => {
            Health::Loading
        }
        _ => Health::Down,
    }
}

/// Make sure a ready llama-server answers at `provider.base_url`.
///
/// Already ready: returns immediately. Still loading: waits for readiness.
/// Down: spawns one when possible — the URL points at this machine,
/// `llama-server` is on `PATH`, and the provider has a usable `gguf_path` —
/// and waits for it; anything less is an actionable error telling the user
/// how to start the server themselves.
pub async fn ensure_running(provider: &ProviderConfig, progress: Progress<'_>) -> Result<()> {
    let base_url = provider.base_url.trim_end_matches('/');
    match probe(base_url).await {
        Health::Ready => return Ok(()),
        Health::Loading => {
            progress(&format!(
                "llama-server at {base_url} is loading its model — waiting…"
            ));
            return wait_ready(base_url, None, progress).await;
        }
        Health::Down => {}
    }

    let Some(port) = local_port(base_url) else {
        bail!(
            "cannot reach llama-server at {base_url} — the host is not this machine, so Wizard \
             cannot start it for you; start it there with {START_HINT}, or fix the provider's \
             `base_url` in ~/.wizard/config.toml"
        );
    };
    let Some(gguf) = provider
        .gguf_path
        .as_deref()
        .filter(|p| !p.trim().is_empty())
    else {
        bail!(
            "cannot reach llama-server at {base_url} and the provider has no `gguf_path`, so \
             Wizard cannot start it for you — start it with {START_HINT}, or set `gguf_path` \
             in ~/.wizard/config.toml"
        );
    };
    // A missing GGUF that names a known tier is downloaded into place (the
    // one-click local onboarding writes exactly such a path); anything else
    // stays an actionable error.
    if !Path::new(gguf).exists() {
        let tier = Path::new(gguf)
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(crate::hardware::gguf_tier_for_file);
        match tier {
            Some(tier) => crate::local_setup::download_gguf(tier, Path::new(gguf), progress)
                .await
                .context("downloading the local model")?,
            None => bail!(
                "cannot start llama-server: the model file {gguf} does not exist — fix \
                 `gguf_path` in ~/.wizard/config.toml"
            ),
        }
    }
    let binary = match find_binary() {
        Some(binary) => binary,
        // Not installed anywhere Wizard looks: install it from the official
        // llama.cpp releases, the same way `install.sh` does.
        None => crate::local_setup::install_llama_server(progress)
            .await
            .with_context(|| {
                format!(
                    "cannot reach llama-server at {base_url} and installing llama.cpp failed — \
                     install it yourself (https://github.com/ggml-org/llama.cpp), then start \
                     the server with {START_HINT}"
                )
            })?,
    };

    let pid = spawn(&binary, gguf, port)?;
    progress(&format!(
        "started llama-server (PID {pid}, port {port}) — log: {}",
        log_path()?.display()
    ));
    progress(&format!(
        "waiting for the model to load (up to {}s)…",
        READY_TIMEOUT.as_secs()
    ));
    wait_ready(base_url, Some(pid), progress).await
}

/// Poll `{base_url}/health` until the server reports ready, up to
/// [`READY_TIMEOUT`] (GGUF loads are slow). When `pid` names a server Wizard
/// just spawned, its early death short-circuits the wait with a pointer to
/// the log instead of timing out.
pub async fn wait_ready(base_url: &str, pid: Option<u32>, progress: Progress<'_>) -> Result<()> {
    let started = Instant::now();
    let mut reported_loading = false;
    while started.elapsed() < READY_TIMEOUT {
        match probe(base_url).await {
            Health::Ready => return Ok(()),
            Health::Loading if !reported_loading => {
                reported_loading = true;
                progress("llama-server is up — loading the model…");
            }
            _ => {}
        }
        if let Some(pid) = pid
            && !process_name(pid).is_some_and(|name| is_llama_server(&name))
        {
            bail!(
                "llama-server (PID {pid}) exited during startup — check {}",
                log_path()?.display()
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    bail!(
        "llama-server at {base_url} did not become ready within {}s — check {}",
        READY_TIMEOUT.as_secs(),
        log_path()?.display()
    )
}

/// Start a detached `llama-server` serving `gguf_path` on `port`. The child
/// gets its own process group and appends stdout/stderr to [`log_path`], so
/// it keeps serving after Wizard exits. The PID is recorded in [`pid_path`]
/// for `/server stop` and returned.
pub fn spawn(binary: &Path, gguf_path: &str, port: u16) -> Result<u32> {
    let log_path = log_path()?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("opening {}", log_path.display()))?;
    let mut command = std::process::Command::new(binary);
    command
        .args(["-m", gguf_path, "--port", &port.to_string()])
        // --jinja enables the chat-template engine llama-server needs for
        // OpenAI-style tool calling; without it /v1/chat/completions
        // rejects requests that carry tools.
        .args(["--ctx-size", &CTX_SIZE.to_string(), "--jinja"])
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone().context("duplicating log handle")?)
        .stderr(log);
    // Own process group: a Ctrl-C in Wizard's terminal signals the
    // foreground group and must not take the server down with it.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("spawning {}", binary.display()))?;
    let pid = child.id();
    // Reap the child if it exits while Wizard is still running, so it never
    // lingers as a zombie. The thread dies with Wizard; the server doesn't.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    write_pid(&pid_path()?, pid)?;
    Ok(pid)
}

/// Outcome of [`stop`].
#[derive(Debug, PartialEq, Eq)]
pub enum StopOutcome {
    /// SIGTERM sent to the recorded PID.
    Stopped(u32),
    /// No PID on record — Wizard never started a server.
    NotRecorded,
    /// The recorded PID is gone (the server already exited).
    NotRunning(u32),
    /// The recorded PID now belongs to some other program; refused to kill.
    NotOurs { pid: u32, name: String },
}

/// Stop the llama-server recorded in [`pid_path`]. The PID is verified to
/// still be a running `llama-server` before any signal is sent — a recycled
/// PID must never kill an unrelated process.
pub fn stop() -> Result<StopOutcome> {
    stop_at(&pid_path()?)
}

/// Testable core of [`stop`]: operates on an explicit PID-file path.
fn stop_at(pid_file: &Path) -> Result<StopOutcome> {
    let Some(pid) = read_pid(pid_file) else {
        return Ok(StopOutcome::NotRecorded);
    };
    match process_name(pid) {
        None => {
            let _ = std::fs::remove_file(pid_file);
            Ok(StopOutcome::NotRunning(pid))
        }
        Some(name) if !is_llama_server(&name) => Ok(StopOutcome::NotOurs { pid, name }),
        Some(_) => {
            let status = std::process::Command::new("kill")
                .arg(pid.to_string())
                .status()
                .context("running kill")?;
            if !status.success() {
                bail!("kill {pid} exited with {status}");
            }
            let _ = std::fs::remove_file(pid_file);
            Ok(StopOutcome::Stopped(pid))
        }
    }
}

/// PID recorded by [`spawn`], when that process is still a running
/// `llama-server`.
pub fn spawned_pid() -> Option<u32> {
    let pid = read_pid(&pid_path().ok()?)?;
    process_name(pid)
        .is_some_and(|name| is_llama_server(&name))
        .then_some(pid)
}

/// The executable Wizard looks for and verifies PIDs against.
const BINARY_NAME: &str = "llama-server";

/// The "start it yourself" command quoted by every unspawnable-server error.
const START_HINT: &str = "`llama-server -m <model.gguf> --port 8080`";

/// Find `llama-server`: on `PATH`, then in the locations Wizard's own
/// installer uses (`~/.wizard/bin`, `~/.wizard/llama.cpp`) — those are not
/// usually on `PATH`.
pub fn find_binary() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("PATH")
        && let Some(found) = std::env::split_paths(&path)
            .map(|dir| dir.join(BINARY_NAME))
            .find(|candidate| is_executable(candidate))
    {
        return Some(found);
    }
    let wizard = Config::wizard_dir().ok()?;
    [wizard.join("bin"), wizard.join("llama.cpp")]
        .into_iter()
        .map(|dir| dir.join(BINARY_NAME))
        .find(|candidate| is_executable(candidate))
}

/// True when `name` resolves to an executable on `PATH`.
pub fn on_path(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .any(|candidate| is_executable(&candidate))
    })
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Port to pass as `--port` when spawning a server for `base_url`. `None`
/// when the URL does not point at this machine — Wizard never spawns a
/// server on behalf of a remote host.
pub fn local_port(base_url: &str) -> Option<u16> {
    let url = reqwest::Url::parse(base_url).ok()?;
    let local = matches!(
        url.host_str(),
        Some("127.0.0.1" | "localhost" | "[::1]" | "::1" | "0.0.0.0")
    );
    local.then(|| url.port_or_known_default())?
}

/// `~/.wizard/llama-server.log` — stdout/stderr of servers Wizard spawned.
pub fn log_path() -> Result<PathBuf> {
    Ok(Config::wizard_dir()?.join("llama-server.log"))
}

/// `~/.wizard/llama-server.pid` — PID of the server Wizard spawned.
pub fn pid_path() -> Result<PathBuf> {
    Ok(Config::wizard_dir()?.join("llama-server.pid"))
}

fn write_pid(path: &Path, pid: u32) -> Result<()> {
    std::fs::write(path, format!("{pid}\n")).with_context(|| format!("writing {}", path.display()))
}

fn read_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Whether a process name belongs to llama-server. Distro wrappers rename
/// the real executable (NixOS execs `.llama-server-wrapped`, which `comm`
/// truncates to 15 bytes), so this matches on substring rather than
/// equality — a recycled PID with an unrelated name still never matches.
fn is_llama_server(name: &str) -> bool {
    name.contains(BINARY_NAME)
}

/// Name of the running process `pid`, or `None` when no such process.
fn process_name(pid: u32) -> Option<String> {
    // /proc is authoritative on Linux; `ps` covers macOS and the BSDs.
    if let Ok(comm) = std::fs::read_to_string(format!("/proc/{pid}/comm")) {
        return Some(comm.trim().to_string());
    }
    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if name.is_empty() {
        return None;
    }
    // Some platforms print the full path; PID checks compare the file name.
    Some(name.rsplit('/').next().unwrap_or(&name).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_port_accepts_loopback_hosts_only() {
        assert_eq!(local_port("http://127.0.0.1:8080"), Some(8080));
        assert_eq!(local_port("http://localhost:9000/"), Some(9000));
        assert_eq!(local_port("http://[::1]:8081"), Some(8081));
        assert_eq!(local_port("http://localhost"), Some(80), "known default");
        assert_eq!(local_port("http://10.0.0.5:8080"), None, "remote host");
        assert_eq!(local_port("http://example.com:8080"), None);
        assert_eq!(local_port("not a url"), None);
    }

    #[test]
    fn pid_file_round_trips_and_rejects_garbage() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("llama-server.pid");

        assert_eq!(read_pid(&path), None, "missing file");
        write_pid(&path, 4242).expect("write pid");
        assert_eq!(read_pid(&path), Some(4242));

        std::fs::write(&path, "not a pid\n").expect("write garbage");
        assert_eq!(read_pid(&path), None, "garbage is not a pid");
    }

    #[test]
    fn is_llama_server_tolerates_wrapper_names() {
        assert!(is_llama_server("llama-server"));
        // NixOS wraps the binary as `.llama-server-wrapped`; /proc comm
        // additionally truncates names to 15 bytes.
        assert!(is_llama_server(".llama-server-wrapped"));
        assert!(is_llama_server(".llama-server-w"));
        assert!(!is_llama_server("wizard"));
        assert!(!is_llama_server("llama-cli"));
    }

    #[test]
    fn process_name_resolves_live_and_dead_pids() {
        let me = process_name(std::process::id()).expect("own process exists");
        assert!(!me.is_empty());
        // PIDs are capped well below this on every supported platform.
        assert_eq!(process_name(u32::MAX - 1), None);
    }

    #[test]
    fn stop_refuses_to_kill_a_process_that_is_not_llama_server() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("llama-server.pid");
        // Record this test process: alive, but definitely not llama-server.
        write_pid(&path, std::process::id()).expect("write pid");

        match stop_at(&path).expect("stop runs") {
            StopOutcome::NotOurs { pid, name } => {
                assert_eq!(pid, std::process::id());
                assert_ne!(name, BINARY_NAME);
            }
            other => panic!("expected NotOurs, got {other:?}"),
        }
        assert!(path.exists(), "a refused stop keeps the record");
    }

    #[test]
    fn stop_clears_a_stale_pid_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("llama-server.pid");
        write_pid(&path, u32::MAX - 1).expect("write pid");

        assert_eq!(
            stop_at(&path).expect("stop runs"),
            StopOutcome::NotRunning(u32::MAX - 1)
        );
        assert!(!path.exists(), "stale record is removed");
    }

    #[test]
    fn stop_without_a_record_is_a_noop() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert_eq!(
            stop_at(&dir.path().join("llama-server.pid")).expect("stop runs"),
            StopOutcome::NotRecorded
        );
    }

    #[tokio::test]
    async fn probe_reports_down_for_an_unreachable_server() {
        // Port 1 on localhost: connection refused immediately.
        assert_eq!(probe("http://127.0.0.1:1").await, Health::Down);
    }
}
