//! Environment diagnostics: `wizard doctor` (CLI) and `/doctor` (TUI).
//!
//! Runs a battery of checks — config parses, providers reachable, MCP
//! servers handshake, tools registered, hooks parse, state directories
//! writable, checkpoint index sane — and prints one `✓` / `✗` / `–` line
//! per check. Network probes are capped at [`PROBE_TIMEOUT`] so doctor can
//! never hang. The CLI exits 0 when nothing failed, 1 otherwise; skipped
//! (`–`) checks are not failures.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;

use crate::config::{Config, ProviderConfig};
use crate::tools::registry::ToolRegistry;

/// Cap on every network probe (provider health, MCP handshake).
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

/// The native tool set is compiled in and registered.
pub fn check_native_tools() -> Check {
    let count = ToolRegistry::with_native_tools().len();
    if count == 0 {
        Check::fail("native tools", "no native tools registered")
    } else {
        Check::pass("native tools", format!("{count} tools registered"))
    }
}

// ---------------------------------------------------------------------------
// network checks (probe with timeout; never exercised by unit tests)
// ---------------------------------------------------------------------------

/// One configured provider answers its health probe within
/// [`PROBE_TIMEOUT`]. Skipped when its API key env var is not set.
async fn check_provider(provider: &ProviderConfig) -> Check {
    let label = format!("provider {}", provider.name);
    if let Some(env) = &provider.api_key_env
        && !std::env::var(env).is_ok_and(|value| !value.trim().is_empty())
    {
        return Check::skip(label, format!("${env} not set"));
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
/// within [`PROBE_TIMEOUT`].
async fn check_mcp_servers(path: &Path) -> Vec<Check> {
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
            match tokio::time::timeout(PROBE_TIMEOUT, crate::mcp::McpConnection::connect(server))
                .await
            {
                Ok(Ok(connection)) => {
                    let detail =
                        match tokio::time::timeout(PROBE_TIMEOUT, connection.list_tools()).await {
                            Ok(Ok(tools)) => format!("handshake ok, {} tool(s)", tools.len()),
                            _ => "handshake ok".to_string(),
                        };
                    Check::pass(label, detail)
                }
                Ok(Err(err)) => Check::fail(label, format!("{err:#}")),
                Err(_) => Check::fail(
                    label,
                    format!("no handshake within {}s", PROBE_TIMEOUT.as_secs()),
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
        }
        Err(err) => checks.push(Check::fail(
            "providers",
            format!("config unusable: {err:#}"),
        )),
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

/// `wizard doctor`: print the report, exit 0 when nothing failed.
pub async fn run() -> Result<i32> {
    let project_root = std::env::current_dir()?;
    let checks = run_checks(&project_root).await;
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
    async fn provider_check_skips_when_the_key_env_is_unset() {
        let provider = ProviderConfig {
            name: "cloud".to_string(),
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
        assert!(check.detail.contains("not set"));
    }
}
