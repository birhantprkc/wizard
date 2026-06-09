//! Native `execute` tool: run shell commands with a timeout.
//!
//! Security note (see `docs/architecture.md`): this is real shell access and
//! cannot be confined to the working directory. Genie mode gates it behind
//! approval; sovereign mode auto-approves.

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::process::Command;

use super::{
    MAX_OUTPUT_BYTES, Tool, ToolContext, ToolError, ToolOutput, parse_args, truncate_output,
};

/// Default command timeout when the model does not specify one.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// Hard upper bound a model-supplied timeout is clamped to.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(600);

/// Captured result of a finished child process.
pub(crate) struct CommandResult {
    pub stdout: String,
    pub stderr: String,
    /// Exit code, or `None` when the process was terminated by a signal.
    pub code: Option<i32>,
}

/// Spawn `command` with piped stdio, wait for it under `timeout`, and capture
/// its output. The child is killed if the timeout elapses (via
/// `kill_on_drop`). Shared by `execute`, the git tools, `search_files`, and
/// scripted tools.
pub(crate) async fn run_command(
    tool: &str,
    mut command: Command,
    timeout: Duration,
) -> Result<CommandResult, ToolError> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let child = command.spawn().map_err(|err| ToolError::Execution {
        tool: tool.to_string(),
        source: anyhow::Error::new(err).context("failed to spawn process"),
    })?;

    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(err)) => {
            return Err(ToolError::Execution {
                tool: tool.to_string(),
                source: anyhow::Error::new(err).context("failed to collect process output"),
            });
        }
        Err(_) => {
            return Err(ToolError::Timeout {
                tool: tool.to_string(),
                seconds: timeout.as_secs(),
            });
        }
    };

    Ok(CommandResult {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        code: output.status.code(),
    })
}

/// Render a [`CommandResult`] as the model-facing tool output: stdout, then a
/// labelled stderr section, then the exit code when non-zero. `is_error`
/// mirrors the exit status.
pub(crate) fn render_command_result(result: &CommandResult) -> ToolOutput {
    let stdout = result.stdout.trim_end();
    let stderr = result.stderr.trim_end();

    let mut content = String::new();
    if !stdout.is_empty() {
        content.push_str(stdout);
    }
    if !stderr.is_empty() {
        if !content.is_empty() {
            content.push('\n');
        }
        content.push_str("stderr:\n");
        content.push_str(stderr);
    }

    match result.code {
        Some(0) => {
            if content.is_empty() {
                content.push_str("(command succeeded with no output)");
            }
            ToolOutput::ok(truncate_output(content, MAX_OUTPUT_BYTES))
        }
        Some(code) => {
            if !content.is_empty() {
                content.push('\n');
            }
            content.push_str(&format!("exit code: {code}"));
            ToolOutput::error(truncate_output(content, MAX_OUTPUT_BYTES))
        }
        None => {
            if !content.is_empty() {
                content.push('\n');
            }
            content.push_str("terminated by signal");
            ToolOutput::error(truncate_output(content, MAX_OUTPUT_BYTES))
        }
    }
}

/// Arguments for [`ExecuteTool`].
#[derive(Debug, Deserialize)]
pub struct ExecuteArgs {
    /// Shell command line, run via `sh -c` in the project root.
    pub command: String,
    /// Timeout in seconds (default 120, clamped to 600).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// `execute` — run a shell command, capturing stdout, stderr, and exit code.
pub struct ExecuteTool;

#[async_trait]
impl Tool for ExecuteTool {
    fn name(&self) -> &str {
        "execute"
    }

    fn description(&self) -> &str {
        "Run a shell command in the project root and return its stdout, stderr, and exit code. Killed on timeout."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Shell command line (run via sh -c)" },
                "timeout_secs": { "type": "integer", "description": "Timeout in seconds (default 120, max 600)" }
            },
            "required": ["command"]
        })
    }

    fn requires_approval(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: ExecuteArgs = parse_args(self.name(), args)?;
        if args.command.trim().is_empty() {
            return Err(ToolError::InvalidArgs {
                tool: self.name().to_string(),
                message: "command must not be empty".to_string(),
            });
        }

        let timeout = match args.timeout_secs {
            Some(0) => {
                return Err(ToolError::InvalidArgs {
                    tool: self.name().to_string(),
                    message: "timeout_secs must be at least 1".to_string(),
                });
            }
            Some(secs) => Duration::from_secs(secs).min(MAX_TIMEOUT),
            None => DEFAULT_TIMEOUT,
        };

        let mut command = Command::new("sh");
        command.arg("-c").arg(&args.command).current_dir(&ctx.cwd);

        let result = run_command(self.name(), command, timeout).await?;
        Ok(render_command_result(&result))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// Temp project dir removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("wizard-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }

        fn ctx(&self) -> ToolContext {
            ToolContext::new(&self.0)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn execute_captures_stdout() {
        let tmp = TempDir::new();
        let out = ExecuteTool
            .execute(json!({ "command": "echo spellbook" }), &tmp.ctx())
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(out.content, "spellbook");
    }

    #[tokio::test]
    async fn execute_runs_in_project_root() {
        let tmp = TempDir::new();
        let out = ExecuteTool
            .execute(json!({ "command": "pwd" }), &tmp.ctx())
            .await
            .unwrap();
        let reported = std::fs::canonicalize(out.content.trim()).unwrap();
        let expected = std::fs::canonicalize(&tmp.0).unwrap();
        assert_eq!(reported, expected);
    }

    #[tokio::test]
    async fn execute_times_out_and_reports_seconds() {
        let tmp = TempDir::new();
        let err = ExecuteTool
            .execute(
                json!({ "command": "sleep 5", "timeout_secs": 1 }),
                &tmp.ctx(),
            )
            .await
            .expect_err("must time out");
        match err {
            ToolError::Timeout { tool, seconds } => {
                assert_eq!(tool, "execute");
                assert_eq!(seconds, 1);
            }
            other => panic!("expected Timeout, got: {other}"),
        }
    }

    #[tokio::test]
    async fn execute_nonzero_exit_is_tool_output_error() {
        let tmp = TempDir::new();
        let out = ExecuteTool
            .execute(json!({ "command": "echo oops >&2; exit 3" }), &tmp.ctx())
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("stderr:\noops"));
        assert!(out.content.contains("exit code: 3"));
    }

    #[tokio::test]
    async fn execute_success_with_no_output_says_so() {
        let tmp = TempDir::new();
        let out = ExecuteTool
            .execute(json!({ "command": "true" }), &tmp.ctx())
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(out.content, "(command succeeded with no output)");
    }

    #[tokio::test]
    async fn execute_rejects_empty_command() {
        let tmp = TempDir::new();
        let err = ExecuteTool
            .execute(json!({ "command": "   " }), &tmp.ctx())
            .await
            .expect_err("blank command must be rejected");
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn execute_rejects_zero_timeout() {
        let tmp = TempDir::new();
        let err = ExecuteTool
            .execute(json!({ "command": "true", "timeout_secs": 0 }), &tmp.ctx())
            .await
            .expect_err("zero timeout must be rejected");
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn execute_rejects_missing_command_argument() {
        let tmp = TempDir::new();
        let err = ExecuteTool
            .execute(json!({}), &tmp.ctx())
            .await
            .expect_err("missing command must be rejected");
        assert!(matches!(err, ToolError::InvalidArgs { tool, .. } if tool == "execute"));
    }

    #[test]
    fn render_merges_stdout_and_stderr_sections() {
        let result = CommandResult {
            stdout: "out line\n".to_string(),
            stderr: "err line\n".to_string(),
            code: Some(0),
        };
        let out = render_command_result(&result);
        assert!(!out.is_error);
        assert_eq!(out.content, "out line\nstderr:\nerr line");
    }

    #[test]
    fn render_signal_termination_is_an_error() {
        let result = CommandResult {
            stdout: String::new(),
            stderr: String::new(),
            code: None,
        };
        let out = render_command_result(&result);
        assert!(out.is_error);
        assert_eq!(out.content, "terminated by signal");
    }
}
