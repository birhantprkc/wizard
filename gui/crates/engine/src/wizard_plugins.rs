//! Pi packages installed for Wizard on this device (beta), through the
//! `wizard plugins` CLI: `list`, `inspect`, `install --for wizard` and
//! `remove --for wizard`, each with `--json`. Settings → Plugins drives these
//! over relay-forwardable RPCs, so a remote or SSH device installs into its
//! own `~/.wizard` with its own Wizard.
//!
//! Wizard owns the mapping (skills to `~/.wizard/skills`, prompts to
//! commands, extensions reported as not supported yet) and the record of what
//! it wrote. This module only runs the CLI with a validated argument, never
//! through a shell, and reads its JSON.

use std::time::Duration;

use serde::{Deserialize, Serialize};

const LIST_TIMEOUT: Duration = Duration::from_secs(30);
const FETCH_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// One resource of a package, as `wizard plugins` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WizardPluginItem {
    /// `skill`, `prompt`, `extension` or `theme`.
    pub kind: String,
    pub name: String,
    /// `ready`, `installed`, `unsupported` or `skipped`.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loaded: Option<bool>,
}

impl WizardPluginItem {
    /// Wizard uses it (or will, once installed).
    pub fn works(&self) -> bool {
        matches!(self.status.as_str(), "ready" | "installed")
    }
}

/// A Pi package as Wizard installed it, or would.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WizardPlugin {
    pub name: String,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub items: Vec<WizardPluginItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

impl WizardPlugin {
    /// `("2 skills, 1 prompt", "1 extension")`: what works in Wizard and
    /// what doesn't, counted by kind. Either may be empty.
    pub fn support(&self) -> (String, String) {
        let count = |works: bool| {
            ["skill", "prompt", "extension", "theme"]
                .iter()
                .filter_map(|kind| {
                    let n = self
                        .items
                        .iter()
                        .filter(|item| item.kind == *kind && item.works() == works)
                        .count();
                    (n > 0).then(|| format!("{n} {}", plural(kind, n)))
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        (count(true), count(false))
    }
}

fn plural(kind: &str, n: usize) -> String {
    if n == 1 {
        kind.to_string()
    } else {
        format!("{kind}s")
    }
}

/// Everything the Plugins page needs from Wizard on this device.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WizardPlugins {
    /// A `wizard` CLI is installed here.
    pub wizard_installed: bool,
    /// It has `wizard plugins` (older Wizards don't).
    pub supported: bool,
    #[serde(default)]
    pub plugins: Vec<WizardPlugin>,
    /// `wizard plugins list` ran but failed; `plugins` is empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl WizardPlugins {
    /// Wizard can take a Pi package on this device.
    pub fn available(&self) -> bool {
        self.wizard_installed && self.supported
    }
}

/// The reply to an install: what was installed plus the refreshed list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WizardPluginChange {
    pub plugin: WizardPlugin,
    pub list: WizardPlugins,
}

fn wizard_cli() -> Option<std::path::PathBuf> {
    zeron_harness::AcpHarness::wizard().cli_path()
}

/// What `wizard plugins list --json` says, or why it can't be asked.
pub async fn list() -> WizardPlugins {
    let Some(program) = wizard_cli() else {
        return WizardPlugins::default();
    };
    let mut list = WizardPlugins {
        wizard_installed: true,
        ..WizardPlugins::default()
    };
    match run(&program, &["plugins", "list", "--json"], LIST_TIMEOUT).await {
        Ok(value) => {
            list.supported = true;
            match serde_json::from_value::<Vec<WizardPlugin>>(value["plugins"].clone()) {
                Ok(plugins) => list.plugins = plugins,
                Err(err) => list.error = Some(format!("Couldn't read Wizard's plugin list: {err}")),
            }
        }
        Err(Failure::Unsupported) => {}
        Err(Failure::Message(message)) => {
            list.supported = true;
            list.error = Some(message);
        }
    }
    list
}

/// What Wizard can use from a package, without installing it.
pub async fn inspect(source: &str) -> Result<WizardPlugin, String> {
    let source = crate::pi_packages::normalize_source(source)?;
    let program = wizard_cli().ok_or("Wizard isn't installed on this device")?;
    let value = run(
        &program,
        &["plugins", "inspect", &source, "--json"],
        FETCH_TIMEOUT,
    )
    .await
    .map_err(Failure::into_message)?;
    serde_json::from_value(value["plugin"].clone()).map_err(|e| e.to_string())
}

/// `wizard plugins install <source> --for wizard --json`.
pub async fn install(source: &str) -> Result<WizardPluginChange, String> {
    let source = crate::pi_packages::normalize_source(source)?;
    let program = wizard_cli().ok_or("Wizard isn't installed on this device")?;
    let value = run(
        &program,
        &["plugins", "install", &source, "--for", "wizard", "--json"],
        FETCH_TIMEOUT,
    )
    .await
    .map_err(Failure::into_message)?;
    let plugin = harness_outcome(&value, "wizard")?;
    Ok(WizardPluginChange {
        plugin,
        list: list().await,
    })
}

/// `wizard plugins remove <name> --for wizard --json`.
pub async fn remove(name: &str) -> Result<WizardPlugins, String> {
    let name = name.trim();
    if name.is_empty()
        || name.starts_with('-')
        || name.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(format!("{name:?} isn't a plugin name"));
    }
    let program = wizard_cli().ok_or("Wizard isn't installed on this device")?;
    let value = run(
        &program,
        &["plugins", "remove", name, "--for", "wizard", "--json"],
        LIST_TIMEOUT,
    )
    .await
    .map_err(Failure::into_message)?;
    harness_outcome(&value, "wizard")?;
    Ok(list().await)
}

/// The plugin one harness's half of a report carries, or its error.
fn harness_outcome(report: &serde_json::Value, harness: &str) -> Result<WizardPlugin, String> {
    let outcome = &report[harness];
    if outcome["ok"].as_bool() != Some(true) {
        return Err(outcome["error"]
            .as_str()
            .or_else(|| report["error"].as_str())
            .unwrap_or("Wizard didn't say what went wrong")
            .to_string());
    }
    serde_json::from_value(outcome["plugin"].clone()).map_err(|e| e.to_string())
}

enum Failure {
    /// This Wizard has no `plugins` subcommand.
    Unsupported,
    Message(String),
}

impl Failure {
    fn into_message(self) -> String {
        match self {
            Failure::Unsupported => {
                "This device's Wizard is too old for Pi plugins; update Wizard".to_string()
            }
            Failure::Message(message) => message,
        }
    }
}

/// Run `wizard <args>` and parse the JSON object it prints. A non-zero exit
/// still carries a JSON report (`ok: false`) when the subcommand exists.
async fn run(
    program: &std::path::Path,
    args: &[&str],
    timeout: Duration,
) -> Result<serde_json::Value, Failure> {
    let mut command = zeron_harness::install::cli_command(program);
    command
        .args(args)
        .env("NO_COLOR", "1")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let output = match tokio::time::timeout(timeout, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(err)) => return Err(Failure::Message(format!("Could not run wizard: {err}"))),
        Err(_) => {
            return Err(Failure::Message(format!(
                "wizard {} did not finish within {} seconds",
                args.get(1).copied().unwrap_or(""),
                timeout.as_secs()
            )));
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    parse_output(&stdout, &stderr, output.status.success())
}

fn parse_output(stdout: &str, stderr: &str, success: bool) -> Result<serde_json::Value, Failure> {
    if let Some(start) = stdout.find('{')
        && let Ok(value) = serde_json::from_str::<serde_json::Value>(stdout[start..].trim())
    {
        if value.get("ok").and_then(|ok| ok.as_bool()) == Some(false)
            && value.get("wizard").is_none()
            && let Some(error) = value.get("error").and_then(|e| e.as_str())
        {
            return Err(Failure::Message(error.to_string()));
        }
        return Ok(value);
    }
    if stderr.contains("unrecognized subcommand") {
        return Err(Failure::Unsupported);
    }
    let detail = stderr
        .lines()
        .chain(stdout.lines())
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim();
    Err(Failure::Message(if detail.is_empty() {
        format!(
            "wizard exited {}",
            if success {
                "without output"
            } else {
                "with an error"
            }
        )
    } else {
        detail.to_string()
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trimmed copy of what `wizard plugins install --for wizard --json`
    /// prints (src/pi_plugins in the Wizard repo, schema 1).
    const INSTALL: &str = r#"{
      "schema": 1, "beta": true, "ok": true, "target": "wizard",
      "wizard": { "ok": true, "plugin": {
        "name": "pi-fixture", "source": "npm:pi-fixture", "version": "1.2.0",
        "installedAt": "2026-09-28T15:30:15Z",
        "items": [
          {"kind":"skill","name":"pdf-tools","status":"installed","from":"skills/pdf-tools/SKILL.md","path":"skills/pdf-tools"},
          {"kind":"skill","name":"deep","status":"installed","from":"skills/deep/SKILL.md","path":"skills/deep",
           "notes":["disable-model-invocation is Pi-only"]},
          {"kind":"prompt","name":"review","status":"installed","from":"prompts/review.md","path":"commands/review.md"},
          {"kind":"prompt","name":"model","status":"skipped","reason":"Wizard has a built-in /model","from":"prompts/model.md"},
          {"kind":"extension","name":"extensions/index.ts","status":"unsupported","reason":"not supported by Wizard yet","from":"extensions/index.ts"}
        ] } }
    }"#;

    #[test]
    fn parses_an_install_report_and_counts_support() {
        let value = parse_output(INSTALL, "", true).ok().unwrap();
        let plugin = harness_outcome(&value, "wizard").unwrap();
        assert_eq!(plugin.name, "pi-fixture");
        assert_eq!(plugin.version.as_deref(), Some("1.2.0"));
        assert_eq!(plugin.items.len(), 5);
        assert_eq!(
            plugin.support(),
            (
                "2 skills, 1 prompt".to_string(),
                "1 prompt, 1 extension".to_string()
            )
        );
    }

    #[test]
    fn a_failed_half_reports_its_own_error() {
        let report = serde_json::json!({
            "schema": 1, "ok": false, "target": "wizard",
            "wizard": {"ok": false, "error": "pi-x isn't on npm"}
        });
        let value = parse_output(&report.to_string(), "", false).ok().unwrap();
        assert_eq!(
            harness_outcome(&value, "wizard").unwrap_err(),
            "pi-x isn't on npm"
        );
    }

    #[test]
    fn errors_and_old_wizards_are_told_apart() {
        let err = parse_output(
            r#"{"schema":1,"beta":true,"ok":false,"error":"bad spec"}"#,
            "",
            false,
        );
        assert!(matches!(err, Err(Failure::Message(m)) if m == "bad spec"));
        let old = parse_output("", "error: unrecognized subcommand 'plugins'\n", false);
        assert!(matches!(old, Err(Failure::Unsupported)));
        let noise = parse_output("", "warning: x\nthread panicked\n", false);
        assert!(matches!(noise, Err(Failure::Message(m)) if m == "thread panicked"));
        // Anything printed before the JSON object is ignored.
        let value = parse_output("note: hello\n{\"ok\":true,\"plugins\":[]}", "", true)
            .ok()
            .unwrap();
        assert_eq!(value["plugins"], serde_json::json!([]));
    }

    #[test]
    fn list_json_deserializes_with_loaded_flags() {
        let text = r#"{"schema":1,"beta":true,"ok":true,"plugins":[
            {"name":"a","source":"npm:a","items":[
              {"kind":"skill","name":"s","status":"installed","from":"SKILL.md","path":"skills/s","loaded":true}
            ]}],"pi":{"available":true,"packages":[]}}"#;
        let value = parse_output(text, "", true).ok().unwrap();
        let plugins: Vec<WizardPlugin> = serde_json::from_value(value["plugins"].clone()).unwrap();
        assert_eq!(plugins[0].items[0].loaded, Some(true));
        assert!(plugins[0].items[0].works());
        let list = WizardPlugins {
            wizard_installed: true,
            supported: false,
            ..WizardPlugins::default()
        };
        assert!(!list.available());
    }
}
