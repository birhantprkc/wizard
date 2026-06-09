//! Native git tools: `git_status` and `git_diff` (shelling out to `git`).

use std::ffi::OsStr;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::process::Command;

use super::shell::{CommandResult, run_command};
use super::{
    MAX_OUTPUT_BYTES, Tool, ToolContext, ToolError, ToolOutput, parse_args, truncate_output,
};

/// Timeout for git subprocesses. Status and diff are local operations, so
/// anything slower than this indicates a wedged repository.
const GIT_TIMEOUT: Duration = Duration::from_secs(30);

/// Run `git <args>` in the project root.
async fn run_git<I, S>(tool: &str, ctx: &ToolContext, args: I) -> Result<CommandResult, ToolError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = Command::new("git");
    command.args(args).current_dir(&ctx.cwd);
    run_command(tool, command, GIT_TIMEOUT).await
}

/// Model-facing error output for a failed git invocation.
fn git_failure(result: &CommandResult, fallback: &str) -> ToolOutput {
    let stderr = result.stderr.trim_end();
    let detail = if stderr.is_empty() { fallback } else { stderr };
    ToolOutput::error(truncate_output(detail.to_string(), MAX_OUTPUT_BYTES))
}

/// `git_status` — working tree status (`git status --porcelain=v1 -b`).
pub struct GitStatusTool;

#[async_trait]
impl Tool for GitStatusTool {
    fn name(&self) -> &str {
        "git_status"
    }

    fn description(&self) -> &str {
        "Show the git working tree status of the project (branch, staged, modified, and untracked files)."
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let _ = args; // no arguments

        let result = run_git(self.name(), ctx, ["status", "--porcelain=v1", "-b"]).await?;
        if result.code != Some(0) {
            return Ok(git_failure(&result, "git status failed"));
        }

        let status = result.stdout.trim_end();
        // Porcelain v1 with `-b` always emits a `## branch` header first;
        // a header-only output means a clean tree.
        let content = if status.lines().count() <= 1 {
            format!("{status}\n(clean working tree)")
        } else {
            status.to_string()
        };
        Ok(ToolOutput::ok(truncate_output(content, MAX_OUTPUT_BYTES)))
    }
}

/// Arguments for [`GitDiffTool`].
#[derive(Debug, Deserialize)]
pub struct GitDiffArgs {
    /// Diff the index (staged changes) instead of the working tree.
    #[serde(default)]
    pub staged: bool,
    /// Limit the diff to a single path.
    #[serde(default)]
    pub path: Option<String>,
}

/// `git_diff` — staged or unstaged diff. Also backs the TUI diff sidebar.
pub struct GitDiffTool;

#[async_trait]
impl Tool for GitDiffTool {
    fn name(&self) -> &str {
        "git_diff"
    }

    fn description(&self) -> &str {
        "Show the git diff of the project: unstaged changes by default, staged with staged=true, optionally limited to one path."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "staged": { "type": "boolean", "description": "Diff staged changes instead of the working tree" },
                "path": { "type": "string", "description": "Limit the diff to this path" }
            }
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: GitDiffArgs = parse_args(self.name(), args)?;

        let mut argv: Vec<String> = vec!["diff".to_string()];
        if args.staged {
            argv.push("--cached".to_string());
        }
        if let Some(path) = args.path {
            argv.push("--".to_string());
            argv.push(path);
        }

        let result = run_git(self.name(), ctx, &argv).await?;
        if result.code != Some(0) {
            return Ok(git_failure(&result, "git diff failed"));
        }

        let diff = result.stdout.trim_end();
        if diff.is_empty() {
            Ok(ToolOutput::ok("No changes."))
        } else {
            Ok(ToolOutput::ok(truncate_output(
                diff.to_string(),
                MAX_OUTPUT_BYTES,
            )))
        }
    }
}
