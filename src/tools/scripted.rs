//! Scripted tools: agent-authored scripts under `~/.wizard/tools/`
//! (the Hermes `execute_code` analog). Each tool is a script file plus a
//! sibling `<name>.toml` manifest; `/evolve` writes them and `/reload`
//! picks them up — no rebuild.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::process::Command;

use super::shell::{DEFAULT_TIMEOUT, MAX_TIMEOUT, render_command_result, run_command};
use super::{Tool, ToolContext, ToolError, ToolKind, ToolOutput};

/// Manifest stored as `~/.wizard/tools/<name>.toml`, describing the script
/// next to it.
///
/// ```toml
/// name = "mermaid-png"
/// description = "Render a mermaid diagram to PNG"
/// script = "mermaid-png.sh"
///
/// [parameters]            # JSON Schema (TOML-encoded) for the arguments
/// type = "object"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptManifest {
    /// Tool name advertised to the model (snake_case recommended).
    pub name: String,
    /// Description shown to the model.
    pub description: String,
    /// Script filename, relative to the manifest's directory.
    pub script: String,
    /// Interpreter to run the script with (default: execute directly,
    /// relying on the shebang).
    #[serde(default)]
    pub interpreter: Option<String>,
    /// JSON Schema for the arguments object. The arguments are passed to the
    /// script as a single JSON string in argv[1].
    #[serde(default = "ScriptManifest::default_parameters")]
    pub parameters: Value,
    /// Timeout in seconds (default 120).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

impl ScriptManifest {
    fn default_parameters() -> Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }
}

/// A loaded scripted tool: manifest plus resolved script path.
#[derive(Debug, Clone)]
pub struct ScriptedTool {
    pub manifest: ScriptManifest,
    /// Absolute path to the script file.
    pub script_path: PathBuf,
}

impl ScriptedTool {
    /// Load one scripted tool from its manifest file, resolving and
    /// validating the script path.
    pub fn load(manifest_path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(manifest_path)
            .with_context(|| format!("reading manifest {}", manifest_path.display()))?;
        let manifest: ScriptManifest = toml::from_str(&raw)
            .with_context(|| format!("parsing manifest {}", manifest_path.display()))?;

        ensure!(
            !manifest.name.trim().is_empty(),
            "manifest {} has an empty tool name",
            manifest_path.display()
        );
        ensure!(
            !manifest.script.trim().is_empty(),
            "manifest {} has an empty script filename",
            manifest_path.display()
        );

        let dir = manifest_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let script_path = dir.join(&manifest.script).canonicalize().with_context(|| {
            format!(
                "script '{}' referenced by {} not found",
                manifest.script,
                manifest_path.display()
            )
        })?;
        ensure!(
            script_path.is_file(),
            "script {} referenced by {} is not a regular file",
            script_path.display(),
            manifest_path.display()
        );

        Ok(Self {
            manifest,
            script_path,
        })
    }
}

/// Scan `dir` (normally `~/.wizard/tools/`) for `*.toml` manifests and load
/// each scripted tool. Returns an empty vec when the directory is missing.
/// Invalid manifests are skipped with a warning rather than failing the
/// whole load.
pub fn load_dir(dir: &Path) -> Result<Vec<ScriptedTool>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut manifests: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading scripted tools directory {}", dir.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    // Stable registration order across reloads.
    manifests.sort();

    let mut tools = Vec::with_capacity(manifests.len());
    for manifest_path in manifests {
        match ScriptedTool::load(&manifest_path) {
            Ok(tool) => tools.push(tool),
            Err(err) => {
                tracing::warn!(
                    manifest = %manifest_path.display(),
                    error = %format!("{err:#}"),
                    "skipping invalid scripted tool manifest"
                );
            }
        }
    }
    Ok(tools)
}

#[async_trait]
impl Tool for ScriptedTool {
    fn name(&self) -> &str {
        &self.manifest.name
    }

    fn description(&self) -> &str {
        &self.manifest.description
    }

    fn parameters(&self) -> Value {
        self.manifest.parameters.clone()
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Scripted
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args_json = serde_json::to_string(&args).map_err(|err| ToolError::InvalidArgs {
            tool: self.name().to_string(),
            message: format!("arguments are not serializable as JSON: {err}"),
        })?;

        let mut command = match &self.manifest.interpreter {
            Some(interpreter) => {
                // Allow interpreters with flags, e.g. "python3 -u".
                let mut parts = interpreter.split_whitespace();
                let program = parts.next().ok_or_else(|| ToolError::Execution {
                    tool: self.name().to_string(),
                    source: anyhow::anyhow!("manifest interpreter is empty"),
                })?;
                let mut command = Command::new(program);
                command.args(parts);
                command.arg(&self.script_path);
                command
            }
            None => Command::new(&self.script_path),
        };
        command.arg(&args_json).current_dir(&ctx.cwd);

        let timeout = self
            .manifest
            .timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_TIMEOUT)
            .clamp(Duration::from_secs(1), MAX_TIMEOUT);

        let result = run_command(self.name(), command, timeout).await?;
        Ok(render_command_result(&result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("wizard-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write_tool(dir: &Path, name: &str) {
        std::fs::write(
            dir.join(format!("{name}.sh")),
            "#!/bin/sh\necho \"args: $1\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join(format!("{name}.toml")),
            format!(
                "name = \"{name}\"\ndescription = \"test tool\"\nscript = \"{name}.sh\"\ninterpreter = \"sh\"\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn load_dir_missing_is_empty() {
        let tmp = TempDir::new();
        let missing = tmp.0.join("does-not-exist");
        assert!(load_dir(&missing).unwrap().is_empty());
    }

    #[test]
    fn load_dir_skips_invalid_manifests() {
        let tmp = TempDir::new();
        write_tool(&tmp.0, "good");
        // Manifest referencing a script that does not exist.
        std::fs::write(
            tmp.0.join("bad.toml"),
            "name = \"bad\"\ndescription = \"x\"\nscript = \"missing.sh\"\n",
        )
        .unwrap();

        let tools = load_dir(&tmp.0).unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].manifest.name, "good");
    }

    #[test]
    fn load_rejects_empty_name_and_missing_script() {
        let tmp = TempDir::new();
        std::fs::write(tmp.0.join("noname.sh"), "#!/bin/sh\n").unwrap();
        std::fs::write(
            tmp.0.join("noname.toml"),
            "name = \"  \"\ndescription = \"x\"\nscript = \"noname.sh\"\n",
        )
        .unwrap();
        let err = ScriptedTool::load(&tmp.0.join("noname.toml")).unwrap_err();
        assert!(format!("{err:#}").contains("empty tool name"), "{err:#}");

        std::fs::write(
            tmp.0.join("ghost.toml"),
            "name = \"ghost\"\ndescription = \"x\"\nscript = \"ghost.sh\"\n",
        )
        .unwrap();
        let err = ScriptedTool::load(&tmp.0.join("ghost.toml")).unwrap_err();
        assert!(format!("{err:#}").contains("not found"), "{err:#}");
    }

    #[tokio::test]
    async fn failing_script_reports_stderr_and_exit_code_as_error() {
        let tmp = TempDir::new();
        std::fs::write(
            tmp.0.join("fail.sh"),
            "#!/bin/sh\necho \"went wrong\" >&2\nexit 3\n",
        )
        .unwrap();
        std::fs::write(
            tmp.0.join("fail.toml"),
            "name = \"fail\"\ndescription = \"x\"\nscript = \"fail.sh\"\ninterpreter = \"sh\"\n",
        )
        .unwrap();
        let tool = ScriptedTool::load(&tmp.0.join("fail.toml")).unwrap();
        let ctx = ToolContext::new(&tmp.0);
        let out = tool.execute(serde_json::json!({}), &ctx).await.unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("went wrong"), "{}", out.content);
        assert!(out.content.contains("exit code: 3"), "{}", out.content);
    }

    #[tokio::test]
    async fn interpreter_flags_are_split_and_passed() {
        let tmp = TempDir::new();
        std::fs::write(tmp.0.join("strict.sh"), "false\necho reached\n").unwrap();
        std::fs::write(
            tmp.0.join("strict.toml"),
            "name = \"strict\"\ndescription = \"x\"\nscript = \"strict.sh\"\ninterpreter = \"sh -e\"\n",
        )
        .unwrap();
        let tool = ScriptedTool::load(&tmp.0.join("strict.toml")).unwrap();
        let ctx = ToolContext::new(&tmp.0);
        let out = tool.execute(serde_json::json!({}), &ctx).await.unwrap();
        assert!(out.is_error, "sh -e stops at the failing command");
        assert!(!out.content.contains("reached"), "{}", out.content);
    }

    #[tokio::test]
    async fn scripted_tool_passes_args_json() {
        let tmp = TempDir::new();
        write_tool(&tmp.0, "echoer");

        let tools = load_dir(&tmp.0).unwrap();
        assert_eq!(tools.len(), 1);

        let ctx = ToolContext::new(&tmp.0);
        let out = tools[0]
            .execute(serde_json::json!({ "key": "value" }), &ctx)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("args: {\"key\":\"value\"}"));
    }
}
