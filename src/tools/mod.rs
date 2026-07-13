//! Tool system: the [`Tool`] trait implemented by native tools
//! ([`file`], [`shell`], [`git`]), agent-authored [`scripted`] tools, and
//! MCP tools (`crate::mcp`). All three present a uniform interface through
//! [`registry::ToolRegistry`], so the model calls them identically.

pub mod command;
pub mod evolve;
pub mod file;
pub mod git;
pub mod interview;
pub mod memory;
pub mod plan;
pub mod publish;
pub mod registry;
pub mod scripted;
pub mod shell;
pub mod subagent_tasks;
pub mod tasks;
pub mod todo;
pub mod web;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::llm::{Image, ToolSpec};

/// Where a tool comes from. Affects display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    /// Compiled into the binary.
    Native,
    /// Agent-authored script under `~/.wizard/tools/`.
    Scripted,
    /// Served by an external MCP server.
    Mcp,
}

/// How a tool touches the world. Drives the plan-mode read-only gate and
/// checkpoint snapshots of `Edit`-class targets — never prompting.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ToolAccess {
    /// Observes only (reads files, queries state).
    ReadOnly,
    /// Modifies a file at a resolvable path.
    Edit,
    /// Runs commands or has other side effects.
    Execute,
}

/// Per-call execution context.
#[derive(Debug, Clone)]
pub struct ToolContext {
    /// Project root all relative paths resolve against.
    pub cwd: PathBuf,
    /// Session-wide registry of background shell tasks.
    pub tasks: Arc<tasks::TaskRegistry>,
    /// Session-wide registry of background subagent runs (`spawn_subagent`
    /// with `background: true`).
    pub subagents: Arc<subagent_tasks::SubagentTaskRegistry>,
    /// The agent's working todo list, shared by every call in the session.
    pub todos: Arc<Mutex<todo::TodoList>>,
    /// Event channel of the turn currently dispatching, injected by the
    /// dispatcher so tools that converse with the surface (`exit_plan`'s
    /// approval round-trip) can reach it. `None` outside the dispatch
    /// pipeline (subagents, direct registry execution).
    pub events: Option<tokio::sync::mpsc::Sender<crate::agent::AgentEvent>>,
    /// Settings for the native web tools (`[web]` in `config.toml`), set by
    /// the agent at construction; defaults elsewhere.
    pub web: Arc<crate::config::WebConfig>,
    /// Per-file checkpoint store, set by the agent at construction. The
    /// dispatcher and the subagent loop snapshot `Edit`-class targets into
    /// it before execution. `None` outside an agent (direct registry
    /// execution in tests).
    pub checkpoints: Option<Arc<crate::checkpoint::CheckpointStore>>,
    /// Where images produced during this session are written, set by the agent
    /// at construction (`~/.wizard/images/<session>/`). The agent loop and the
    /// subagent loop persist through it before announcing an image to the
    /// surfaces. `None` outside an agent (direct registry execution in tests),
    /// in which case images still reach the model but land nowhere on disk.
    pub images: Option<Arc<crate::images::ImageStore>>,
    /// True only on the interactive TUI surface, which drains and dispatches
    /// slash commands the agent queues via `run_command`. A live `events`
    /// channel alone does not imply this — headless and gateway runs stream
    /// events to a printer that cannot apply a command — so the `run_command`
    /// tool gates on this flag to avoid reporting success for work that would
    /// never run. Set by the TUI's agent builder; false everywhere else.
    pub dispatches_commands: bool,
}

impl ToolContext {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            tasks: Arc::new(tasks::TaskRegistry::new()),
            subagents: Arc::new(subagent_tasks::SubagentTaskRegistry::new()),
            todos: Arc::new(Mutex::new(todo::TodoList::new())),
            events: None,
            web: Arc::new(crate::config::WebConfig::default()),
            checkpoints: None,
            images: None,
            dispatches_commands: false,
        }
    }

    /// Mark this context as belonging to a surface that drains queued slash
    /// commands (the interactive TUI). Enables the `run_command` tool.
    pub fn with_command_dispatch(mut self, on: bool) -> Self {
        self.dispatches_commands = on;
        self
    }

    /// This context with `web` tool settings applied (agent construction).
    pub fn with_web(mut self, web: crate::config::WebConfig) -> Self {
        self.web = Arc::new(web);
        self
    }

    /// This context with the checkpoint store attached (agent construction).
    pub fn with_checkpoints(mut self, store: Arc<crate::checkpoint::CheckpointStore>) -> Self {
        self.checkpoints = Some(store);
        self
    }

    /// This context with the session's image store attached (agent
    /// construction).
    pub fn with_images(mut self, store: Arc<crate::images::ImageStore>) -> Self {
        self.images = Some(store);
        self
    }

    /// A copy of this context carrying the turn's event channel.
    pub fn with_events(&self, events: tokio::sync::mpsc::Sender<crate::agent::AgentEvent>) -> Self {
        Self {
            events: Some(events),
            ..self.clone()
        }
    }
}

/// Result of a tool execution, fed back to the model as a `role: tool`
/// message.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    /// Text returned to the model (stdout, file contents, diff, ...).
    pub content: String,
    /// True when the tool ran but reported failure (non-zero exit, missing
    /// file, ...). Distinct from [`ToolError`], which means the call itself
    /// could not be carried out.
    pub is_error: bool,
    /// Images the tool produced (a generated image, a screenshot, a rendered
    /// chart). Build them with [`Image::from_bytes`](crate::llm::Image::from_bytes),
    /// which sniffs the media type and enforces the size cap.
    ///
    /// The agent loop takes them from here: it writes them to the session's
    /// image directory, announces them to the surfaces
    /// ([`AgentEvent::Images`](crate::agent::AgentEvent::Images)), and feeds
    /// them back to the model on a following user message — a `tool`-role
    /// message cannot carry image blocks on OpenAI, but a user message can
    /// everywhere (see [`ChatMessage::user_with_images`]).
    pub images: Vec<Image>,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            images: Vec::new(),
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            images: Vec::new(),
        }
    }

    /// Successful output carrying one or more images alongside its text. The
    /// text is what the model reads; the images are what it sees.
    pub fn ok_with_images(content: impl Into<String>, images: Vec<Image>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            images,
        }
    }
}

/// Failures in dispatching or running a tool call.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("unknown tool: {0}")]
    UnknownTool(String),
    #[error("invalid arguments for '{tool}': {message}")]
    InvalidArgs { tool: String, message: String },
    #[error("tool '{tool}' timed out after {seconds}s")]
    Timeout { tool: String, seconds: u64 },
    #[error("tool '{tool}' failed")]
    Execution {
        tool: String,
        #[source]
        source: anyhow::Error,
    },
}

/// Byte cap applied to tool output returned to the model. Keeps a single
/// tool result from flooding the context window.
pub(crate) const MAX_OUTPUT_BYTES: usize = 30_000;

/// Resolve a model-supplied path against the project root, expanding a
/// leading `~`. Absolute paths are used as-is.
pub(crate) fn resolve_path(ctx: &ToolContext, path: &str) -> PathBuf {
    let expanded = shellexpand::tilde(path);
    let candidate = std::path::Path::new(expanded.as_ref());
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        ctx.cwd.join(candidate)
    }
}

/// Deserialize tool arguments, mapping shape mismatches to
/// [`ToolError::InvalidArgs`]. `null` is treated as an empty object so models
/// may omit arguments for zero-parameter tools.
pub(crate) fn parse_args<T: serde::de::DeserializeOwned>(
    tool: &str,
    args: serde_json::Value,
) -> Result<T, ToolError> {
    let args = if args.is_null() {
        serde_json::Value::Object(serde_json::Map::new())
    } else {
        args
    };
    serde_json::from_value(args).map_err(|err| ToolError::InvalidArgs {
        tool: tool.to_string(),
        message: err.to_string(),
    })
}

/// Bytes reserved for the truncation marker inside the `max_bytes` budget.
const TRUNCATION_MARKER_RESERVE: usize = 192;

/// Truncate `text` to at most `max_bytes` (cutting on char boundaries),
/// keeping the head and a larger tail — build and test failures land at the
/// end of output — around a marker that says how much was omitted.
pub(crate) fn truncate_output(mut text: String, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text;
    }
    // Budgets too small for head+tail framing fall back to a plain head cut.
    if max_bytes <= TRUNCATION_MARKER_RESERVE {
        let mut cut = max_bytes;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n... [output truncated]");
        return text;
    }
    let budget = max_bytes - TRUNCATION_MARKER_RESERVE;
    let mut head_end = budget / 4;
    while head_end > 0 && !text.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = text.len() - (budget - budget / 4);
    while tail_start < text.len() && !text.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let omitted = tail_start - head_end;
    format!(
        "{}\n... [output truncated] {omitted} bytes omitted from the middle; rerun a narrower \
         command for the full output, or task_output for a background task ...\n{}",
        &text[..head_end],
        &text[tail_start..]
    )
}

/// A callable capability exposed to the model.
///
/// Contract:
/// - `name` is unique within the registry (MCP tools are namespaced
///   `server__tool` on collision).
/// - `parameters` returns a JSON Schema object; `execute` receives arguments
///   already validated against nothing — implementations must deserialize
///   defensively and return [`ToolError::InvalidArgs`] on shape mismatch.
/// - `access` classifies side effects conservatively: anything not provably
///   read-only or a path-addressed edit stays [`ToolAccess::Execute`].
#[async_trait]
pub trait Tool: Send + Sync {
    /// Unique tool name as advertised to the model (snake_case).
    fn name(&self) -> &str;

    /// One-paragraph description shown to the model.
    fn description(&self) -> &str;

    /// JSON Schema describing the arguments object.
    fn parameters(&self) -> serde_json::Value;

    /// How this tool touches the world. Drives the plan-mode read-only gate
    /// and checkpoint snapshots — never prompting.
    fn access(&self) -> ToolAccess {
        ToolAccess::Execute
    }

    /// Origin of this tool.
    fn kind(&self) -> ToolKind {
        ToolKind::Native
    }

    /// Wire-format spec sent to Ollama in the request `tools` array.
    fn spec(&self) -> ToolSpec {
        ToolSpec::function(self.name(), self.description(), self.parameters())
    }

    /// Run the tool with `args` (a JSON object) in `ctx`.
    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput, ToolError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_leaves_short_text_alone() {
        assert_eq!(truncate_output("short".to_string(), 1_000), "short");
    }

    #[test]
    fn truncate_keeps_head_and_tail_and_counts_omitted_bytes() {
        let text = format!("HEAD{}TAIL", "x".repeat(10_000));
        let out = truncate_output(text, 1_000);
        assert!(
            out.len() <= 1_000,
            "stays within budget: {} bytes",
            out.len()
        );
        assert!(out.starts_with("HEAD"), "head preserved");
        assert!(out.ends_with("TAIL"), "tail preserved");
        assert!(out.contains("[output truncated]"));
        assert!(out.contains("bytes omitted"), "{out}");
    }

    #[test]
    fn truncate_tail_is_larger_than_head() {
        let text = "h".repeat(500) + &"t".repeat(10_000);
        let out = truncate_output(text, 1_000);
        let heads = out.chars().filter(|&c| c == 'h').count();
        let tails = out.chars().filter(|&c| c == 't').count();
        assert!(tails > heads, "tail-weighted: {heads} head vs {tails} tail");
    }

    #[test]
    fn truncate_cuts_on_char_boundaries() {
        let text = "é".repeat(20_000);
        let out = truncate_output(text, 1_001);
        assert!(out.len() <= 1_001);
        assert!(out.contains("[output truncated]"));
    }

    #[test]
    fn truncate_tiny_budget_falls_back_to_head_cut() {
        let text = "x".repeat(500);
        let out = truncate_output(text, 100);
        assert!(out.starts_with("xxx"));
        assert!(out.ends_with("[output truncated]"));
    }
}
