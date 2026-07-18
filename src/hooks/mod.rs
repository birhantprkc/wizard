//! Lifecycle hooks: user-supplied shell commands that observe and steer the
//! agent at fixed points of every mode (TUI genie, sovereign headless,
//! perpetual continuous, gateway). See `docs/hooks.md`.
//!
//! Hooks are declared in `~/.wizard/hooks.toml` and
//! `<project>/.wizard/hooks.toml` (project hooks run after global ones) and
//! receive a JSON payload on stdin. Exit 0 continues — depending on the
//! event, stdout may rewrite tool arguments or append extra context. Exit 2
//! blocks, with stderr as the reason. Any other exit code, a timeout, or a
//! spawn failure is a logged warning and the pipeline continues: a broken
//! hook can never wedge the agent.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;

use globset::{Glob, GlobMatcher};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

use crate::agent::AgentEvent;
use crate::config::{Config, Mode};

/// Wall-clock budget for one hook when `timeout_secs` is not set.
const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// Cap on the `tool_output` field in `post_tool_use` payloads.
const TOOL_OUTPUT_CAP_BYTES: usize = 32 * 1024;

/// `output` capped to [`TOOL_OUTPUT_CAP_BYTES`] on a char boundary, with a
/// marker when content was dropped.
fn cap_tool_output(output: &str) -> String {
    if output.len() <= TOOL_OUTPUT_CAP_BYTES {
        return output.to_string();
    }
    let mut cut = TOOL_OUTPUT_CAP_BYTES;
    while cut > 0 && !output.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n... [tool output truncated]", &output[..cut])
}

/// Lifecycle points a hook can attach to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    /// Before a tool call executes. May rewrite the arguments or block.
    PreToolUse,
    /// After a tool call executes. Stdout is appended to the tool result.
    PostToolUse,
    /// When a user message starts a turn. Stdout is appended to the message;
    /// exit 2 ends the turn before the model sees it.
    UserPromptSubmit,
    /// Once when a session begins. Stdout becomes system context.
    SessionStart,
    /// Once when a session ends. Observational.
    SessionEnd,
    /// After every turn finishes. Observational.
    TurnEnd,
}

impl HookEvent {
    /// The snake_case name used in `hooks.toml` and the stdin payload.
    pub fn name(self) -> &'static str {
        match self {
            HookEvent::PreToolUse => "pre_tool_use",
            HookEvent::PostToolUse => "post_tool_use",
            HookEvent::UserPromptSubmit => "user_prompt_submit",
            HookEvent::SessionStart => "session_start",
            HookEvent::SessionEnd => "session_end",
            HookEvent::TurnEnd => "turn_end",
        }
    }
}

/// One `[[hooks]]` entry from a `hooks.toml` file.
#[derive(Debug, Clone, Deserialize)]
pub struct HookDef {
    /// Which lifecycle event this hook fires on.
    pub event: HookEvent,
    /// Optional glob over the tool name (`"execute"`, `"git_*"`). Only
    /// meaningful for the tool events; other events fire regardless.
    #[serde(default)]
    pub matcher: Option<String>,
    /// Shell command, run via `sh -c` in the project root.
    pub command: String,
    /// Wall-clock budget; the hook is killed and ignored past it
    /// (default 60).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// On-disk shape of a `hooks.toml` file.
#[derive(Debug, Default, Deserialize)]
struct HooksFile {
    #[serde(default)]
    hooks: Vec<HookDef>,
}

/// Parse the contents of one `hooks.toml`.
pub fn parse(raw: &str) -> Result<Vec<HookDef>, toml::de::Error> {
    toml::from_str::<HooksFile>(raw).map(|file| file.hooks)
}

/// Load hook definitions for a project: global `~/.wizard/hooks.toml` first,
/// then `<project>/.wizard/hooks.toml` appended after it.
pub fn load(project_root: &Path) -> Vec<HookDef> {
    let mut paths = Vec::new();
    match Config::wizard_dir() {
        Ok(dir) => paths.push(dir.join("hooks.toml")),
        Err(err) => tracing::warn!("could not resolve ~/.wizard for hooks: {err}"),
    }
    paths.push(project_root.join(".wizard").join("hooks.toml"));
    load_files(&paths)
}

/// Read and parse each path in order, concatenating the results. Missing
/// files mean no hooks; unreadable or invalid files are skipped with a
/// warning so a bad hook file can never prevent startup.
fn load_files(paths: &[PathBuf]) -> Vec<HookDef> {
    let mut defs = Vec::new();
    for path in paths {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                tracing::warn!("could not read {}: {err}", path.display());
                continue;
            }
        };
        match parse(&raw) {
            Ok(mut hooks) => defs.append(&mut hooks),
            Err(err) => tracing::warn!("skipping invalid {}: {err}", path.display()),
        }
    }
    defs
}

/// What one hook execution did, surfaced via [`AgentEvent::HookFired`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookOutcome {
    /// `pre_tool_use` rewrote the tool arguments.
    UpdatedArgs,
    /// Stdout was appended as extra context.
    AppendedContext,
    /// Exit 2: the action was blocked for this reason.
    Blocked(String),
    /// The hook misbehaved (other exit code, timeout, spawn failure) and was
    /// ignored.
    Warning(String),
}

impl fmt::Display for HookOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HookOutcome::UpdatedArgs => write!(f, "updated args"),
            HookOutcome::AppendedContext => write!(f, "appended context"),
            HookOutcome::Blocked(reason) => write!(f, "blocked — {reason}"),
            HookOutcome::Warning(why) => write!(f, "warning — {why}"),
        }
    }
}

/// Verdict of the `pre_tool_use` chain for one tool call.
#[derive(Debug)]
pub enum PreToolUse {
    /// Proceed; `Some` holds rewritten arguments.
    Continue(Option<Value>),
    /// Veto the call; the reason feeds back to the model as a tool error.
    Block(String),
}

/// Verdict of the `user_prompt_submit` chain for one turn.
#[derive(Debug)]
pub enum PromptSubmit {
    /// Proceed; `Some` holds extra context to append to the message.
    Continue(Option<String>),
    /// End the turn before the model sees the prompt.
    Block(String),
}

/// Append `post_tool_use` hook context to a tool result body. Shared by the
/// dispatcher and the subagent loop so the framing stays identical.
pub fn append_context(content: &mut String, extra: &str) {
    if !content.is_empty() {
        content.push_str("\n\n");
    }
    content.push_str("[post_tool_use hook]\n");
    content.push_str(extra);
}

/// A [`HookDef`] with its matcher glob compiled.
struct CompiledHook {
    def: HookDef,
    matcher: Option<GlobMatcher>,
}

/// How one hook process finished.
enum RunResult {
    Exited {
        code: i32,
        stdout: String,
        stderr: String,
    },
    /// Spawn failure, wait failure, or timeout.
    Failed(String),
}

/// Internal verdict of [`HookEngine::fire`].
enum FireResult {
    Continue(Option<String>),
    Block(String),
}

/// Runs the configured hooks for lifecycle events. Built once per agent and
/// shared (via `Arc`) by the dispatcher, the agent loop, and the subagent
/// spawner, so every tool call in every mode sees the same hooks. With no
/// hooks configured every fire point reduces to an empty-Vec scan.
pub struct HookEngine {
    hooks: Vec<CompiledHook>,
    /// Project root: hooks run here and it is the payload `cwd`.
    cwd: PathBuf,
    /// Current session id for payloads; swapped when `/clear` starts a new
    /// session.
    session_id: Mutex<String>,
}

impl HookEngine {
    /// Compile `defs` into an engine. A hook with an invalid matcher glob is
    /// dropped with a warning.
    pub fn new(defs: Vec<HookDef>, cwd: PathBuf, session_id: String) -> Self {
        let hooks = defs
            .into_iter()
            .filter_map(|def| {
                let matcher = match &def.matcher {
                    Some(pattern) => match Glob::new(pattern) {
                        Ok(glob) => Some(glob.compile_matcher()),
                        Err(err) => {
                            tracing::warn!(
                                "dropping hook '{}': invalid matcher '{pattern}': {err}",
                                def.command
                            );
                            return None;
                        }
                    },
                    None => None,
                };
                Some(CompiledHook { def, matcher })
            })
            .collect();
        Self {
            hooks,
            cwd,
            session_id: Mutex::new(session_id),
        }
    }

    /// True when no hooks are configured (the default).
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    /// Swap the payload session id (after `/clear` starts a new session).
    pub fn set_session_id(&self, id: String) {
        *self.session_id.lock().unwrap() = id;
    }

    /// `pre_tool_use`: run the matching hooks before `tool` executes. Exit 2
    /// vetoes the call; exit 0 with stdout `{"updated_args": {...}}` rewrites
    /// the arguments (later hooks in the chain see the rewritten args).
    pub async fn pre_tool_use(
        &self,
        tool: &str,
        args: &Value,
        mode: Mode,
        events: Option<&mpsc::Sender<AgentEvent>>,
    ) -> PreToolUse {
        let mut updated: Option<Value> = None;
        for hook in self.matching(HookEvent::PreToolUse, Some(tool)) {
            let effective = updated.as_ref().unwrap_or(args);
            let run = self
                .run(
                    hook,
                    HookEvent::PreToolUse,
                    Some((tool, effective)),
                    None,
                    mode,
                )
                .await;
            match run {
                RunResult::Exited {
                    code: 0, stdout, ..
                } => {
                    if let Some(args) = parse_updated_args(&stdout) {
                        updated = Some(args);
                        self.report(
                            events,
                            HookEvent::PreToolUse,
                            hook,
                            HookOutcome::UpdatedArgs,
                        )
                        .await;
                    }
                }
                RunResult::Exited {
                    code: 2, stderr, ..
                } => {
                    let reason = block_reason(&stderr);
                    self.report(
                        events,
                        HookEvent::PreToolUse,
                        hook,
                        HookOutcome::Blocked(reason.clone()),
                    )
                    .await;
                    return PreToolUse::Block(reason);
                }
                RunResult::Exited { code, stderr, .. } => {
                    self.report(
                        events,
                        HookEvent::PreToolUse,
                        hook,
                        HookOutcome::Warning(exit_warning(code, &stderr)),
                    )
                    .await;
                }
                RunResult::Failed(why) => {
                    self.report(
                        events,
                        HookEvent::PreToolUse,
                        hook,
                        HookOutcome::Warning(why),
                    )
                    .await;
                }
            }
        }
        PreToolUse::Continue(updated)
    }

    /// `post_tool_use` with the tool result in the payload: `tool_output`
    /// (capped to [`TOOL_OUTPUT_CAP_BYTES`]) and `is_error`, so hooks can
    /// react to what the tool actually did. Returns extra context to append
    /// to the tool result, if any matching hook printed some.
    pub async fn post_tool_use_with_output(
        &self,
        tool: &str,
        args: &Value,
        output: &str,
        is_error: bool,
        mode: Mode,
        events: Option<&mpsc::Sender<AgentEvent>>,
    ) -> Option<String> {
        let extra = json!({
            "tool_output": cap_tool_output(output),
            "is_error": is_error,
        });
        match self
            .fire(
                HookEvent::PostToolUse,
                Some((tool, args)),
                Some(extra),
                mode,
                false,
                true,
                events,
            )
            .await
        {
            FireResult::Continue(extra) => extra,
            // Unreachable: post_tool_use is not blockable.
            FireResult::Block(_) => None,
        }
    }

    /// `user_prompt_submit` with the user message in the payload as `prompt`,
    /// so hooks can block or annotate a turn based on what was asked. May
    /// veto the turn or add context to the message.
    pub async fn user_prompt_submit_with_prompt(
        &self,
        prompt: &str,
        mode: Mode,
        events: Option<&mpsc::Sender<AgentEvent>>,
    ) -> PromptSubmit {
        let extra = json!({ "prompt": prompt });
        match self
            .fire(
                HookEvent::UserPromptSubmit,
                None,
                Some(extra),
                mode,
                true,
                true,
                events,
            )
            .await
        {
            FireResult::Continue(extra) => PromptSubmit::Continue(extra),
            FireResult::Block(reason) => PromptSubmit::Block(reason),
        }
    }

    /// `session_start`: extra system context for the session, if any matching
    /// hook printed some.
    pub async fn session_start(
        &self,
        mode: Mode,
        events: Option<&mpsc::Sender<AgentEvent>>,
    ) -> Option<String> {
        match self
            .fire(
                HookEvent::SessionStart,
                None,
                None,
                mode,
                false,
                true,
                events,
            )
            .await
        {
            FireResult::Continue(extra) => extra,
            FireResult::Block(_) => None,
        }
    }

    /// `session_end`: observational; output is ignored.
    pub async fn session_end(&self, mode: Mode, events: Option<&mpsc::Sender<AgentEvent>>) {
        self.fire(
            HookEvent::SessionEnd,
            None,
            None,
            mode,
            false,
            false,
            events,
        )
        .await;
    }

    /// `turn_end`: observational; output is ignored.
    pub async fn turn_end(&self, mode: Mode, events: Option<&mpsc::Sender<AgentEvent>>) {
        self.fire(HookEvent::TurnEnd, None, None, mode, false, false, events)
            .await;
    }

    /// Run the matching hooks for `event` sequentially. `payload_extra`
    /// carries event-specific payload fields; `blockable` honors exit 2;
    /// `capture` collects non-empty stdout as extra context.
    #[allow(clippy::too_many_arguments)]
    async fn fire(
        &self,
        event: HookEvent,
        tool: Option<(&str, &Value)>,
        payload_extra: Option<Value>,
        mode: Mode,
        blockable: bool,
        capture: bool,
        events: Option<&mpsc::Sender<AgentEvent>>,
    ) -> FireResult {
        let mut extra: Vec<String> = Vec::new();
        for hook in self.matching(event, tool.map(|(name, _)| name)) {
            match self
                .run(hook, event, tool, payload_extra.as_ref(), mode)
                .await
            {
                RunResult::Exited {
                    code: 0, stdout, ..
                } => {
                    let stdout = stdout.trim();
                    if capture && !stdout.is_empty() {
                        extra.push(stdout.to_string());
                        self.report(events, event, hook, HookOutcome::AppendedContext)
                            .await;
                    }
                }
                RunResult::Exited {
                    code: 2, stderr, ..
                } if blockable => {
                    let reason = block_reason(&stderr);
                    self.report(events, event, hook, HookOutcome::Blocked(reason.clone()))
                        .await;
                    return FireResult::Block(reason);
                }
                RunResult::Exited { code, stderr, .. } => {
                    self.report(
                        events,
                        event,
                        hook,
                        HookOutcome::Warning(exit_warning(code, &stderr)),
                    )
                    .await;
                }
                RunResult::Failed(why) => {
                    self.report(events, event, hook, HookOutcome::Warning(why))
                        .await;
                }
            }
        }
        FireResult::Continue((!extra.is_empty()).then(|| extra.join("\n")))
    }

    /// The configured hooks for `event`, filtered by matcher when the event
    /// carries a tool name.
    fn matching(
        &self,
        event: HookEvent,
        tool: Option<&str>,
    ) -> impl Iterator<Item = &CompiledHook> {
        self.hooks.iter().filter(move |hook| {
            hook.def.event == event
                && match (&hook.matcher, tool) {
                    (Some(matcher), Some(name)) => matcher.is_match(name),
                    // Matchers are only meaningful for tool events.
                    _ => true,
                }
        })
    }

    /// Run one hook to completion: payload on stdin, stdout/stderr captured,
    /// timeout enforced (dropping the wait kills the child via
    /// `kill_on_drop`). `payload_extra` fields are merged into the payload.
    async fn run(
        &self,
        hook: &CompiledHook,
        event: HookEvent,
        tool: Option<(&str, &Value)>,
        payload_extra: Option<&Value>,
        mode: Mode,
    ) -> RunResult {
        let session_id = self.session_id.lock().unwrap().clone();
        let mut payload = json!({
            "event": event.name(),
            "tool_name": tool.map(|(name, _)| name),
            "args": tool.map(|(_, args)| args),
            "cwd": self.cwd,
            "session_id": session_id,
            "mode": mode.to_string(),
        });
        if let Some(Value::Object(fields)) = payload_extra {
            payload
                .as_object_mut()
                .expect("payload is an object")
                .extend(fields.clone());
        }

        let mut child = match tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&hook.def.command)
            .current_dir(&self.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(child) => child,
            Err(err) => return RunResult::Failed(format!("spawn failed: {err}")),
        };

        if let Some(mut stdin) = child.stdin.take() {
            // A hook that never reads stdin may close the pipe early; that is
            // its prerogative, not an error.
            let _ = stdin.write_all(payload.to_string().as_bytes()).await;
        }

        let timeout = Duration::from_secs(hook.def.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));
        match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(Ok(output)) => RunResult::Exited {
                code: output.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            },
            Ok(Err(err)) => RunResult::Failed(format!("wait failed: {err}")),
            Err(_) => RunResult::Failed(format!("timed out after {}s", timeout.as_secs())),
        }
    }

    /// Surface one hook's outcome. Warnings also land in the log; callers
    /// skip this for plain successes, so default behavior is unchanged.
    async fn report(
        &self,
        events: Option<&mpsc::Sender<AgentEvent>>,
        event: HookEvent,
        hook: &CompiledHook,
        outcome: HookOutcome,
    ) {
        if let HookOutcome::Warning(why) = &outcome {
            tracing::warn!("hook {} ({}): {why}", event.name(), hook.def.command);
        }
        if let Some(events) = events {
            let _ = events
                .send(AgentEvent::HookFired {
                    event: event.name(),
                    command: hook.def.command.clone(),
                    outcome,
                })
                .await;
        }
    }

    /// Commands of the hooks that would fire for `event`/`tool` (tests).
    #[cfg(test)]
    fn matching_commands(&self, event: HookEvent, tool: Option<&str>) -> Vec<&str> {
        self.matching(event, tool)
            .map(|hook| hook.def.command.as_str())
            .collect()
    }
}

/// Parse a `pre_tool_use` hook's stdout as `{"updated_args": {...}}`.
/// Anything else (empty, non-JSON, missing key, non-object value) means
/// "leave the arguments alone".
fn parse_updated_args(stdout: &str) -> Option<Value> {
    let value: Value = serde_json::from_str(stdout.trim()).ok()?;
    let updated = value.get("updated_args")?;
    updated.is_object().then(|| updated.clone())
}

/// Reason for a block: trimmed stderr, with a fallback for silent hooks.
fn block_reason(stderr: &str) -> String {
    let trimmed = stderr.trim();
    if trimmed.is_empty() {
        "hook gave no reason".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Warning text for an unexpected exit code.
fn exit_warning(code: i32, stderr: &str) -> String {
    let trimmed = stderr.trim();
    if trimmed.is_empty() {
        format!("exit {code}")
    } else {
        format!("exit {code}: {trimmed}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(event: HookEvent, matcher: Option<&str>, command: &str) -> HookDef {
        HookDef {
            event,
            matcher: matcher.map(str::to_string),
            command: command.to_string(),
            timeout_secs: None,
        }
    }

    fn engine(defs: Vec<HookDef>) -> HookEngine {
        HookEngine::new(defs, std::env::temp_dir(), "test-session".to_string())
    }

    #[test]
    fn parse_full_entry() {
        let hooks = parse(
            r#"
            [[hooks]]
            event = "pre_tool_use"
            matcher = "execute"
            command = "/path/to/script.sh"
            timeout_secs = 30
            "#,
        )
        .expect("valid hooks.toml");
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].event, HookEvent::PreToolUse);
        assert_eq!(hooks[0].matcher.as_deref(), Some("execute"));
        assert_eq!(hooks[0].command, "/path/to/script.sh");
        assert_eq!(hooks[0].timeout_secs, Some(30));
    }

    #[test]
    fn parse_defaults_matcher_and_timeout() {
        let hooks =
            parse("[[hooks]]\nevent = \"turn_end\"\ncommand = \"date\"").expect("valid hooks.toml");
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].event, HookEvent::TurnEnd);
        assert!(hooks[0].matcher.is_none());
        assert!(hooks[0].timeout_secs.is_none());
    }

    #[test]
    fn parse_every_event_name() {
        for (name, event) in [
            ("pre_tool_use", HookEvent::PreToolUse),
            ("post_tool_use", HookEvent::PostToolUse),
            ("user_prompt_submit", HookEvent::UserPromptSubmit),
            ("session_start", HookEvent::SessionStart),
            ("session_end", HookEvent::SessionEnd),
            ("turn_end", HookEvent::TurnEnd),
        ] {
            let raw = format!("[[hooks]]\nevent = \"{name}\"\ncommand = \"true\"");
            let hooks = parse(&raw).expect("valid event name");
            assert_eq!(hooks[0].event, event);
            assert_eq!(event.name(), name, "name() round-trips the serde name");
        }
    }

    #[test]
    fn parse_rejects_unknown_event() {
        assert!(parse("[[hooks]]\nevent = \"on_boot\"\ncommand = \"true\"").is_err());
    }

    #[test]
    fn parse_empty_and_missing_hooks_key() {
        assert!(parse("").expect("empty file parses").is_empty());
        assert!(parse("# just a comment\n").expect("parses").is_empty());
    }

    #[test]
    fn load_files_appends_project_after_global_and_skips_bad_files() {
        let dir = std::env::temp_dir().join(format!("wizard-hooks-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let global = dir.join("global.toml");
        let project = dir.join("project.toml");
        let broken = dir.join("broken.toml");
        let missing = dir.join("missing.toml");
        std::fs::write(
            &global,
            "[[hooks]]\nevent = \"turn_end\"\ncommand = \"global\"",
        )
        .unwrap();
        std::fs::write(
            &project,
            "[[hooks]]\nevent = \"turn_end\"\ncommand = \"project\"",
        )
        .unwrap();
        std::fs::write(&broken, "this is not toml [[[").unwrap();

        let defs = load_files(&[global, broken, missing, project]);
        let commands: Vec<&str> = defs.iter().map(|d| d.command.as_str()).collect();
        assert_eq!(commands, vec!["global", "project"], "order preserved");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn matcher_glob_filters_tool_events() {
        let engine = engine(vec![
            def(HookEvent::PreToolUse, Some("execute"), "exact"),
            def(HookEvent::PreToolUse, Some("git_*"), "glob"),
            def(HookEvent::PreToolUse, None, "all"),
            def(HookEvent::PostToolUse, Some("execute"), "post"),
        ]);
        assert_eq!(
            engine.matching_commands(HookEvent::PreToolUse, Some("execute")),
            vec!["exact", "all"]
        );
        assert_eq!(
            engine.matching_commands(HookEvent::PreToolUse, Some("git_status")),
            vec!["glob", "all"]
        );
        assert_eq!(
            engine.matching_commands(HookEvent::PreToolUse, Some("read_file")),
            vec!["all"]
        );
        assert_eq!(
            engine.matching_commands(HookEvent::PostToolUse, Some("execute")),
            vec!["post"]
        );
    }

    #[test]
    fn matcher_is_ignored_for_non_tool_events() {
        // A matcher on a non-tool event is meaningless and matches anyway.
        let engine = engine(vec![def(HookEvent::SessionStart, Some("execute"), "start")]);
        assert_eq!(
            engine.matching_commands(HookEvent::SessionStart, None),
            vec!["start"]
        );
    }

    #[test]
    fn invalid_matcher_glob_drops_the_hook() {
        let engine = engine(vec![
            def(HookEvent::PreToolUse, Some("a{"), "bad"),
            def(HookEvent::PreToolUse, None, "good"),
        ]);
        assert_eq!(
            engine.matching_commands(HookEvent::PreToolUse, Some("execute")),
            vec!["good"]
        );
    }

    #[test]
    fn updated_args_requires_a_json_object_under_the_key() {
        assert_eq!(
            parse_updated_args(r#"{"updated_args": {"path": "b.rs"}}"#),
            Some(json!({"path": "b.rs"}))
        );
        assert!(parse_updated_args("").is_none());
        assert!(parse_updated_args("all clear").is_none());
        assert!(parse_updated_args(r#"{"other": 1}"#).is_none());
        assert!(parse_updated_args(r#"{"updated_args": "nope"}"#).is_none());
    }

    #[test]
    fn block_reason_falls_back_when_stderr_is_empty() {
        assert_eq!(block_reason("  denied \n"), "denied");
        assert_eq!(block_reason(""), "hook gave no reason");
    }

    #[test]
    fn cap_tool_output_truncates_on_char_boundaries() {
        assert_eq!(cap_tool_output("short"), "short");
        let long = "é".repeat(TOOL_OUTPUT_CAP_BYTES); // 2 bytes per char
        let capped = cap_tool_output(&long);
        assert!(capped.len() <= TOOL_OUTPUT_CAP_BYTES + 32);
        assert!(capped.ends_with("[tool output truncated]"));
    }

    // A `cat` hook echoes the stdin payload back as appended context, so
    // these tests observe exactly what a hook receives.

    #[tokio::test]
    async fn user_prompt_submit_with_prompt_puts_the_prompt_in_the_payload() {
        let engine = engine(vec![def(HookEvent::UserPromptSubmit, None, "cat")]);
        match engine
            .user_prompt_submit_with_prompt("deploy to prod", Mode::default(), None)
            .await
        {
            PromptSubmit::Continue(Some(payload)) => {
                assert!(
                    payload.contains(r#""event":"user_prompt_submit""#),
                    "{payload}"
                );
                assert!(
                    payload.contains(r#""prompt":"deploy to prod""#),
                    "{payload}"
                );
            }
            other => panic!("expected appended context, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn post_tool_use_with_output_puts_the_result_in_the_payload() {
        let engine = engine(vec![def(HookEvent::PostToolUse, None, "cat")]);
        let payload = engine
            .post_tool_use_with_output(
                "execute",
                &json!({ "command": "true" }),
                "exit code: 3",
                true,
                Mode::default(),
                None,
            )
            .await
            .expect("cat echoes the payload");
        assert!(
            payload.contains(r#""tool_output":"exit code: 3""#),
            "{payload}"
        );
        assert!(payload.contains(r#""is_error":true"#), "{payload}");
        assert!(payload.contains(r#""tool_name":"execute""#), "{payload}");
    }
}
