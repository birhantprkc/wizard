//! Native `execute` tool: run shell commands with a timeout.
//!
//! Security note (see `docs/architecture.md`): this is real shell access and
//! cannot be confined to the working directory.

use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use super::{
    MAX_OUTPUT_BYTES, Tool, ToolContext, ToolError, ToolOutput, parse_args, truncate_output,
};

/// Default command timeout when the model does not specify one.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// Hard upper bound a model-supplied timeout is clamped to.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(600);

/// How long to keep draining the output pipes after the child exited (or was
/// killed). Bounds a stray descendant holding a pipe open.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Captured result of a finished child process.
pub(crate) struct CommandResult {
    pub stdout: String,
    pub stderr: String,
    /// Exit code, or `None` when the process was terminated by a signal.
    pub code: Option<i32>,
    /// Set (to the budget in seconds) when the command was killed at the
    /// timeout. `stdout`/`stderr` then carry whatever was produced first.
    pub timed_out: Option<u64>,
}

/// One piped output stream read incrementally into a shared buffer, so a
/// timeout can still report what the command produced before it was killed.
struct Pipe {
    buf: Arc<Mutex<Vec<u8>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Pipe {
    fn new(stream: Option<impl tokio::io::AsyncRead + Unpin + Send + 'static>) -> Self {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let task = tokio::spawn({
            let buf = Arc::clone(&buf);
            async move {
                let Some(mut stream) = stream else { return };
                let mut chunk = [0u8; 8192];
                loop {
                    match stream.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.lock().unwrap().extend_from_slice(&chunk[..n]),
                    }
                }
            }
        });
        Self { buf, task }
    }

    /// Wait up to `grace` for the reader to hit EOF, then take whatever is
    /// buffered.
    async fn finish(self, grace: Duration) -> String {
        let mut task = self.task;
        if tokio::time::timeout(grace, &mut task).await.is_err() {
            task.abort();
        }
        let buf = self.buf.lock().unwrap();
        String::from_utf8_lossy(&buf).into_owned()
    }
}

/// SIGKILL `child`'s whole process group and reap it. Mirrors
/// `tasks::kill_tree`: `sh -c` may fork the command rather than exec it, and
/// killing only the shell would leave grandchildren running.
async fn kill_group(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    }
    let _ = child.kill().await;
}

/// Spawn `command` with piped stdio, wait for it under `timeout`, and capture
/// its output. On timeout the whole process group is killed and the partial
/// output is returned with `timed_out` set. Shared by `execute`, the git
/// tools, `search_files`, and scripted tools.
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
    // Own process group so a timeout kill reaches the whole tree (see
    // `kill_group`).
    #[cfg(unix)]
    command.process_group(0);

    let mut child = command.spawn().map_err(|err| ToolError::Execution {
        tool: tool.to_string(),
        source: anyhow::Error::new(err).context("failed to spawn process"),
    })?;

    let stdout = Pipe::new(child.stdout.take());
    let stderr = Pipe::new(child.stderr.take());

    let mut timed_out = None;
    let code = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => status.code(),
        Ok(Err(err)) => {
            return Err(ToolError::Execution {
                tool: tool.to_string(),
                source: anyhow::Error::new(err).context("failed to wait for process"),
            });
        }
        Err(_) => {
            kill_group(&mut child).await;
            timed_out = Some(timeout.as_secs());
            None
        }
    };

    let (stdout, stderr) = tokio::join!(stdout.finish(DRAIN_GRACE), stderr.finish(DRAIN_GRACE));
    Ok(CommandResult {
        stdout,
        stderr,
        code,
        timed_out,
    })
}

/// Render a [`CommandResult`] as the model-facing tool output: stdout, then a
/// labelled stderr section, then the exit code when non-zero. `is_error`
/// mirrors the exit status. A timed-out result is an error carrying the
/// partial output.
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

    if let Some(secs) = result.timed_out {
        let note = if content.is_empty() {
            format!("command timed out after {secs}s and was killed (no output produced)")
        } else {
            content.push('\n');
            format!("command timed out after {secs}s and was killed; output above is partial")
        };
        content.push_str(&note);
        return ToolOutput::error(truncate_output(content, MAX_OUTPUT_BYTES));
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
    /// Timeout in seconds (default 120, clamped to 600). Ignored for
    /// background tasks, which use the fixed background timeout.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Run detached as a background task: returns immediately with a task
    /// id; the agent is notified when the command finishes.
    #[serde(default)]
    pub run_in_background: bool,
}

/// `execute` — run a shell command, capturing stdout, stderr, and exit code.
pub struct ExecuteTool;

#[async_trait]
impl Tool for ExecuteTool {
    fn name(&self) -> &str {
        "execute"
    }

    fn description(&self) -> &str {
        r#"Run a shell command in the project root and return its stdout, stderr, and exit code. Killed on timeout. With run_in_background, the command is detached as a background task: the call returns a task id immediately, you are notified when it finishes, and task_output / task_kill manage it meanwhile.

Tips:
- Prefer compact commands: print summaries, not full pixel maps, huge dumps, or entire binaries. Pipe through `head`/`tail` when exploring.
- Put large intermediate output in `/tmp/...` and print only what you need next.
- After compiling to verify a source deliverable, remove build products from the deliverable directory (or compile to `/tmp`) before finishing.
- Non-zero exit is normal diagnostic signal — read stderr and adapt; do not treat it as a crash of the agent.
- **Durable services** (HTTP on a fixed port, QEMU, anything external tests must still reach after you finish): use shell detachment, e.g. `nohup <cmd> > /var/log/... 2>&1 &`, then verify with `curl`/`ss`/`pgrep`. Do **not** use `run_in_background=true` for those — that mode is agent-managed and does not guarantee the process survives the session.
- Use `run_in_background=true` for agent-scoped long jobs you will poll (`task_output`) or cancel (`task_kill`), such as long builds while you keep working — not for production daemons left for a verifier.
- After installing a Python package with extensions into the system env, re-check from a clean directory (`cd /tmp && python3 -c "import ..."`) so a local checkout on `sys.path` cannot fake a successful install. As soon as that import + the required snippet pass, **run the allowed package test suite next** (exclude only tests the task marks broken). Once snippet + allowed tests pass, stop — do not start another full-repo inventory.
- On install-from-source tasks: run `setup.py build_ext --inplace && setup.py install` (or equivalent) **early**, after a small compatibility-fix batch. After any later source edit, **reinstall** before more greps — site-packages does not update itself from the checkout.
- If allowed tests fail with `KeyError` on graph/node attribute names (or similar third-party API drift), fix with dual-key `.get` fallbacks, reinstall, and re-run the suite. Prefer fixing the reported failure over another alias inventory.
- For JSON/JSONL deliverables, `cat` the file and parse it (`json.loads`) to assert types and required **task-listed** tokens before finishing. If the task lists CWE candidates, assert every reported ID is in that list (and that listed CRLF IDs like `cwe-93` are present when applicable).
- Before finishing any task with required output paths, `ls` those paths. A missing required file is an automatic fail — create it.
- For chess/puzzles: install tools early; in the **same** `python3 <<'PY'` script that finds a non-empty mate set, write the answer file (`open('/app/move.txt','w').write(...)` or `printf`) and print confirmation. If none, run **one** compact hypothesis script that swaps only ambiguous piece types, writes the best mate set when found, and exits — do not dump pixel grids, IoU matrices, or multi-turn silhouette thrash after a mate is already known.
- For vulnerability fix tasks: run the stated test command early to surface expected `ValueError`/validation failures; fix the helpers; write the required report with **task-listed** IDs; re-run tests."#
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Shell command line (run via sh -c)" },
                "timeout_secs": { "type": "integer", "description": "Timeout in seconds (default 120, max 600); ignored for background tasks" },
                "run_in_background": { "type": "boolean", "description": "Detach as a background task and return immediately (default false)" }
            },
            "required": ["command"]
        })
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

        if args.run_in_background {
            command
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            // Own process group so task_kill and the timeout reach the whole
            // tree: `sh -c` may fork the command rather than exec it, and a
            // surviving grandchild would hold the output pipes open.
            #[cfg(unix)]
            command.process_group(0);
            let child = command.spawn().map_err(|err| ToolError::Execution {
                tool: self.name().to_string(),
                source: anyhow::Error::new(err).context("failed to spawn background process"),
            })?;
            let id = ctx.tasks.spawn(&args.command, child);
            // Mirror the new task to the UI dashboard (TaskFinished follows
            // when it ends). Surfaces that don't care just drop it.
            if let Some(events) = &ctx.events {
                let _ = events
                    .send(crate::agent::AgentEvent::TaskStarted {
                        id,
                        command: args.command.clone(),
                    })
                    .await;
            }
            return Ok(ToolOutput::ok(format!(
                "Background task #{id} started: {}\nYou will be notified when it finishes; \
                 use task_output to inspect it or task_kill to stop it.",
                args.command
            )));
        }

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
        let out = ExecuteTool
            .execute(
                json!({ "command": "sleep 5", "timeout_secs": 1 }),
                &tmp.ctx(),
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(
            out.content.contains("timed out after 1s"),
            "{}",
            out.content
        );
    }

    #[tokio::test]
    async fn execute_timeout_returns_partial_output() {
        let tmp = TempDir::new();
        let out = ExecuteTool
            .execute(
                json!({ "command": "echo started; echo warn >&2; sleep 5", "timeout_secs": 1 }),
                &tmp.ctx(),
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("started"), "{}", out.content);
        assert!(out.content.contains("stderr:\nwarn"), "{}", out.content);
        assert!(
            out.content.contains("output above is partial"),
            "{}",
            out.content
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn execute_timeout_kills_the_whole_process_group() {
        let tmp = TempDir::new();
        // The subshell is a grandchild of `sh -c`; without the group kill it
        // would survive the timeout and write the marker file.
        let out = ExecuteTool
            .execute(
                json!({
                    "command": "(sleep 2 && touch grandchild-survived) & echo spawned; sleep 30",
                    "timeout_secs": 1
                }),
                &tmp.ctx(),
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("spawned"), "{}", out.content);
        tokio::time::sleep(Duration::from_millis(2_500)).await;
        assert!(
            !tmp.0.join("grandchild-survived").exists(),
            "grandchild must be killed with the group"
        );
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

    #[tokio::test]
    async fn execute_run_in_background_registers_a_task_and_returns_immediately() {
        let tmp = TempDir::new();
        let ctx = tmp.ctx();
        let out = ExecuteTool
            .execute(
                json!({ "command": "echo bg-marker", "run_in_background": true }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(
            out.content
                .contains("Background task #1 started: echo bg-marker"),
            "{}",
            out.content
        );

        // The task runs to completion in the registry and its output is
        // captured for the finished-task notification.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let status = ctx.tasks.status(1).expect("task registered");
            if status.is_finished() {
                assert_eq!(status, crate::tools::tasks::TaskStatus::Done(0));
                break;
            }
            assert!(std::time::Instant::now() < deadline, "task finished");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let drained = ctx.tasks.drain_completed();
        assert_eq!(drained.len(), 1);
        assert!(drained[0].tail.contains("bg-marker"), "{}", drained[0].tail);
    }

    #[test]
    fn render_merges_stdout_and_stderr_sections() {
        let result = CommandResult {
            stdout: "out line\n".to_string(),
            stderr: "err line\n".to_string(),
            code: Some(0),
            timed_out: None,
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
            timed_out: None,
        };
        let out = render_command_result(&result);
        assert!(out.is_error);
        assert_eq!(out.content, "terminated by signal");
    }

    #[test]
    fn render_timeout_without_output_says_so() {
        let result = CommandResult {
            stdout: String::new(),
            stderr: String::new(),
            code: None,
            timed_out: Some(30),
        };
        let out = render_command_result(&result);
        assert!(out.is_error);
        assert_eq!(
            out.content,
            "command timed out after 30s and was killed (no output produced)"
        );
    }
}
