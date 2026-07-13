//! Environment diagnostics: `wizard doctor` (CLI) and `/doctor` (TUI).
//!
//! Runs a battery of checks — config parses, providers reachable, MCP
//! servers handshake, tools registered, hooks parse, state directories
//! writable, checkpoint index sane — and prints one `✓` / `✗` / `–` line
//! per check. Provider probes are capped at [`PROBE_TIMEOUT`] and MCP
//! handshakes at the runtime's own [`crate::mcp::CONNECT_TIMEOUT`], so
//! doctor can never hang. The CLI exits 0 when nothing failed, 1 otherwise;
//! skipped (`–`) checks are not failures.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;

use crate::config::{Config, ProviderConfig};
use crate::tools::registry::ToolRegistry;

/// Cap on every provider health probe. MCP handshakes use
/// [`crate::mcp::CONNECT_TIMEOUT`] instead — the same budget the runtime
/// allows, so a slow-starting `npx`/`uvx` server that works in the app does
/// not fail doctor.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Outcome of one check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// `✓` — works.
    Pass,
    /// `✗` — broken; the doctor run exits 1.
    Fail,
    /// `–` — not applicable / nothing to check (missing optional file,
    /// unset API key).
    Skip,
}

/// One check result: a label, an outcome, and a short detail.
#[derive(Debug, Clone)]
pub struct Check {
    pub label: String,
    pub status: Status,
    pub detail: String,
}

impl Check {
    fn pass(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            status: Status::Pass,
            detail: detail.into(),
        }
    }

    fn fail(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            status: Status::Fail,
            detail: detail.into(),
        }
    }

    fn skip(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            status: Status::Skip,
            detail: detail.into(),
        }
    }
}

/// Render checks as aligned report lines.
pub fn render(checks: &[Check]) -> String {
    let width = checks
        .iter()
        .map(|check| check.label.chars().count())
        .max()
        .unwrap_or(0);
    checks
        .iter()
        .map(|check| {
            let mark = match check.status {
                Status::Pass => "✓",
                Status::Fail => "✗",
                Status::Skip => "–",
            };
            format!("{mark} {:<width$}  {}", check.label, check.detail)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// True when any check failed (drives the exit code).
pub fn has_failures(checks: &[Check]) -> bool {
    checks.iter().any(|check| check.status == Status::Fail)
}

// ---------------------------------------------------------------------------
// pure checks (unit-tested)
// ---------------------------------------------------------------------------

/// `config.toml` parses. A missing file is fine: defaults apply.
pub fn check_config_file(path: &Path) -> Check {
    let label = "config";
    match std::fs::read_to_string(path) {
        Ok(raw) => match toml::from_str::<Config>(&raw) {
            Ok(_) => Check::pass(label, format!("{} parses", path.display())),
            Err(err) => Check::fail(label, format!("{}: {err}", path.display())),
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Check::skip(label, format!("{} absent (defaults apply)", path.display()))
        }
        Err(err) => Check::fail(label, format!("{}: {err}", path.display())),
    }
}

/// One `hooks.toml` parses. Missing file means no hooks — fine.
pub fn check_hooks_file(label: &str, path: &Path) -> Check {
    match std::fs::read_to_string(path) {
        Ok(raw) => match crate::hooks::parse(&raw) {
            Ok(hooks) => Check::pass(
                label,
                format!("{} hook(s) in {}", hooks.len(), path.display()),
            ),
            Err(err) => Check::fail(label, format!("{}: {err}", path.display())),
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Check::skip(label, format!("{} absent (no hooks)", path.display()))
        }
        Err(err) => Check::fail(label, format!("{}: {err}", path.display())),
    }
}

/// `dir` exists (created if needed) and accepts a probe file.
pub fn check_writable(label: &str, dir: &Path) -> Check {
    if let Err(err) = std::fs::create_dir_all(dir) {
        return Check::fail(label, format!("cannot create {}: {err}", dir.display()));
    }
    let probe = dir.join(format!(".doctor-probe-{}", std::process::id()));
    match std::fs::write(&probe, b"ok") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            Check::pass(label, format!("{} writable", dir.display()))
        }
        Err(err) => Check::fail(label, format!("{} not writable: {err}", dir.display())),
    }
}

/// The checkpoint index parses, and every snapshot directory under
/// `.wizard/checkpoints/` belongs to an indexed turn (stale directories are
/// left over from interrupted rewinds/gc and are reported but harmless).
pub fn check_checkpoints(project_root: &Path) -> Check {
    let label = "checkpoints";
    let root = project_root.join(".wizard").join("checkpoints");
    let index = root.join("index.jsonl");
    let raw = match std::fs::read_to_string(&index) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Check::skip(label, "no checkpoint index yet".to_string());
        }
        Err(err) => return Check::fail(label, format!("{}: {err}", index.display())),
    };
    let mut turns = std::collections::BTreeSet::new();
    let mut records = 0usize;
    let mut corrupt = 0usize;
    for line in raw.lines().filter(|line| !line.trim().is_empty()) {
        match serde_json::from_str::<crate::checkpoint::SnapshotRecord>(line) {
            Ok(record) => {
                turns.insert(record.turn);
                records += 1;
            }
            Err(_) => corrupt += 1,
        }
    }
    if corrupt > 0 {
        return Check::fail(
            label,
            format!("{corrupt} corrupt line(s) in {}", index.display()),
        );
    }
    // Numeric subdirectories not referenced by any index record are stale.
    let stale = std::fs::read_dir(&root)
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| entry.path().is_dir())
                .filter_map(|entry| entry.file_name().to_str()?.parse::<u64>().ok())
                .filter(|turn| !turns.contains(turn))
                .count()
        })
        .unwrap_or(0);
    let mut detail = format!("{records} snapshot(s) across {} turn(s)", turns.len());
    if stale > 0 {
        detail.push_str(&format!(", {stale} stale snap dir(s)"));
    }
    Check::pass(label, detail)
}

/// `active_provider` in `config.toml` names a configured provider. An unknown
/// name (typo, removed provider) silently falls back to the first provider,
/// so the user would run against a different backend without noticing.
pub fn check_active_provider(config: &Config) -> Check {
    let label = "active provider";
    match config.active_provider_mismatch() {
        Some(name) => Check::fail(
            label,
            format!(
                "active_provider '{name}' matches no configured provider; \
                 falling back to '{}'",
                config.active().name
            ),
        ),
        None => Check::pass(label, format!("'{}'", config.active().name)),
    }
}

/// `credentials.toml` parses cleanly and is not group/world-accessible.
/// Normal reads degrade a corrupt file to "no stored keys", which silently
/// breaks every provider relying on a stored key — doctor surfaces it.
pub fn check_credentials_file(path: &Path) -> Check {
    let label = "credentials";
    if !path.exists() {
        return Check::skip(label, format!("{} absent (no stored keys)", path.display()));
    }
    let count = match crate::credentials::parse_strict(path) {
        Ok(count) => count,
        Err(err) => return Check::fail(label, format!("{err:#}")),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(path) {
            Ok(meta) => {
                let mode = meta.permissions().mode() & 0o777;
                if mode & 0o077 != 0 {
                    return Check::fail(
                        label,
                        format!(
                            "{} is mode {mode:03o}, expected 0600 (chmod 600 it)",
                            path.display()
                        ),
                    );
                }
            }
            Err(err) => return Check::fail(label, format!("{}: {err}", path.display())),
        }
    }
    Check::pass(label, format!("{count} stored key(s), permissions ok"))
}

/// The native tool set is compiled in and registered.
pub fn check_native_tools() -> Check {
    let count = ToolRegistry::with_native_tools().len();
    if count == 0 {
        Check::fail("native tools", "no native tools registered")
    } else {
        Check::pass("native tools", format!("{count} tools registered"))
    }
}

/// Messaging gateway configuration and token presence. Never prints the
/// secret. Warns when a telegram token is stored but `gateway.kind` is
/// still `none`, and when kind is telegram but no process appears to be
/// listening.
pub fn check_gateway(config: &Config) -> Vec<Check> {
    let mut checks = Vec::new();
    let kind = config.gateway.kind;
    let token_in_credentials =
        crate::credentials::get("telegram").is_some_and(|t| !t.trim().is_empty());
    let env_name = config.gateway.token_env();
    let token_in_env = std::env::var(env_name)
        .ok()
        .is_some_and(|t| !t.trim().is_empty());
    let token_present = token_in_credentials || token_in_env;

    match kind {
        crate::config::GatewayKind::None => {
            if token_in_credentials {
                checks.push(Check::fail(
                    "gateway",
                    "token stored under [keys] telegram but gateway.kind is \"none\" \
                     — set kind = \"telegram\" in config.toml (or re-run wizard --onboard)",
                ));
            } else {
                checks.push(Check::skip(
                    "gateway",
                    "kind = none (terminal only; set kind = \"telegram\" to enable)",
                ));
            }
        }
        crate::config::GatewayKind::Telegram => {
            checks.push(Check::pass("gateway", "kind = telegram"));
            if token_present {
                let source = if token_in_credentials {
                    "credentials.toml"
                } else {
                    env_name
                };
                checks.push(Check::pass(
                    "gateway token",
                    format!("present ({source}; secret not shown)"),
                ));
            } else {
                checks.push(Check::fail(
                    "gateway token",
                    format!(
                        "missing — paste during `wizard --onboard`, store under [keys] \
                         telegram in ~/.wizard/credentials.toml, or export {env_name}"
                    ),
                ));
            }
            checks.push(check_gateway_process());
        }
    }
    checks
}

/// Best-effort: is a `wizard --gateway` process running on this machine?
/// Uses `pgrep -af`; a missing `pgrep` is a skip, not a failure.
pub fn check_gateway_process() -> Check {
    let label = "gateway process";
    let output = std::process::Command::new("pgrep")
        .args(["-af", "wizard"])
        .output();
    match output {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let listening = stdout.lines().any(|line| {
                // Match the gateway flag without matching this doctor process.
                (line.contains("--gateway") || line.contains(" wizard-gateway"))
                    && !line.contains("pgrep")
            });
            if listening {
                Check::pass(label, "wizard --gateway appears to be running")
            } else {
                Check::fail(
                    label,
                    "no wizard --gateway process found — messages will get no reply \
                     until you run `cd <project> && wizard --gateway` (or enable the \
                     systemd user unit; see docs/gateway.md)",
                )
            }
        }
        Ok(_) => {
            // pgrep exits 1 when nothing matches.
            Check::fail(
                label,
                "no wizard --gateway process found — messages will get no reply \
                 until you run `cd <project> && wizard --gateway` (or enable the \
                 systemd user unit; see docs/gateway.md)",
            )
        }
        Err(_) => Check::skip(
            label,
            "pgrep not available; cannot check for a running gateway",
        ),
    }
}

// ---------------------------------------------------------------------------
// network checks (probe with timeout; never exercised by unit tests)
// ---------------------------------------------------------------------------

/// One configured provider answers its health probe within
/// [`PROBE_TIMEOUT`]. Skipped only when it has no key at all: neither the
/// API key env var nor a stored credential (`resolved_key` checks
/// `credentials.toml` first, so a stored key means the probe is real).
async fn check_provider(provider: &ProviderConfig) -> Check {
    let label = format!("provider {}", provider.name);
    if let Some(env) = &provider.api_key_env
        && !std::env::var(env).is_ok_and(|value| !value.trim().is_empty())
        && crate::credentials::get(&provider.name).is_none()
    {
        return Check::skip(label, format!("${env} not set and no stored key"));
    }
    let client = match provider.build() {
        Ok(client) => client,
        Err(err) => return Check::fail(label, format!("build failed: {err:#}")),
    };
    match tokio::time::timeout(PROBE_TIMEOUT, client.health()).await {
        Ok(Ok(())) => Check::pass(
            label,
            format!("{} ({}) reachable", client.label(), provider.model),
        ),
        Ok(Err(err)) => Check::fail(label, format!("{err:#}")),
        Err(_) => Check::fail(
            label,
            format!("no answer within {}s", PROBE_TIMEOUT.as_secs()),
        ),
    }
}

/// Every `[[server]]` in `mcp.toml` spawns and completes the MCP handshake
/// within the runtime's [`crate::mcp::CONNECT_TIMEOUT`], so a server that
/// works in the app never fails doctor on startup time alone.
async fn check_mcp_servers(path: &Path) -> Vec<Check> {
    let connect_timeout = crate::mcp::CONNECT_TIMEOUT;
    let config = match crate::mcp::McpConfig::load(path) {
        Ok(config) => config,
        Err(err) => return vec![Check::fail("mcp", format!("{err:#}"))],
    };
    if config.servers.is_empty() {
        return vec![Check::skip("mcp", "no MCP servers configured")];
    }
    let mut checks = Vec::new();
    for server in config.servers {
        let label = format!("mcp {}", server.name);
        let check =
            match tokio::time::timeout(connect_timeout, crate::mcp::McpConnection::connect(server))
                .await
            {
                Ok(Ok(connection)) => {
                    let detail = match tokio::time::timeout(
                        connect_timeout,
                        connection.list_tools(),
                    )
                    .await
                    {
                        Ok(Ok(tools)) => format!("handshake ok, {} tool(s)", tools.len()),
                        _ => "handshake ok".to_string(),
                    };
                    Check::pass(label, detail)
                }
                Ok(Err(err)) => Check::fail(label, format!("{err:#}")),
                Err(_) => Check::fail(
                    label,
                    format!("no handshake within {}s", connect_timeout.as_secs()),
                ),
            };
        checks.push(check);
    }
    checks
}

// ---------------------------------------------------------------------------
// assembly
// ---------------------------------------------------------------------------

/// Run the full battery for `project_root`.
pub async fn run_checks(project_root: &Path) -> Vec<Check> {
    let mut checks = Vec::new();

    // Config first: later checks reuse it when it loads.
    let config_path = Config::path().unwrap_or_else(|_| PathBuf::from("~/.wizard/config.toml"));
    checks.push(check_config_file(&config_path));

    match Config::load() {
        Ok(config) => {
            checks.push(check_active_provider(&config));
            // The synthesized local default counts when nothing is
            // configured explicitly.
            let providers = if config.providers.is_empty() {
                vec![config.active()]
            } else {
                config.providers.clone()
            };
            for provider in &providers {
                checks.push(check_provider(provider).await);
            }
            checks.extend(check_gateway(&config));
        }
        Err(err) => checks.push(Check::fail(
            "providers",
            format!("config unusable: {err:#}"),
        )),
    }

    if let Ok(path) = crate::credentials::path() {
        checks.push(check_credentials_file(&path));
    }

    if let Ok(path) = Config::mcp_config_path() {
        checks.extend(check_mcp_servers(&path).await);
    }

    checks.push(check_native_tools());

    if let Ok(dir) = Config::wizard_dir() {
        checks.push(check_hooks_file("hooks (global)", &dir.join("hooks.toml")));
        checks.push(check_writable("~/.wizard", &dir));
    }
    checks.push(check_hooks_file(
        "hooks (project)",
        &project_root.join(".wizard").join("hooks.toml"),
    ));
    checks.push(check_writable(
        "project .wizard",
        &project_root.join(".wizard"),
    ));
    if let Ok(dir) = Config::sessions_dir() {
        checks.push(check_writable("sessions", &dir));
    }
    checks.push(check_checkpoints(project_root));

    checks
}

/// `wizard doctor`: print the report, exit 0 when nothing failed. A spinner
/// covers the network probes (capped at [`PROBE_TIMEOUT`] each) while they
/// run, then clears before the report so the rendered output is unchanged;
/// it is silent when stderr is not a terminal. The TUI `/doctor` calls
/// [`run_checks`] directly — it owns the screen and draws no spinner here.
pub async fn run() -> Result<i32> {
    let project_root = std::env::current_dir()?;
    let spinner = crate::progress::Spinner::start("running checks…");
    let checks = run_checks(&project_root).await;
    spinner.finish();
    println!("{}", render(&checks));
    Ok(if has_failures(&checks) { 1 } else { 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_check_passes_skips_and_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");

        let check = check_config_file(&path);
        assert_eq!(check.status, Status::Skip);

        std::fs::write(&path, "mode = \"sovereign\"\n").unwrap();
        let check = check_config_file(&path);
        assert_eq!(check.status, Status::Pass, "{}", check.detail);

        std::fs::write(&path, "mode = [broken\n").unwrap();
        let check = check_config_file(&path);
        assert_eq!(check.status, Status::Fail);
    }

    #[test]
    fn hooks_check_passes_skips_and_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("hooks.toml");

        assert_eq!(check_hooks_file("hooks", &path).status, Status::Skip);

        std::fs::write(
            &path,
            "[[hooks]]\nevent = \"pre_tool_use\"\ncommand = \"true\"\n",
        )
        .unwrap();
        let check = check_hooks_file("hooks", &path);
        assert_eq!(check.status, Status::Pass);
        assert!(check.detail.contains("1 hook(s)"), "{}", check.detail);

        std::fs::write(&path, "[[hooks]]\nevent = \"no_such_event\"\n").unwrap();
        assert_eq!(check_hooks_file("hooks", &path).status, Status::Fail);
    }

    #[test]
    fn writable_check_creates_and_probes() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("fresh").join("nested");
        let check = check_writable("dir", &dir);
        assert_eq!(check.status, Status::Pass, "{}", check.detail);
        assert!(dir.is_dir(), "directory was created");
        // The probe file is cleaned up.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    }

    #[test]
    fn checkpoints_check_reports_records_and_stale_dirs() {
        let tmp = tempfile::tempdir().unwrap();

        // No index yet: skip.
        assert_eq!(check_checkpoints(tmp.path()).status, Status::Skip);

        let root = tmp.path().join(".wizard").join("checkpoints");
        std::fs::create_dir_all(root.join("3")).unwrap();
        std::fs::create_dir_all(root.join("9")).unwrap(); // stale: not indexed
        let record = serde_json::json!({
            "turn": 3,
            "tool": "write_file",
            "path": "/tmp/x",
            "snap": "3/0.snap",
            "existed_before": true,
        });
        std::fs::write(root.join("index.jsonl"), format!("{record}\n")).unwrap();

        let check = check_checkpoints(tmp.path());
        assert_eq!(check.status, Status::Pass, "{}", check.detail);
        assert!(check.detail.contains("1 snapshot(s)"), "{}", check.detail);
        assert!(check.detail.contains("1 stale"), "{}", check.detail);

        // Corrupt index lines fail the check.
        std::fs::write(root.join("index.jsonl"), "not json\n").unwrap();
        assert_eq!(check_checkpoints(tmp.path()).status, Status::Fail);
    }

    #[test]
    fn native_tools_check_counts_the_registry() {
        let check = check_native_tools();
        assert_eq!(check.status, Status::Pass);
        let count = ToolRegistry::with_native_tools().len();
        assert!(check.detail.contains(&count.to_string()));
    }

    #[test]
    fn render_marks_and_aligns() {
        let checks = vec![
            Check::pass("ok", "fine"),
            Check::fail("broken-thing", "nope"),
            Check::skip("na", "nothing to do"),
        ];
        let report = render(&checks);
        let lines: Vec<&str> = report.lines().collect();
        assert!(lines[0].starts_with("✓ ok"));
        assert!(lines[1].starts_with("✗ broken-thing"));
        assert!(lines[2].starts_with("– na"));
        assert!(has_failures(&checks));
        assert!(!has_failures(&[Check::pass("a", ""), Check::skip("b", "")]));
    }

    #[tokio::test]
    async fn provider_check_skips_when_env_and_stored_key_are_both_absent() {
        // The provider name must also miss the (real) credentials store for
        // the probe to be skipped; a nonsense name guarantees that.
        let provider = ProviderConfig {
            name: "wizard-doctor-test-provider-never-stored".to_string(),
            kind: crate::config::ProviderKind::Openai,
            base_url: "https://example.invalid/v1".to_string(),
            model: "gpt-test".to_string(),
            api_key_env: Some("WIZARD_DOCTOR_TEST_KEY_THAT_IS_NEVER_SET".to_string()),
            gguf_path: None,
            usd_per_mtok_in: None,
            usd_per_mtok_out: None,
        };
        let check = check_provider(&provider).await;
        assert_eq!(check.status, Status::Skip);
        assert!(check.detail.contains("not set"), "{}", check.detail);
        assert!(check.detail.contains("no stored key"), "{}", check.detail);
    }

    #[test]
    fn active_provider_check_flags_unknown_selection() {
        let provider = ProviderConfig {
            name: "local".to_string(),
            kind: crate::config::ProviderKind::LlamaCpp,
            base_url: "http://127.0.0.1:11435".to_string(),
            model: "qwen3.6:27b".to_string(),
            api_key_env: None,
            gguf_path: None,
            usd_per_mtok_in: None,
            usd_per_mtok_out: None,
        };

        let config = Config {
            providers: vec![provider.clone()],
            active_provider: Some("local".to_string()),
            ..Config::default()
        };
        let check = check_active_provider(&config);
        assert_eq!(check.status, Status::Pass, "{}", check.detail);

        let config = Config {
            providers: vec![provider],
            active_provider: Some("claud".to_string()),
            ..Config::default()
        };
        let check = check_active_provider(&config);
        assert_eq!(check.status, Status::Fail);
        assert!(check.detail.contains("'claud'"), "{}", check.detail);
        assert!(
            check.detail.contains("falling back to 'local'"),
            "{}",
            check.detail
        );
    }

    #[test]
    fn credentials_check_skips_passes_and_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("credentials.toml");

        // Absent: nothing stored, nothing to check.
        assert_eq!(check_credentials_file(&path).status, Status::Skip);

        // Valid store with tight permissions: pass.
        std::fs::write(&path, "[keys]\nopenai = \"sk-test\"\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let check = check_credentials_file(&path);
        assert_eq!(check.status, Status::Pass, "{}", check.detail);
        assert!(check.detail.contains("1 stored key(s)"), "{}", check.detail);

        // Group/world-readable: fail (the file holds plaintext secrets).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            let check = check_credentials_file(&path);
            assert_eq!(check.status, Status::Fail);
            assert!(check.detail.contains("644"), "{}", check.detail);
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        // Corrupt TOML: fail loudly instead of degrading to "no stored keys".
        std::fs::write(&path, "this is not valid toml = = =").unwrap();
        assert_eq!(check_credentials_file(&path).status, Status::Fail);
    }

    #[test]
    fn gateway_check_skips_when_kind_is_none_and_no_token() {
        let config = Config {
            gateway: crate::config::GatewayConfig {
                kind: crate::config::GatewayKind::None,
                ..Default::default()
            },
            ..Config::default()
        };
        // Without a stored telegram token this is a skip. We cannot force
        // credentials::get to miss if the real home has a token, so only
        // assert the none/no-token path when get returns None.
        if crate::credentials::get("telegram").is_none() {
            let checks = check_gateway(&config);
            assert_eq!(checks.len(), 1);
            assert_eq!(checks[0].status, Status::Skip, "{}", checks[0].detail);
            assert!(checks[0].detail.contains("none"), "{}", checks[0].detail);
        }
    }

    #[test]
    fn gateway_check_telegram_reports_token_status_without_leaking_secret() {
        let config = Config {
            gateway: crate::config::GatewayConfig {
                kind: crate::config::GatewayKind::Telegram,
                token_env: Some("WIZARD_DOCTOR_TEST_TG_TOKEN_NEVER_SET".to_string()),
                allowed_chat_ids: vec![1],
            },
            ..Config::default()
        };
        let checks = check_gateway(&config);
        assert!(
            checks
                .iter()
                .any(|c| c.label == "gateway" && c.status == Status::Pass),
            "{checks:?}"
        );
        // Token check: either pass (if real credentials have a token) or fail.
        let token = checks
            .iter()
            .find(|c| c.label == "gateway token")
            .expect("token check present");
        assert!(
            !token.detail.contains(":")
                || token.detail.contains("credentials.toml")
                || token.detail.contains("missing")
                || token.detail.contains("WIZARD_DOCTOR"),
            "must not leak a raw token: {}",
            token.detail
        );
        // Process check is always present for telegram.
        assert!(
            checks.iter().any(|c| c.label == "gateway process"),
            "{checks:?}"
        );
    }
}
