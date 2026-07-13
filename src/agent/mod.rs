//! Agent loop: build messages → stream completion → parse tool calls →
//! execute tools → repeat until done or `max_steps`.
//!
//! The loop is UI-agnostic: it emits [`AgentEvent`]s over a channel that the
//! Ratatui TUI (genie) or the headless runner (sovereign) consumes.

pub mod mission;
pub mod prompts;
pub mod session;
pub mod subagent;
pub mod ultra;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::cli::Cli;
use crate::config::{Config, Mode, ProviderKind};
use crate::dispatch::Dispatcher;
use crate::hooks::{HookEngine, PromptSubmit};
use crate::llm::provider::LlmProvider;
use crate::llm::{ChatMessage, ChatOptions, ChatRequest, FunctionCall, Role, ToolCall};
use crate::mcp::{McpConfig, McpManager};
use crate::skills::Skill;
use crate::tools::{ToolContext, ToolOutput, registry::ToolRegistry};

use session::Session;

/// Why an agent turn (or sovereign run) ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoneReason {
    /// Model finished without requesting more tools.
    Completed,
    /// Step budget exhausted.
    MaxSteps,
    /// `--max-hours` elapsed (sovereign).
    TimeLimit,
    /// Stopped via the loop-control file or user interrupt.
    Stopped,
    /// Circuit breaker: repeated identical failures (sovereign) or too many
    /// consecutive failures of one tool.
    CircuitBreaker,
}

/// Verdict on a plan presented via the `exit_plan` tool (plan mode).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanVerdict {
    /// True when the plan was approved and execution may proceed.
    pub approved: bool,
    /// Reviewer feedback on rejection (empty = a generic rejection).
    pub feedback: String,
}

impl PlanVerdict {
    /// Approve the plan: plan mode ends and the model executes it.
    pub fn approve() -> Self {
        Self {
            approved: true,
            feedback: String::new(),
        }
    }

    /// Reject the plan with `feedback`; plan mode stays on.
    pub fn reject(feedback: impl Into<String>) -> Self {
        Self {
            approved: false,
            feedback: feedback.into(),
        }
    }
}

/// One clarifying question asked via the `interview` tool (plan mode). The
/// surface collects an answer for each; an empty option list means a
/// free-text answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterviewQuestion {
    /// The question text shown to the user.
    pub question: String,
    /// Suggested answers the user can pick from; empty for free-text only.
    /// The user may always type their own answer instead of picking.
    pub options: Vec<String>,
}

/// Events emitted by the agent loop. The TUI renders them; the headless
/// runner logs them.
#[derive(Debug)]
pub enum AgentEvent {
    /// Streaming assistant text delta.
    TextDelta(String),
    /// Streaming model reasoning ("thinking") delta. Rendered dimmed by the
    /// TUI; never part of the assistant message or the session history.
    ThinkingDelta(String),
    /// A tool call is being executed.
    ToolStarted { name: String, args: Value },
    /// A tool call finished.
    ToolFinished { name: String, output: ToolOutput },
    /// One agent step (model round-trip) completed. 1-based.
    StepCompleted { step: u32 },
    /// Non-fatal error surfaced to the user; the loop may continue.
    Error(String),
    /// Informational progress notice (e.g. history compaction); never an
    /// error.
    Notice(String),
    /// A completion stream died mid-response and is about to be retried from
    /// scratch. Whatever partial text was streamed so far never entered
    /// history and will be re-generated — consumers rendering deltas must
    /// discard their partial buffer or the retry duplicates it.
    StreamRetrying,
    /// A lifecycle hook did something worth surfacing (rewrote arguments,
    /// appended context, blocked, or failed). Plain successes are silent.
    /// Rendered as a dim log line.
    HookFired {
        /// Lifecycle event name (e.g. `"pre_tool_use"`).
        event: &'static str,
        /// The hook's shell command.
        command: String,
        /// What the hook did.
        outcome: crate::hooks::HookOutcome,
    },
    /// Plan mode: the model presented a plan via `exit_plan` and the turn is
    /// paused awaiting a verdict. The consumer must send exactly one
    /// [`PlanVerdict`] on `respond` (the TUI renders a review; headless and
    /// gateway auto-approve). Dropping the sender counts as no verdict and
    /// keeps plan mode on.
    PlanReady {
        /// The plan markdown (also persisted to `.wizard/plan.md`).
        plan: String,
        respond: tokio::sync::oneshot::Sender<PlanVerdict>,
    },
    /// Plan mode: the model asked clarifying questions via the `interview`
    /// tool and the turn is paused awaiting answers. The consumer must send
    /// exactly one response on `respond`: `Some(answers)` aligned with
    /// `questions` (empty string = the user skipped that one), or `None` to
    /// decline the interview entirely (no interactive user, or the user
    /// dismissed it). Dropping the sender counts as `None`. Read-only, so it
    /// is allowed mid-plan.
    Interview {
        /// The questions to put to the user, in order.
        questions: Vec<InterviewQuestion>,
        respond: tokio::sync::oneshot::Sender<Option<Vec<String>>>,
    },
    /// Omakase (chef's-choice) mode: the model finished planning and, because
    /// there is no human review gate, is proceeding to execute. Informational
    /// only — the plan markdown for the surface to display. The plan is also
    /// persisted to `.wizard/plan.md`.
    OmakaseProceeding {
        /// The plan markdown the chef chose.
        plan: String,
    },
    /// Token usage of one completed model call, when the backend reported
    /// counts. Surfaces accumulate these (status bar, headless summary).
    /// Emitted for the parent's own calls and for every subagent call made
    /// under it (`spawn_subagent`, `/ultra`'s candidates and judges), so the
    /// counter reflects what the turn actually spent.
    Usage {
        prompt_tokens: u64,
        completion_tokens: u64,
    },
    /// The `/ultra` pre-phase produced its guidance: `label` is the roster that
    /// ran (`"ultra ×3 · implementer+skeptic+minimalist · 1 judge"`), `guidance`
    /// the candidate drafts and the judge's verdict exactly as they were
    /// injected into the turn.
    ///
    /// The TUI folds it into a collapsed transcript card, which is the durable
    /// record of a fan-out the user paid several× a normal turn for: the
    /// candidates' panes retire off the rail within seconds of finishing, while
    /// the main agent is still working. Surfaces that only print the turn's
    /// answer ignore it — the drafts are advice, not the answer.
    UltraGuidance { label: String, guidance: String },
    /// The todo list was replaced via the `todo` tool. Carries the full new
    /// list; the TUI mirrors it in a side panel, headless prints a one-line
    /// summary, the gateway ignores it.
    TodoUpdated(Vec<crate::tools::todo::TodoItem>),
    /// A background task (`execute` with `run_in_background`) was just
    /// spawned. The TUI mirrors it into the dashboard's task list; other
    /// surfaces ignore it.
    TaskStarted { id: u32, command: String },
    /// A background task (`execute` with `run_in_background`) finished; its
    /// output tail was injected into history. The TUI and headless surfaces
    /// print a one-liner, the gateway ignores it.
    TaskFinished {
        id: u32,
        command: String,
        status: crate::tools::tasks::TaskStatus,
    },
    /// `spawn_subagent` was called with `background: true` and just detached.
    /// The TUI mirrors it into the dashboard's subagent list; other surfaces
    /// ignore it.
    SubagentStarted { id: u32, name: String, task: String },
    /// A backgrounded subagent finished; its report was injected into
    /// history. The TUI and headless surfaces print a one-liner, the gateway
    /// ignores it.
    SubagentFinished {
        id: u32,
        name: String,
        task: String,
        completed: bool,
        output: String,
    },
    /// A subagent run started, foreground or background. `run` scopes every
    /// later `SubagentRun*` event below to this run, so a surface can demux
    /// concurrent runs of the same subagent into separate panes. `bg` carries
    /// the background-registry id when the run was detached, so the surface
    /// can kill it.
    SubagentRunStarted {
        run: u64,
        bg: Option<u32>,
        name: String,
        task: String,
    },
    /// A subagent produced assistant text (its own message, between tool
    /// calls). Scoped to a run.
    SubagentRunText { run: u64, text: String },
    /// A subagent started a tool call. Scoped to a run; the tool name is bare
    /// (the pane supplies the subagent's name).
    SubagentRunToolStarted { run: u64, name: String, args: Value },
    /// A subagent's tool call finished. Scoped to a run.
    SubagentRunToolFinished {
        run: u64,
        name: String,
        output: ToolOutput,
    },
    /// A subagent completed one step (model round-trip). 1-based, scoped to a
    /// run.
    SubagentRunStep { run: u64, step: u32 },
    /// A subagent run ended. Scoped to a run. `error` is set when it died on a
    /// hard error; `completed` is false when it hit its step budget.
    SubagentRunDone {
        run: u64,
        completed: bool,
        output: String,
        steps_used: u32,
        error: Option<String>,
    },
    /// The agent asked to run one of Wizard's own slash commands via the
    /// `run_command` tool. Carries the raw command line (e.g. `/effort high`).
    /// The interactive surface validates and dispatches it once the turn ends
    /// and the agent is back in its slot; other surfaces ignore it (there is
    /// no menu to drive).
    CommandRequested(String),
    /// The turn is over.
    Done { reason: DoneReason },
}

/// Sovereign-mode run control, read from `.wizard/loop-control` in the
/// project between steps (see `docs/modes.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopControl {
    /// Graceful shutdown after the current step.
    Stop,
    /// Wait until the file is removed or set to `resume`.
    Pause,
    /// Skip the current sub-task.
    Skip,
}

/// Read and parse `.wizard/loop-control` under `project_root`.
/// `None` when the file is absent, unreadable, or holds `resume`/unknown
/// content.
pub fn read_loop_control(project_root: &Path) -> Option<LoopControl> {
    let path = loop_control_path(project_root);
    let raw = std::fs::read_to_string(path).ok()?;
    match raw.trim().to_ascii_lowercase().as_str() {
        "stop" => Some(LoopControl::Stop),
        "pause" => Some(LoopControl::Pause),
        "skip" => Some(LoopControl::Skip),
        _ => None,
    }
}

fn loop_control_path(project_root: &Path) -> PathBuf {
    project_root.join(".wizard").join("loop-control")
}

/// Remove the loop-control file after consuming a one-shot command
/// (`stop`/`skip`), so it does not re-trigger on the next run.
fn clear_loop_control(project_root: &Path) {
    let path = loop_control_path(project_root);
    if let Err(err) = std::fs::remove_file(&path)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!("could not remove {}: {err}", path.display());
    }
}

/// Parse a prompt-protocol tool call (`{"tool": ..., "arguments": {...}}`)
/// out of assistant text, for models without native tool calling. Lenient:
/// accepts the whole message, a fenced ```json block, or any line that is a
/// JSON object with a `tool` field.
pub(crate) fn parse_json_tool_call(text: &str) -> Option<ToolCall> {
    #[derive(serde::Deserialize)]
    struct ProtocolCall {
        tool: String,
        #[serde(default)]
        arguments: Value,
    }

    fn try_parse(candidate: &str) -> Option<ToolCall> {
        let call: ProtocolCall = serde_json::from_str(candidate).ok()?;
        let arguments = if call.arguments.is_null() {
            json!({})
        } else {
            call.arguments
        };
        Some(ToolCall {
            function: FunctionCall {
                name: call.tool,
                arguments,
            },
        })
    }

    let trimmed = text.trim();
    if let Some(call) = try_parse(trimmed) {
        return Some(call);
    }
    // Fenced ```json block.
    if let Some(start) = trimmed.find("```") {
        let after = &trimmed[start + 3..];
        let after = after.strip_prefix("json").unwrap_or(after);
        if let Some(end) = after.find("```")
            && let Some(call) = try_parse(after[..end].trim())
        {
            return Some(call);
        }
    }
    // Any single line that is a JSON object.
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('{')
            && let Some(call) = try_parse(line)
        {
            return Some(call);
        }
    }
    None
}

/// Normalize model-provided tool arguments to a JSON object: `null` becomes
/// `{}`, and stringified JSON (some models double-encode) is parsed.
pub(crate) fn normalize_args(args: &Value) -> Value {
    match args {
        Value::Null => json!({}),
        Value::String(raw) => serde_json::from_str(raw).unwrap_or_else(|_| args.clone()),
        other => other.clone(),
    }
}

/// Send an event, reporting whether the receiver is still listening.
pub(crate) async fn emit(events: &mpsc::Sender<AgentEvent>, event: AgentEvent) -> bool {
    events.send(event).await.is_ok()
}

/// Whether an LLM error is worth retrying after backoff. Typed provider
/// errors classify themselves; unknown errors (mid-stream drops surface as
/// plain `anyhow` context chains) stay transient for robustness.
pub(crate) fn error_is_transient(err: &anyhow::Error) -> bool {
    if let Some(provider) = err.downcast_ref::<crate::llm::ProviderError>() {
        return provider.is_transient();
    }
    if let Some(ollama) = err.downcast_ref::<crate::llm::ollama::OllamaError>() {
        return ollama.is_transient();
    }
    true
}

/// Cooperative cancellation handle for a running turn. Cloneable and
/// thread-safe: the surface keeps a clone (see [`Agent::cancel_handle`]) and
/// calls [`CancelHandle::cancel`] (e.g. on Esc); the run loop observes it in
/// the stream loop and between tool calls, synthesizes results for the tool
/// calls it skips, and ends the turn with [`DoneReason::Stopped`] — without
/// the agent (or its background tasks) being torn down. The flag auto-resets
/// at the start of the next turn.
#[derive(Clone, Default)]
pub struct CancelHandle(Arc<CancelState>);

#[derive(Default)]
struct CancelState {
    flag: std::sync::atomic::AtomicBool,
    notify: tokio::sync::Notify,
}

impl CancelHandle {
    /// Request cancellation of the turn currently running (if any).
    pub fn cancel(&self) {
        self.0.flag.store(true, std::sync::atomic::Ordering::SeqCst);
        self.0.notify.notify_waiters();
    }

    /// Whether cancellation has been requested for the current turn.
    pub fn is_cancelled(&self) -> bool {
        self.0.flag.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Resolves once cancellation is requested (immediately if it already
    /// was).
    pub async fn cancelled(&self) {
        loop {
            let notified = self.0.notify.notified();
            tokio::pin!(notified);
            // Register interest before checking the flag so a concurrent
            // `cancel` cannot slip between the check and the await.
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }

    /// Arm for a new turn (a stale request must not cancel it).
    fn clear(&self) {
        self.0
            .flag
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// One streamed completion, or the turn's cancellation observed mid-stream.
enum Completion {
    Done {
        content: String,
        tool_calls: Vec<ToolCall>,
    },
    Cancelled,
}

/// A background completion surfaced to the model, returned by
/// [`Agent::drain_finished_notifications`] so surfaces can render it.
#[derive(Debug)]
pub enum FinishedNotification {
    /// A background shell task (`execute` with `run_in_background`).
    Task(crate::tools::tasks::FinishedTask),
    /// A backgrounded subagent (`spawn_subagent` with `background: true`).
    Subagent(crate::tools::subagent_tasks::SubagentTaskResult),
}

/// History/system note announcing a finished background task.
fn task_note(task: &crate::tools::tasks::FinishedTask) -> String {
    let mut note = format!(
        "[background task #{} finished ({})] {}",
        task.id,
        task.status.describe(),
        task.command
    );
    let tail = task.tail.trim();
    if !tail.is_empty() {
        note.push('\n');
        note.push_str(tail);
    }
    note
}

/// History/system note announcing a finished background subagent.
fn subagent_note(task: &crate::tools::subagent_tasks::SubagentTaskResult) -> String {
    let status = match &task.error {
        Some(error) => format!("failed: {error}"),
        None if task.completed => "completed".to_string(),
        None => "hit its step budget".to_string(),
    };
    format!(
        "[background subagent #{} '{}' {} after {} step(s)] {}\n\n{}",
        task.id, task.name, status, task.steps_used, task.task, task.output
    )
}

/// The tool-calling agent. Owns the conversation history, the model client,
/// the tool dispatcher, and session persistence.
pub struct Agent {
    client: Arc<dyn LlmProvider>,
    /// Active model tag (from `config.active().model`); switched by
    /// [`Agent::set_model`].
    model: String,
    /// Tool-call pipeline; owns the registry and the failure breakers.
    dispatcher: Dispatcher,
    /// Lifecycle hooks; the dispatcher and the subagent spawner share it.
    hooks: Arc<HookEngine>,
    config: Config,
    mode: Mode,
    /// Full conversation including the system prompt at index 0.
    history: Vec<ChatMessage>,
    session: Session,
    ctx: ToolContext,
    /// Whether the model supports native tool calling; when false the loop
    /// uses the prompt-based JSON tool protocol.
    native_tools: bool,
    /// Skills baked into the system prompt (kept for `/mode` rebuilds).
    skills: Vec<Skill>,
    /// Assembled instruction hierarchy (`WIZARD.md` / `AGENTS.md` /
    /// `CLAUDE.md` from the project root up, plus `~/.wizard/WIZARD.md` —
    /// see [`crate::instructions`]), if any file exists.
    agents_md: Option<String>,
    /// Persistent memory index (MEMORY.md) for this project, if any
    /// memories are saved. Re-read on every system prompt refresh so
    /// `/reload` picks up changes.
    memory_index: Option<String>,
    /// Wall-clock deadline for sovereign runs (`--max-hours`).
    deadline: Option<Instant>,
    /// Warning from session resume (corrupt/unreadable file), emitted on
    /// the next turn so the UI can surface it.
    load_warning: Option<String>,
    /// Plan-mode flag, shared with the dispatcher (read-only gate) and the
    /// `exit_plan` tool (cleared on approval).
    plan_mode: Arc<std::sync::atomic::AtomicBool>,
    /// Whether the plan-mode instruction block is currently baked into the
    /// system prompt; [`Agent::sync_plan_prompt`] refreshes on mismatch.
    plan_prompt_on: bool,
    /// Omakase (chef's-choice) flag, shared with the `exit_plan` and
    /// `interview` tools. While set, `exit_plan` auto-approves the plan and
    /// `interview` declines to ask — the agent decides and proceeds.
    /// Implies plan mode (the read-only exploration phase).
    omakase: Arc<std::sync::atomic::AtomicBool>,
    /// Whether the omakase instruction block is currently baked into the
    /// system prompt; refreshed on mismatch alongside the plan block.
    omakase_prompt_on: bool,
    /// Token counters fed from `ChatChunk` eval counts during streaming.
    /// Shared into the tool context (`ToolContext::usage`) so a subagent's
    /// model calls — `spawn_subagent`, and every `/ultra` candidate and judge —
    /// bill this agent instead of vanishing from the totals.
    usage: Arc<crate::usage::UsageTracker>,
    /// Where per-turn usage records are appended
    /// (`~/.wizard/usage.jsonl`); `None` disables the log.
    usage_log: Option<PathBuf>,
    /// Per-file checkpoint store (`.wizard/checkpoints/` in the project).
    /// Shared with the tool context so the dispatcher and subagents snapshot
    /// `Edit`-class targets into it; `/rewind` and perpetual rollback
    /// restore from it.
    checkpoints: Arc<crate::checkpoint::CheckpointStore>,
    /// Cooperative cancellation of the running turn (see
    /// [`Agent::cancel_handle`]). Cleared at the start of every turn.
    cancel: CancelHandle,
    /// The spawn tool's shared model slot, when bound
    /// ([`Agent::bind_subagent_model`]): `/model` switches write through so
    /// subagents run on the parent's active model.
    subagent_model: Option<subagent::SharedActiveModel>,
    /// The `/ultra` engine while mixture-of-agents mode is on: each turn first
    /// fans candidate subagents out on *this* client and model, has judges
    /// compare their drafts, and injects the verdict — then runs normally.
    /// `None` (the default, and what every non-TUI surface gets) is an ordinary
    /// turn.
    ///
    /// Session state, not config: a rebuilt agent (`/model`, a provider switch,
    /// `/resume`) starts without it, so every rebuild path must re-arm it.
    ultra: Option<Arc<ultra::UltraEngine>>,
}

/// One row of the `/rewind` picker: a turn, the prompt that started it, and
/// the files its tool calls snapshotted.
#[derive(Debug, Clone)]
pub struct RewindCandidate {
    pub turn: u64,
    /// First line of the turn's user prompt (empty when unknown).
    pub prompt: String,
    /// Files the turn snapshotted before editing.
    pub files: Vec<PathBuf>,
}

/// Number of most-recent messages preserved verbatim when compacting history.
const KEEP_RECENT: usize = 10;

/// Heading of the note [`Agent::compact_now`] leaves in place of the span it
/// summarized. Public because it is the only handle anything downstream has on
/// that note: it is a `Role::System` message like any other, and
/// [`ultra::render_context`] has to be able to tell "everything older than the
/// tail, summarized" apart from an ordinary injected note when it briefs a
/// candidate.
pub const COMPACT_SUMMARY_HEADING: &str = "[Compacted progress summary]";

/// Fraction of the provider's context window the last prompt may fill before
/// token-aware compaction kicks in.
const COMPACT_WINDOW_FRACTION: f64 = 0.8;

/// Chunk size (chars) fed to one rolling-summary pass during compaction.
const COMPACT_CHUNK_CHARS: usize = 20_000;

/// What a compaction pass did, reported back so callers (auto-compaction
/// notices and the `/compact` command) can describe the outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactOutcome {
    /// Too little history between the system prompt and the recent tail.
    Nothing,
    /// Summarized `count` middle messages into one progress note.
    Summarized(usize),
    /// The summary LLM failed, so `count` middle messages were dropped.
    Truncated { count: usize, error: String },
}

impl CompactOutcome {
    /// One-line notice describing what the pass did, shared by the
    /// auto-compaction events and the `/compact` command.
    pub fn describe(&self) -> String {
        fn messages(count: usize) -> String {
            if count == 1 {
                "1 message".to_string()
            } else {
                format!("{count} messages")
            }
        }
        match self {
            CompactOutcome::Nothing => "nothing to compact yet".to_string(),
            CompactOutcome::Summarized(count) => {
                format!("compacted {} into a summary", messages(*count))
            }
            CompactOutcome::Truncated { count, error } => format!(
                "compacted {} by truncation (summary failed: {error})",
                messages(*count)
            ),
        }
    }
}

/// User-role nudge injected (in memory only) when a completion comes back
/// with no visible text and no tool calls.
const EMPTY_COMPLETION_NUDGE: &str = "(continue: reply to the user with your findings)";

/// True when a completed assistant message has neither visible content nor
/// tool calls (e.g. a reasoning model that thought and then just stopped).
pub(crate) fn completion_is_empty(content: &str, tool_calls: &[ToolCall]) -> bool {
    content.trim().is_empty() && tool_calls.is_empty()
}

impl Agent {
    /// Build an agent: compose the system prompt from `mode`, `skills`, and
    /// any project `AGENTS.md`; seed history from `session` (resumed
    /// sessions replay their persisted messages under a fresh system
    /// prompt). `hooks` is loaded by the builders (`crate::hooks::load`) and
    /// injected so tests can supply their own definitions.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        client: Arc<dyn LlmProvider>,
        registry: ToolRegistry,
        config: Config,
        skills: Vec<Skill>,
        project_root: PathBuf,
        session: Session,
        native_tools: bool,
        hooks: Arc<HookEngine>,
    ) -> Result<Self> {
        let agents_md = crate::instructions::load(&project_root);
        let memory_index = read_memory_index(&project_root);
        let model = config.active().model;
        let mut load_warning = None;
        // load_history replays persisted system notes, drops stale system
        // prompts, and repairs dangling tool calls from interrupted runs.
        let prior = session.load_history().unwrap_or_else(|err| {
            tracing::warn!("could not load session {}: {err}", session.path().display());
            load_warning = Some(format!(
                "previous session {} could not be read ({err}); starting fresh",
                session.path().display()
            ));
            Vec::new()
        });

        // Plan mode: one flag shared by the dispatcher (read-only gate) and
        // the always-registered exit_plan tool (cleared on approval).
        let plan_mode = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // Omakase: chef's-choice flavor of plan mode, shared with exit_plan
        // (auto-approve) and interview (decline to ask).
        let omakase = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let web = config.web.clone();

        // Checkpoints: per-file snapshots of everything this agent edits.
        // Old turns are garbage-collected once per session, here.
        let checkpoints = Arc::new(crate::checkpoint::CheckpointStore::open(
            &project_root,
            config.checkpoints.keep_turns,
        ));
        match checkpoints.gc() {
            Ok(dropped) if dropped > 0 => {
                tracing::debug!("checkpoint gc dropped {dropped} old turn(s)");
            }
            Ok(_) => {}
            Err(err) => tracing::warn!("checkpoint gc failed: {err:#}"),
        }

        let mut registry = registry;
        registry.register(Arc::new(crate::tools::plan::ExitPlanTool::new(
            Arc::clone(&plan_mode),
            Arc::clone(&omakase),
        )));
        registry.register(Arc::new(crate::tools::interview::InterviewTool::new(
            Arc::clone(&omakase),
        )));

        let usage = Arc::new(crate::usage::UsageTracker::new());

        let mut agent = Self {
            client,
            model,
            dispatcher: Dispatcher::new(
                registry,
                config.mode,
                Arc::clone(&hooks),
                Arc::clone(&plan_mode),
            ),
            hooks,
            mode: config.mode,
            config,
            history: Vec::new(),
            session,
            ctx: ToolContext::new(project_root)
                .with_web(web)
                .with_checkpoints(Arc::clone(&checkpoints))
                .with_usage(Arc::clone(&usage)),
            native_tools,
            skills,
            agents_md,
            memory_index,
            deadline: None,
            load_warning,
            plan_mode,
            plan_prompt_on: false,
            omakase,
            omakase_prompt_on: false,
            usage,
            usage_log: crate::usage::default_log_path(),
            checkpoints,
            cancel: CancelHandle::default(),
            subagent_model: None,
            ultra: None,
        };
        agent
            .history
            .push(ChatMessage::system(agent.compose_system_prompt()));
        agent.history.extend(prior);
        Ok(agent)
    }

    /// Current personality mode.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Switch mode mid-session (`/mode`): swaps the system prompt and
    /// circuit-breaker behavior for subsequent turns.
    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        self.config.mode = mode;
        self.dispatcher.set_mode(mode);
        self.refresh_system_prompt();
    }

    /// Set the reasoning effort (`/effort`) forwarded on subsequent turns.
    /// `None` leaves the provider default. Only reaches models that accept a
    /// `reasoning_effort` request field; others ignore it.
    pub fn set_reasoning_effort(&mut self, effort: Option<crate::config::ReasoningEffort>) {
        self.config.reasoning_effort = effort;
    }

    /// Mark this agent as running on a surface that drains slash commands the
    /// agent queues via `run_command` — the interactive TUI. Only then is the
    /// `run_command` tool useful; headless and gateway runs leave it off so the
    /// tool refuses rather than report success for a command nothing applies.
    /// Preserved across per-turn context clones (`ToolContext::with_events`).
    pub fn set_command_dispatch(&mut self, on: bool) {
        self.ctx.dispatches_commands = on;
    }

    /// Conversation history (system prompt included).
    pub fn history(&self) -> &[ChatMessage] {
        &self.history
    }

    /// Session this agent persists to.
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// The per-file checkpoint store (snapshots powering `/rewind` and
    /// perpetual rollback).
    pub fn checkpoints(&self) -> &Arc<crate::checkpoint::CheckpointStore> {
        &self.checkpoints
    }

    /// Recent turns `/rewind` can return to, newest first: this session's
    /// turn markers (prompt snippets) joined with the checkpoint store's
    /// per-turn file lists. Turns from before this session are listed only
    /// when the session has no markers at all (old-format resume), since
    /// only marked turns can also truncate the conversation.
    pub fn rewind_candidates(&self, limit: usize) -> Vec<RewindCandidate> {
        let markers = self.session.turn_markers().unwrap_or_default();
        let first_marked = markers.first().map(|marker| marker.turn);
        let mut by_turn: std::collections::BTreeMap<u64, RewindCandidate> = markers
            .into_iter()
            .map(|marker| {
                (
                    marker.turn,
                    RewindCandidate {
                        turn: marker.turn,
                        prompt: marker.prompt,
                        files: Vec::new(),
                    },
                )
            })
            .collect();
        for turn_files in self.checkpoints.recent_turns(usize::MAX) {
            if first_marked.is_some_and(|first| turn_files.turn < first) {
                continue;
            }
            by_turn
                .entry(turn_files.turn)
                .or_insert_with(|| RewindCandidate {
                    turn: turn_files.turn,
                    prompt: String::new(),
                    files: Vec::new(),
                })
                .files = turn_files.files;
        }
        by_turn.into_values().rev().take(limit).collect()
    }

    /// `/rewind`: restore every file snapshot from `turn` onward, drop the
    /// rewound turns from the session file, and reload the in-memory
    /// conversation to match. Returns the restored file paths.
    pub fn rewind_to(&mut self, turn: u64) -> Result<Vec<PathBuf>> {
        let restored = self
            .checkpoints
            .restore_turns_from(turn)
            .context("restoring checkpoints")?;
        self.session
            .truncate_after(turn)
            .context("truncating session history")?;
        let prior: Vec<ChatMessage> = self
            .session
            .load_history()
            .context("reloading session history")?;
        self.history.truncate(1);
        self.history.extend(prior);
        self.dispatcher.reset_failures();
        Ok(restored)
    }

    /// Set (or clear) the wall-clock deadline for this run (`--max-hours`).
    pub fn set_deadline(&mut self, deadline: Option<Instant>) {
        self.deadline = deadline;
    }

    /// `/clear`: drop everything but the system prompt and start a fresh
    /// session file. Background work from the old conversation is killed
    /// and detached (fresh registries; late monitors hold the old ones, so
    /// their notes can never reach the new conversation) and the todo list
    /// is reset.
    pub fn clear(&mut self) -> Result<()> {
        self.ctx.tasks.kill_all();
        self.ctx.subagents.kill_all();
        self.ctx.tasks = Arc::new(crate::tools::tasks::TaskRegistry::new());
        self.ctx.subagents = Arc::new(crate::tools::subagent_tasks::SubagentTaskRegistry::new());
        self.ctx.todos = Arc::new(std::sync::Mutex::new(Vec::new()));
        self.session = Session::create(&Config::sessions_dir()?)?;
        self.hooks.set_session_id(self.session.id.clone());
        self.history.truncate(1);
        self.dispatcher.reset_failures();
        Ok(())
    }

    /// Handle for cancelling the running turn cooperatively. The surface
    /// clones it before spawning `run_turn` and calls
    /// [`CancelHandle::cancel`] to interrupt: the turn stops at the next
    /// stream chunk or tool boundary, answers skipped tool calls with
    /// "(not executed — interrupted by user)", emits
    /// [`AgentEvent::Done`] with [`DoneReason::Stopped`], and returns —
    /// no task aborts, no agent rebuild, background work keeps running.
    pub fn cancel_handle(&self) -> CancelHandle {
        self.cancel.clone()
    }

    /// Bind the spawn tool's shared model slot (see
    /// [`subagent::SpawnSubagentTool::model_handle`]) so subagents run on
    /// this agent's active model, including after `/model` switches.
    pub fn bind_subagent_model(&mut self, handle: subagent::SharedActiveModel) {
        if let Ok(mut slot) = handle.write() {
            *slot = Some(self.model.clone());
        }
        self.subagent_model = Some(handle);
    }

    /// The lifecycle-hook engine this agent fires (shared for `/reload`
    /// registry rebuilds).
    pub fn hooks(&self) -> &Arc<HookEngine> {
        &self.hooks
    }

    /// Fire the `session_start` hooks. Hook stdout is appended to the
    /// session as system context, visible to the model on every turn.
    pub async fn fire_session_start(&mut self, events: &mpsc::Sender<AgentEvent>) {
        if let Some(extra) = self.hooks.session_start(self.mode, Some(events)).await {
            self.push(ChatMessage::system(format!(
                "[session_start hook]\n{extra}"
            )));
        }
    }

    /// Fire the `session_end` hooks. `events` is `None` when the surface is
    /// already torn down (e.g. the TUI terminal was restored).
    pub async fn fire_session_end(&self, events: Option<&mpsc::Sender<AgentEvent>>) {
        self.hooks.session_end(self.mode, events).await;
    }

    /// Swap the tool registry (after `/reload` or `/evolve`). Re-registers
    /// the always-present `exit_plan` and `interview` tools (sharing this
    /// agent's plan-mode and omakase flags) and refreshes the system prompt so
    /// the JSON tool protocol's tool list stays current.
    pub fn set_registry(&mut self, mut registry: ToolRegistry) {
        registry.register(Arc::new(crate::tools::plan::ExitPlanTool::new(
            Arc::clone(&self.plan_mode),
            Arc::clone(&self.omakase),
        )));
        registry.register(Arc::new(crate::tools::interview::InterviewTool::new(
            Arc::clone(&self.omakase),
        )));
        self.dispatcher.set_registry(registry);
        self.refresh_system_prompt();
    }

    /// Session token counters (prompt/completion totals, last prompt size).
    pub fn usage(&self) -> &crate::usage::UsageTracker {
        &self.usage
    }

    /// Number of background tasks (`execute` with `run_in_background`)
    /// still running, for `/status`.
    pub fn running_tasks(&self) -> usize {
        self.ctx
            .tasks
            .list()
            .iter()
            .filter(|task| !task.status.is_finished())
            .count()
    }

    /// Snapshot of every background task this session has spawned (running
    /// and finished), oldest first, for `/bashes`.
    pub fn tasks(&self) -> Vec<crate::tools::tasks::Task> {
        self.ctx.tasks.list()
    }

    /// Shared handle on the background-subagent registry, so a surface can
    /// kill a detached run. Cloned out rather than borrowed through the agent
    /// because the TUI parks the whole `Agent` elsewhere while a turn is in
    /// flight — which is exactly when you want to kill a runaway subagent.
    pub fn subagent_registry(&self) -> Arc<crate::tools::subagent_tasks::SubagentTaskRegistry> {
        Arc::clone(&self.ctx.subagents)
    }

    /// Redirect (or disable) the per-turn usage JSONL log. Defaults to
    /// `~/.wizard/usage.jsonl`; tests point it into a temp dir.
    pub fn set_usage_log(&mut self, path: Option<PathBuf>) {
        self.usage_log = path;
    }

    /// Whether plan mode is active.
    pub fn plan_mode(&self) -> bool {
        self.plan_mode.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Turn plan mode on or off (`/plan`, `--plan`, `plan_each_cycle`).
    /// While on, the dispatcher blocks every non-read-only tool except
    /// `exit_plan`, and the system prompt instructs the model to plan.
    pub fn set_plan_mode(&mut self, on: bool) {
        self.plan_mode
            .store(on, std::sync::atomic::Ordering::SeqCst);
        // Leaving plan mode also leaves omakase (omakase is a flavor of plan
        // mode; there is no omakase without the read-only exploration phase).
        if !on {
            self.omakase
                .store(false, std::sync::atomic::Ordering::SeqCst);
        }
        self.sync_plan_prompt();
    }

    /// Whether omakase (chef's-choice) mode is active.
    pub fn omakase(&self) -> bool {
        self.omakase.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Turn omakase mode on or off (`/omakase`, `--omakase`). Omakase implies
    /// plan mode, so enabling it enables plan mode too; the agent explores
    /// read-only, then auto-approves its own plan and proceeds.
    pub fn set_omakase(&mut self, on: bool) {
        self.omakase.store(on, std::sync::atomic::Ordering::SeqCst);
        if on {
            self.plan_mode
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        self.sync_plan_prompt();
    }

    /// Turn `/ultra` on with a built engine, or off. Unlike `/fusion` this swaps
    /// neither the client nor the registry — the candidates run on this agent's
    /// own client and model — so the toggle is instant and the conversation
    /// survives it.
    pub fn set_ultra(&mut self, engine: Option<Arc<ultra::UltraEngine>>) {
        self.ultra = engine;
    }

    /// Whether `/ultra` is on for this session.
    pub fn ultra(&self) -> bool {
        self.ultra.is_some()
    }

    /// Re-compose the system prompt when the plan-mode or omakase flag changed
    /// since it was last baked in. Either flag can flip mid-turn (exit_plan
    /// approval clears plan mode), so the turn loop calls this before every
    /// completion.
    fn sync_plan_prompt(&mut self) {
        let plan = self.plan_mode();
        let omakase = self.omakase();
        if plan != self.plan_prompt_on || omakase != self.omakase_prompt_on {
            self.plan_prompt_on = plan;
            self.omakase_prompt_on = omakase;
            self.refresh_system_prompt();
        }
    }

    /// Switch models mid-session (`/model`) without resetting conversation
    /// context. `native_tools` is the new model's tool-calling capability
    /// (probe with [`OllamaClient::supports_native_tools`]); the system
    /// prompt is recomposed so the JSON tool protocol section matches.
    pub fn set_model(&mut self, model: String, native_tools: bool) {
        self.config.model = model.clone();
        if let Some(handle) = &self.subagent_model
            && let Ok(mut slot) = handle.write()
        {
            *slot = Some(model.clone());
        }
        self.model = model;
        self.native_tools = native_tools;
        self.refresh_system_prompt();
    }

    /// Replace the skill set mid-session (`/reload`) and rebuild the system
    /// prompt so the new skills apply to subsequent turns.
    pub fn set_skills(&mut self, skills: Vec<Skill>) {
        self.skills = skills;
        self.refresh_system_prompt();
    }

    fn compose_system_prompt(&self) -> String {
        let mut prompt = prompts::build_system_prompt(
            self.mode,
            &self.skills,
            self.agents_md.as_deref(),
            self.memory_index.as_deref(),
        );
        if !self.native_tools {
            prompt.push_str("\n\n");
            prompt.push_str(&prompts::render_tool_protocol(
                &self.dispatcher.registry().specs(),
            ));
        }
        if self
            .dispatcher
            .registry()
            .get(crate::tools::todo::TODO_TOOL_NAME)
            .is_some()
        {
            prompt.push_str("\n\n");
            prompt.push_str(prompts::TODO_PROMPT);
        }
        if self.plan_mode() {
            prompt.push_str("\n\n");
            prompt.push_str(prompts::PLAN_MODE_PROMPT);
            if self.omakase() {
                prompt.push_str("\n\n");
                prompt.push_str(prompts::OMAKASE_PROMPT);
            }
        }
        prompt
    }

    fn refresh_system_prompt(&mut self) {
        self.memory_index = read_memory_index(&self.ctx.cwd);
        let prompt = self.compose_system_prompt();
        match self.history.first_mut() {
            Some(first) if first.role == Role::System => first.content = prompt,
            _ => self.history.insert(0, ChatMessage::system(prompt)),
        }
    }

    /// Append to history and persist. Injected system messages (background
    /// notes, subagent reports, hook context) persist as flagged system
    /// notes that replay on resume; the system prompt itself is never
    /// pushed (it lives at history[0] and is recomposed fresh).
    fn push(&mut self, message: ChatMessage) {
        let result = if message.role == Role::System {
            self.session.append_system_note(&message)
        } else {
            self.session.append(&message)
        };
        if let Err(err) = result {
            tracing::warn!("session append failed: {err}");
        }
        self.history.push(message);
    }

    /// Run one user turn: append `input`, then loop
    /// (stream completion → emit deltas → execute tool calls → feed results
    /// back) until the model stops calling tools or `max_steps` is reached.
    /// Always finishes with [`AgentEvent::Done`]. Each message is appended
    /// to the session file as it lands.
    pub async fn run_turn(
        &mut self,
        input: &str,
        events: mpsc::Sender<AgentEvent>,
    ) -> Result<DoneReason> {
        if let Some(warning) = self.load_warning.take() {
            let _ = emit(&events, AgentEvent::Error(warning)).await;
        }
        // Arm cancellation for this turn; a stale request from a previous
        // turn must not kill this one.
        self.cancel.clear();
        self.usage.begin_turn();
        let result = match self.turn_inner(input, &events).await {
            Ok(reason) => {
                let _ = emit(&events, AgentEvent::Done { reason }).await;
                Ok(reason)
            }
            Err(err) => {
                let _ = emit(&events, AgentEvent::Error(format!("{err:#}"))).await;
                let _ = emit(
                    &events,
                    AgentEvent::Done {
                        reason: DoneReason::Stopped,
                    },
                )
                .await;
                Err(err)
            }
        };
        // However the turn ended, ultra's guidance goes with it.
        self.drop_ultra_guidance();
        // turn_end hooks: observational, fired however the turn ended.
        self.hooks.turn_end(self.mode, Some(&events)).await;
        self.record_turn_usage();
        result
    }

    /// Drop the guidance `/ultra` injected for the turn that just ended.
    ///
    /// Guidance is turn-scoped by nature: it is N drafts and a verdict about
    /// *one* request, and that request has now been answered. Left in history it
    /// would be re-sent on every subsequent turn and accumulate one block per
    /// ultra turn — tens of KB each — until it filled a large fraction of the
    /// window with stale advice, and (because a guidance block sits immediately
    /// after its user message) it would also stall the compactor, whose kept
    /// tail must start at a `Role::User` message.
    ///
    /// It is only ever in `self.history`, never in the session file, so nothing
    /// has to be un-persisted. Compaction may have folded it into a summary
    /// mid-turn, in which case there is nothing left to find and this is a
    /// no-op — as it is on every turn of the ordinary single-agent path, which
    /// is why this is unconditional rather than gated on the ultra flag: the
    /// flag can be turned off between turns, and the block it left behind still
    /// has to go.
    fn drop_ultra_guidance(&mut self) {
        self.history.retain(|message| !ultra::is_guidance(message));
    }

    /// Append this turn's token usage to the JSONL log (when the backend
    /// reported counts and the log is enabled). Best-effort: failures are
    /// logged, never surfaced.
    fn record_turn_usage(&self) {
        let (prompt_tokens, completion_tokens) = self.usage.turn_totals();
        if prompt_tokens == 0 && completion_tokens == 0 {
            return;
        }
        let Some(path) = &self.usage_log else {
            return;
        };
        let record = crate::usage::UsageRecord {
            ts: crate::usage::unix_now(),
            project: self.ctx.cwd.display().to_string(),
            model: self.model.clone(),
            provider: self.config.active().name,
            prompt_tokens,
            completion_tokens,
            mode: self.mode.to_string(),
        };
        if let Err(err) = crate::usage::append(path, &record) {
            tracing::warn!("could not append usage record: {err:#}");
        }
    }

    async fn turn_inner(
        &mut self,
        input: &str,
        events: &mpsc::Sender<AgentEvent>,
    ) -> Result<DoneReason> {
        // user_prompt_submit hooks: may veto the turn before the model sees
        // the prompt (the message is never pushed to history), or append
        // extra context to it.
        let input = match self
            .hooks
            .user_prompt_submit_with_prompt(input, self.mode, Some(events))
            .await
        {
            PromptSubmit::Block(reason) => {
                let _ = emit(
                    events,
                    AgentEvent::Error(format!(
                        "prompt blocked by user_prompt_submit hook: {reason}"
                    )),
                )
                .await;
                return Ok(DoneReason::Stopped);
            }
            PromptSubmit::Continue(Some(extra)) => {
                format!("{input}\n\n[user_prompt_submit hook]\n{extra}")
            }
            PromptSubmit::Continue(None) => input.to_string(),
        };
        // Turn boundary: a fresh checkpoint turn for the dispatcher's
        // snapshots, anchored in the session file so /rewind can truncate
        // here. Best-effort — a marker failure never blocks the turn.
        let turn = self.checkpoints.begin_turn();
        if let Err(err) = self.session.append_marker(turn, &input) {
            tracing::warn!("could not append turn marker: {err}");
        }
        self.push(ChatMessage::user(input.clone()));

        // Ultra: the mixture-of-agents pre-phase. Candidates propose and judges
        // compare *before* the main loop starts, so their conclusions enter the
        // turn as one system note and the loop below — the only thing in this
        // session that may write — proceeds unchanged.
        //
        // Position is load-bearing. After the user push, so a cancellation here
        // leaves history exactly as a cancelled model stream does. Before
        // `compact_if_needed`, so a large guidance block is *accounted for* by
        // the compactor instead of overflowing the window behind it. `run`
        // borrows `self` immutably and hands back an owned outcome, so those
        // borrows are over by the time history needs `&mut self`.
        if let Some(engine) = self.ultra.clone() {
            let outcome = ultra::run(
                &engine,
                &input,
                // The history as it stood *before* this request: it is already
                // pushed, and a candidate must not read its own brief twice.
                &self.history[..self.history.len() - 1],
                &self.client,
                &self.model,
                self.dispatcher.registry(),
                &self.hooks,
                // Bare: `ultra::run` wires this turn's event channel into the
                // context itself, since that is what its candidates' panes hang
                // off (see its doc comment).
                &self.ctx,
                &self.cancel,
                events,
            )
            .await;
            match outcome {
                ultra::UltraOutcome::Guidance(guidance) => {
                    // The drafts and the verdict, verbatim, for the surface to
                    // keep: the candidates' panes retire off the rail long
                    // before this turn ends, and the guidance itself is never
                    // rendered, so without this the work the user just paid N×
                    // for would be unreadable everywhere.
                    let _ = emit(
                        events,
                        AgentEvent::UltraGuidance {
                            label: engine.label(),
                            guidance: guidance.clone(),
                        },
                    )
                    .await;
                    // History only, never the session: this is advice about the
                    // *one* request below it, so it is dropped again at the end
                    // of the turn (`drop_ultra_guidance`) and must not come back
                    // on `/resume` either. `push` would persist it as a system
                    // note, which is exactly what we do not want.
                    self.history.push(ChatMessage::system(guidance));
                }
                ultra::UltraOutcome::Skipped(reason) => {
                    let _ = emit(events, AgentEvent::Notice(format!("ultra: {reason}"))).await;
                }
                ultra::UltraOutcome::Cancelled => return Ok(DoneReason::Stopped),
            }
        }

        self.compact_if_needed(events).await;
        let max_steps = self.config.max_steps.max(1);

        for step in 1..=max_steps {
            // Surface background tasks that finished since the last step.
            self.drain_background_tasks(events).await;
            self.drain_background_subagents(events).await;
            if let Some(deadline) = self.deadline
                && Instant::now() >= deadline
            {
                return Ok(DoneReason::TimeLimit);
            }
            if self.mode == Mode::Sovereign
                && let Some(reason) = self.honor_loop_control().await
            {
                return Ok(reason);
            }
            // Plan mode can flip mid-turn (exit_plan approval): keep the
            // system prompt's plan-mode block in step with the flag.
            self.sync_plan_prompt();

            let (mut content, mut tool_calls) =
                match self.stream_completion_with_retry(events).await? {
                    // Cancelled mid-stream: the partial completion is discarded
                    // (never entered history), so nothing dangles.
                    Completion::Cancelled => return Ok(DoneReason::Stopped),
                    Completion::Done {
                        content,
                        tool_calls,
                    } => (content, tool_calls),
                };

            // Some reasoning models (xAI grok-4.3 after tool results) emit
            // only reasoning and stop, leaving the visible message empty.
            // Nudge once; if it stays empty, surface a notice instead of
            // ending the turn silently.
            if completion_is_empty(&content, &tool_calls) {
                // In-memory only (not `push`): the nudge must not pollute the
                // persisted session history.
                self.history.push(ChatMessage::user(EMPTY_COMPLETION_NUDGE));
                let retried = self.stream_completion_with_retry(events).await;
                self.history.pop();
                let (retry_content, retry_calls) = match retried? {
                    Completion::Cancelled => return Ok(DoneReason::Stopped),
                    Completion::Done {
                        content,
                        tool_calls,
                    } => (content, tool_calls),
                };
                if completion_is_empty(&retry_content, &retry_calls) {
                    let _ = emit(
                        events,
                        AgentEvent::Error("model returned an empty response".to_string()),
                    )
                    .await;
                    return Ok(DoneReason::Completed);
                }
                content = retry_content;
                tool_calls = retry_calls;
            }

            let assistant = ChatMessage {
                role: Role::Assistant,
                content: content.clone(),
                tool_calls: tool_calls.clone(),
                tool_name: None,
            };
            self.push(assistant);

            if !self.native_tools
                && tool_calls.is_empty()
                && let Some(call) = parse_json_tool_call(&content)
            {
                tool_calls.push(call);
            }

            if tool_calls.is_empty() {
                return Ok(DoneReason::Completed);
            }

            for (index, call) in tool_calls.iter().enumerate() {
                // Cancellation is honored between tool calls: pending calls
                // are answered so the persisted assistant message never
                // carries dangling tool_use.
                if self.cancel.is_cancelled() {
                    self.answer_skipped_calls(
                        &tool_calls[index..],
                        "(not executed — interrupted by user)",
                    );
                    return Ok(DoneReason::Stopped);
                }
                if let Some(reason) = self.dispatch_call(call, events).await {
                    // dispatch_call answered `call` itself; the rest of the
                    // batch never ran.
                    self.answer_skipped_calls(
                        &tool_calls[index + 1..],
                        "(not executed — turn ended early)",
                    );
                    return Ok(reason);
                }
            }

            // Compact between steps too, so a long tool loop cannot outgrow
            // the context window mid-turn. The compactor always keeps the
            // most recent messages verbatim, so the in-flight turn's tail —
            // the tool calls and results the model is reasoning about —
            // stays intact.
            self.compact_if_needed(events).await;

            if !emit(events, AgentEvent::StepCompleted { step }).await {
                return Ok(DoneReason::Stopped);
            }
        }

        Ok(DoneReason::MaxSteps)
    }

    /// Stream one completion, forwarding text deltas and collecting tool
    /// calls. Observes the turn's cancel handle so an interrupt lands at the
    /// next chunk (or immediately when the stream is idle).
    async fn stream_completion(&self, events: &mpsc::Sender<AgentEvent>) -> Result<Completion> {
        if self.cancel.is_cancelled() {
            return Ok(Completion::Cancelled);
        }
        let request = ChatRequest {
            model: self.model.clone(),
            messages: self.history.clone(),
            tools: if self.native_tools {
                self.dispatcher.registry().specs()
            } else {
                Vec::new()
            },
            stream: true,
            options: Some(ChatOptions {
                temperature: Some(self.mode.temperature()),
                num_ctx: None,
                reasoning_effort: self
                    .config
                    .reasoning_effort
                    .map(|effort| effort.as_str().to_string()),
            }),
        };

        let mut stream = self
            .client
            .chat_stream(request)
            .await
            .context("starting chat completion")?;

        let mut content = String::new();
        let mut tool_calls = Vec::new();
        let mut prompt_tokens = None;
        let mut completion_tokens = None;
        loop {
            let chunk = tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return Ok(Completion::Cancelled),
                chunk = stream.next() => match chunk {
                    Some(chunk) => chunk,
                    None => break,
                },
            };
            let chunk = chunk.context("reading chat stream")?;
            if let Some(message) = chunk.message {
                if !message.content.is_empty() {
                    if chunk.thinking {
                        // Reasoning is surfaced to the UI but never becomes
                        // part of the assistant message.
                        let _ = emit(events, AgentEvent::ThinkingDelta(message.content)).await;
                    } else {
                        content.push_str(&message.content);
                        let _ = emit(events, AgentEvent::TextDelta(message.content)).await;
                    }
                }
                tool_calls.extend(message.tool_calls);
            }
            if chunk.prompt_eval_count.is_some() {
                prompt_tokens = chunk.prompt_eval_count;
            }
            if chunk.eval_count.is_some() {
                completion_tokens = chunk.eval_count;
            }
            if chunk.done {
                break;
            }
        }
        if prompt_tokens.is_some() || completion_tokens.is_some() {
            self.usage.record(prompt_tokens, completion_tokens);
            let _ = emit(
                events,
                AgentEvent::Usage {
                    prompt_tokens: prompt_tokens.unwrap_or(0),
                    completion_tokens: completion_tokens.unwrap_or(0),
                },
            )
            .await;
        }
        Ok(Completion::Done {
            content,
            tool_calls,
        })
    }

    /// [`stream_completion`] with sleep-and-wake exponential backoff so a
    /// transient LLM outage (server down, rate-limited, mid-stream drop)
    /// pauses and retries instead of aborting the run. In continuous mode it
    /// retries indefinitely; otherwise it gives up after ~6 attempts. A
    /// non-transient error (auth, bad request, missing model) fails the turn
    /// immediately with the provider's message — in continuous mode too.
    async fn stream_completion_with_retry(
        &self,
        events: &mpsc::Sender<AgentEvent>,
    ) -> Result<Completion> {
        let mut attempt: u32 = 0;
        loop {
            match self.stream_completion(events).await {
                Ok(result) => return Ok(result),
                Err(err) => {
                    if !error_is_transient(&err) {
                        return Err(err);
                    }
                    if !self.config.continuous && attempt >= 6 {
                        return Err(err);
                    }
                    let secs = self.config.retry_max_secs.min(
                        self.config
                            .retry_base_secs
                            .saturating_mul(2u64.saturating_pow(attempt)),
                    );
                    let n = attempt + 1;
                    // The retry re-generates the response from the top; tell
                    // consumers to drop any partial text this attempt streamed.
                    let _ = emit(events, AgentEvent::StreamRetrying).await;
                    let _ = emit(
                        events,
                        AgentEvent::Error(format!(
                            "LLM unavailable ({err:#}); sleeping {secs}s then retrying (attempt {n})"
                        )),
                    )
                    .await;
                    // The backoff sleep is also cancellable.
                    tokio::select! {
                        biased;
                        _ = self.cancel.cancelled() => return Ok(Completion::Cancelled),
                        _ = tokio::time::sleep(Duration::from_secs(secs)) => {}
                    }
                    attempt += 1;
                }
            }
        }
    }

    /// Whether the history is close enough to overflowing to warrant
    /// compaction: either the serialized history exceeds the byte threshold,
    /// or the last model call's reported prompt size exceeds
    /// [`COMPACT_WINDOW_FRACTION`] of the provider's known context window.
    async fn should_compact(&self) -> bool {
        let total: usize = self.history.iter().map(|msg| msg.content.len()).sum();
        if total > self.config.compact_threshold_bytes {
            return true;
        }
        let Some(last_prompt) = self.usage.last_prompt_tokens() else {
            return false;
        };
        let Some(window) = self.client.context_window(&self.model).await else {
            return false;
        };
        last_prompt as f64 > f64::from(window) * COMPACT_WINDOW_FRACTION
    }

    /// Keep history bounded so the agent can run indefinitely. When
    /// [`Self::should_compact`] fires (byte threshold, or the last prompt
    /// nearing the provider's context window), summarize the middle span
    /// (everything between the system prompt and the last [`KEEP_RECENT`]
    /// messages) into a single progress note. Best-effort: a summarization
    /// failure falls back to dropping the middle span. Never aborts the turn.
    async fn compact_if_needed(&mut self, events: &mpsc::Sender<AgentEvent>) {
        if !self.should_compact().await {
            return;
        }
        match self.compact_now().await {
            CompactOutcome::Nothing => {}
            // Success is informational; only a truncation (the summary LLM
            // genuinely failed) is an error.
            outcome @ CompactOutcome::Summarized(_) => {
                let _ = emit(events, AgentEvent::Notice(outcome.describe())).await;
            }
            outcome @ CompactOutcome::Truncated { .. } => {
                let _ = emit(events, AgentEvent::Error(outcome.describe())).await;
            }
        }
    }

    /// Summarize the middle span (everything between the system prompt and the
    /// last [`KEEP_RECENT`] messages) into a single note, unconditionally —
    /// the `/compact` command's force path, and the shared core of
    /// [`compact_if_needed`]. A summarization failure falls back to dropping
    /// the middle span. Never aborts a turn.
    pub async fn compact_now(&mut self) -> CompactOutcome {
        // Need history[0] (system prompt) + a non-empty middle + the recent tail.
        if self.history.len() <= KEEP_RECENT + 1 {
            return CompactOutcome::Nothing;
        }
        let start = 1;
        let mut end = self.history.len() - KEEP_RECENT;
        // Never cut between an assistant tool-call message and its results:
        // snap the boundary back so the kept tail starts at a user message
        // (every tool-call group is preceded by one).
        while end > start && self.history[end].role != Role::User {
            end -= 1;
        }
        if start >= end {
            return CompactOutcome::Nothing;
        }
        let count = end - start;

        let outcome = match self.summarize_span(start, end).await {
            Ok(summary) => {
                let replacement =
                    ChatMessage::system(format!("{COMPACT_SUMMARY_HEADING}\n{summary}"));
                self.history
                    .splice(start..end, std::iter::once(replacement));
                CompactOutcome::Summarized(count)
            }
            Err(err) => {
                // Fall back to truncation: drop the middle span outright.
                self.history.drain(start..end);
                CompactOutcome::Truncated {
                    count,
                    error: format!("{err:#}"),
                }
            }
        };
        // The history just shrank: the last reported prompt size is stale
        // and must not re-trigger compaction on the next step.
        self.usage.clear_last_prompt();
        outcome
    }

    /// Summarize `history[start..end]` with a rolling per-chunk pass, so an
    /// arbitrarily large span is fully represented instead of hard-truncated:
    /// each chunk is summarized together with the summary of everything
    /// before it.
    async fn summarize_span(&self, start: usize, end: usize) -> Result<String> {
        // Render each message and pack into ~20k-char chunks, splitting
        // oversized messages at char boundaries.
        let mut chunks: Vec<String> = vec![String::new()];
        for msg in &self.history[start..end] {
            let role = match msg.role {
                Role::System => "system",
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "tool",
            };
            let rendered = format!("{role}: {}\n", msg.content);
            let mut rest = rendered.as_str();
            while !rest.is_empty() {
                let chunk = chunks.last_mut().expect("at least one chunk");
                let room = COMPACT_CHUNK_CHARS.saturating_sub(chunk.len());
                if rest.len() <= room {
                    chunk.push_str(rest);
                    break;
                }
                if room == 0 {
                    chunks.push(String::new());
                    continue;
                }
                let mut cut = room;
                while cut > 0 && !rest.is_char_boundary(cut) {
                    cut -= 1;
                }
                if cut == 0 {
                    chunks.push(String::new());
                    continue;
                }
                chunk.push_str(&rest[..cut]);
                rest = &rest[cut..];
                chunks.push(String::new());
            }
        }

        let mut summary: Option<String> = None;
        for chunk in &chunks {
            let blob = match &summary {
                None => chunk.clone(),
                Some(prev) => format!(
                    "[Progress summary of the transcript so far]\n{prev}\n\n\
                     [Transcript continues]\n{chunk}"
                ),
            };
            summary = Some(self.summarize_transcript(&blob).await?);
        }
        summary.ok_or_else(|| anyhow::anyhow!("nothing to summarize"))
    }

    /// Summarize a transcript blob into a terse progress note via the model.
    /// Used by [`compact_if_needed`]; deltas are not forwarded to the UI.
    async fn summarize_transcript(&self, blob: &str) -> Result<String> {
        let request = ChatRequest {
            model: self.model.clone(),
            messages: vec![
                ChatMessage::system(
                    "Summarize the following Wizard agent transcript into a compact progress \
                     note. Preserve: the mission/goal, decisions made, files changed, commands \
                     run, what worked/failed, and open next steps. Preserve the current todo \
                     list state verbatim (every item and its status) if one was maintained, \
                     and mention the plan file path (.wizard/plan.md) if a plan was written. \
                     Be terse and factual.",
                ),
                ChatMessage::user(blob.to_string()),
            ],
            tools: Vec::new(),
            stream: true,
            options: Some(ChatOptions {
                temperature: Some(0.2),
                num_ctx: None,
                // Internal summarization stays at the provider default; the
                // user's `/effort` applies to real turns, not compaction.
                reasoning_effort: None,
            }),
        };

        let mut stream = self
            .client
            .chat_stream(request)
            .await
            .context("starting compaction summary")?;
        let mut summary = String::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("reading compaction stream")?;
            if let Some(message) = chunk.message
                && !chunk.thinking
            {
                summary.push_str(&message.content);
            }
            if chunk.done {
                break;
            }
        }
        if summary.trim().is_empty() {
            anyhow::bail!("empty summary");
        }
        Ok(summary)
    }

    /// Run one tool call through the dispatcher and feed its results back to
    /// the model. Returns `Some(reason)` when the turn must end early (UI
    /// gone, circuit breaker).
    async fn dispatch_call(
        &mut self,
        call: &ToolCall,
        events: &mpsc::Sender<AgentEvent>,
    ) -> Option<DoneReason> {
        let outcome = self.dispatcher.dispatch(call, &self.ctx, events).await;
        match &outcome.output {
            Some(output) => self.push(self.tool_feedback(&call.function.name, output)),
            // No result (event receiver gone mid-batch): answer the call
            // anyway so the persisted history carries no dangling tool_use.
            None => self.push(self.tool_feedback(
                &call.function.name,
                &ToolOutput::ok("(not executed — turn ended early)"),
            )),
        }
        if let Some(nudge) = outcome.nudge {
            self.push(ChatMessage::system(nudge));
        }
        outcome.done
    }

    /// Answer tool calls that will never run (turn ended early, user
    /// interrupt) with a synthesized `note`, so the already-persisted
    /// assistant message carries no dangling tool_use.
    fn answer_skipped_calls(&mut self, calls: &[ToolCall], note: &str) {
        for call in calls {
            self.push(self.tool_feedback(&call.function.name, &ToolOutput::ok(note)));
        }
    }

    /// Build the message that feeds a tool result back to the model.
    fn tool_feedback(&self, name: &str, output: &ToolOutput) -> ChatMessage {
        let body = if output.is_error {
            format!("Error: {}", output.content)
        } else {
            output.content.clone()
        };
        if self.native_tools {
            ChatMessage::tool_result(name, body)
        } else {
            ChatMessage::user(format!("Tool result for `{name}`:\n{body}"))
        }
    }

    /// Honor `.wizard/loop-control` between steps: `stop` ends the turn,
    /// `pause` blocks until released, `skip` injects an instruction to move
    /// on. Returns `Some(reason)` when the turn must end.
    async fn honor_loop_control(&mut self) -> Option<DoneReason> {
        loop {
            match read_loop_control(&self.ctx.cwd) {
                Some(LoopControl::Stop) => {
                    clear_loop_control(&self.ctx.cwd);
                    return Some(DoneReason::Stopped);
                }
                Some(LoopControl::Pause) => {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                Some(LoopControl::Skip) => {
                    clear_loop_control(&self.ctx.cwd);
                    self.push(ChatMessage::user(
                        "Operator control: skip the current sub-task and move on to the next \
                         part of the task.",
                    ));
                    return None;
                }
                None => return None,
            }
        }
    }

    /// Drain background tasks that finished since the last check (each
    /// reported exactly once): inject a notification with the output tail
    /// into history so the model sees it on its next completion, and emit
    /// [`AgentEvent::TaskFinished`] for the surfaces. Called at the top of
    /// every agent step and every perpetual cycle.
    async fn drain_background_tasks(&mut self, events: &mpsc::Sender<AgentEvent>) {
        for task in self.ctx.tasks.drain_completed() {
            self.push(ChatMessage::system(task_note(&task)));
            let _ = emit(
                events,
                AgentEvent::TaskFinished {
                    id: task.id,
                    command: task.command,
                    status: task.status,
                },
            )
            .await;
        }
    }

    /// Drain backgrounded subagents (`spawn_subagent` with `background: true`)
    /// that finished since the last check (each reported exactly once):
    /// inject the report into history so the model sees it on its next
    /// completion, and emit [`AgentEvent::SubagentFinished`] for the
    /// surfaces. Called at the top of every agent step, alongside
    /// [`Self::drain_background_tasks`].
    async fn drain_background_subagents(&mut self, events: &mpsc::Sender<AgentEvent>) {
        for task in self.ctx.subagents.drain_completed() {
            self.push(ChatMessage::system(subagent_note(&task)));
            let _ = emit(
                events,
                AgentEvent::SubagentFinished {
                    id: task.id,
                    name: task.name,
                    task: task.task,
                    completed: task.completed,
                    output: task.output,
                },
            )
            .await;
        }
    }

    /// Collect background tasks and subagents that finished since the last
    /// check, injecting each note into history (persisted, exactly once) and
    /// returning the batch. For surfaces to poll on their idle tick — the
    /// same drain the turn loop runs at the top of every step — so finished
    /// work surfaces while the agent sits between turns. Cheap when nothing
    /// finished (two mutex-guarded scans, no I/O).
    pub fn drain_finished_notifications(&mut self) -> Vec<FinishedNotification> {
        let mut notifications = Vec::new();
        for task in self.ctx.tasks.drain_completed() {
            self.push(ChatMessage::system(task_note(&task)));
            notifications.push(FinishedNotification::Task(task));
        }
        for task in self.ctx.subagents.drain_completed() {
            self.push(ChatMessage::system(subagent_note(&task)));
            notifications.push(FinishedNotification::Subagent(task));
        }
        notifications
    }
}

impl Drop for Agent {
    /// Kill any still-running background tasks. Their children also carry
    /// `kill_on_drop`; this makes the teardown explicit and immediate.
    fn drop(&mut self) {
        self.ctx.tasks.kill_all();
    }
}

/// Read the persistent memory index (MEMORY.md) for `project_root`, if any
/// memories are saved. Failures are logged, not fatal — memory is an
/// enhancement, never a reason a session cannot start.
fn read_memory_index(project_root: &Path) -> Option<String> {
    let store = match crate::memory::MemoryStore::open(project_root) {
        Ok(store) => store,
        Err(err) => {
            tracing::warn!("could not open memory store: {err:#}");
            return None;
        }
    };
    match store.index() {
        Ok(index) => index,
        Err(err) => {
            tracing::warn!("could not read memory index: {err:#}");
            None
        }
    }
}

/// Build a fully wired headless [`Agent`]: construct the active provider's
/// client, health-check it, probe native tool support, assemble the tool
/// registry (native + scripted + MCP + subagent spawner + evolve + publish),
/// load skills, and open/create the session.
///
/// This is the shared agent-construction path used by both the sovereign
/// headless runner ([`run_headless`]) and the messaging gateway
/// ([`crate::gateway`]). `resume` reopens the latest session instead of
/// starting a new one.
pub async fn build_headless_agent(
    config: &Config,
    project_root: &Path,
    resume: bool,
) -> Result<Agent> {
    let active = config.active();
    let model = active.model.clone();
    let client = active
        .build()
        .with_context(|| format!("building provider '{}'", active.name))?;
    // llama.cpp gets a lifecycle hand: when nothing answers, Wizard starts
    // the server itself, showing spawn/load progress on a spinner (plain
    // stdout lines when stderr is not a terminal).
    if active.kind == ProviderKind::LlamaCpp {
        let wait = crate::progress::ServerSpinner::start();
        let outcome = crate::server::ensure_running(&active, &wait).await;
        wait.finish(outcome.is_ok());
        outcome?;
    }
    client
        .health()
        .await
        .with_context(|| format!("LLM health check failed for {}", client.label()))?;

    let native_tools = match client.supports_native_tools(&model).await {
        Ok(supported) => supported,
        Err(err) => {
            tracing::warn!(
                "could not probe tool support for '{model}': {err}; assuming native tools"
            );
            true
        }
    };
    if !native_tools {
        println!("model '{model}' lacks native tool calling; using the JSON tool protocol");
    }

    // Session first: the hook engine carries its id in every payload.
    let sessions_dir = Config::sessions_dir()?;
    let session = if resume {
        match Session::open_latest(&sessions_dir)? {
            Some(session) => session,
            None => Session::create(&sessions_dir)?,
        }
    } else {
        Session::create(&sessions_dir)?
    };

    // Lifecycle hooks, shared by the agent's dispatcher and the subagent
    // spawner so subagent tool calls fire the same hooks.
    let hooks = Arc::new(HookEngine::new(
        crate::hooks::load(project_root),
        project_root.to_path_buf(),
        session.id.clone(),
    ));

    // Tools: natives + scripted + MCP, then the subagent spawner on top.
    let mut base = ToolRegistry::with_native_tools();
    match Config::scripted_tools_dir() {
        Ok(dir) => {
            if let Err(err) = base.load_scripted(&dir) {
                tracing::warn!("loading scripted tools failed: {err}");
            }
        }
        Err(err) => tracing::warn!("scripted tools dir unavailable: {err}"),
    }
    let manager = match Config::mcp_config_path().and_then(|path| McpConfig::load(&path)) {
        Ok(mcp_config) => match McpManager::connect_all(&mcp_config).await {
            Ok(manager) => manager,
            Err(err) => {
                tracing::warn!("MCP startup failed: {err}");
                McpManager::empty()
            }
        },
        Err(err) => {
            tracing::warn!("could not load mcp.toml: {err}");
            McpManager::empty()
        }
    };
    if let Err(err) = base.attach_mcp(&manager).await {
        tracing::warn!("attaching MCP tools failed: {err}");
    }
    base.apply_harness_overrides();

    let subagents_dir = Config::subagents_dir()?;
    let subagent_configs = subagent::available_configs(&subagents_dir);
    let base = Arc::new(base);
    let mut registry = subagent::scoped_registry(&base, None);
    let spawn_tool = Arc::new(subagent::SpawnSubagentTool::new(
        subagent_configs,
        Arc::clone(&client),
        Arc::clone(&base),
        Arc::clone(&hooks),
    ));
    // Bound to the agent below so subagents follow `/model` switches.
    let subagent_model = spawn_tool.model_handle();
    registry.register(spawn_tool);
    registry.register(Arc::new(crate::tools::evolve::EvolveTool::new(
        config.clone(),
    )));
    registry.register(Arc::new(crate::tools::publish::PublishTool::new(
        config.clone(),
    )));

    // Skills: repo/bundled roots + user (~/.wizard/skills), user shadowing.
    let skill_roots = crate::skills::default_roots();
    let skills = crate::skills::load_skills(&skill_roots).unwrap_or_else(|err| {
        tracing::warn!("loading skills failed: {err}");
        Vec::new()
    });

    let mut agent = Agent::new(
        client,
        registry,
        config.clone(),
        skills,
        project_root.to_path_buf(),
        session,
        native_tools,
        hooks,
    )?;
    agent.bind_subagent_model(subagent_model);
    Ok(agent)
}

/// `rollback_failed_cycles`: restore every checkpoint from the failed
/// cycle's first turn onward and note the rollback in the persisted mission.
/// Best-effort — failures are logged and the run proceeds to its normal end.
fn rollback_failed_cycle(
    config: &Config,
    agent: &Agent,
    mission: Option<&mut mission::Mission>,
    project_root: &Path,
    first_turn: u64,
    why: &str,
    // `None` in the structured output formats, where stdout is JSON-only.
    spinner: Option<&crate::progress::TurnSpinner>,
) {
    if !config.rollback_failed_cycles {
        return;
    }
    match agent.checkpoints().restore_turns_from(first_turn) {
        Ok(restored) => {
            if restored.is_empty() {
                return;
            }
            if let Some(spinner) = spinner {
                spinner.println(&format!(
                    "[rolled back {} file(s) after {why}]",
                    restored.len()
                ));
            }
            if let Some(mission) = mission {
                mission.note(format!(
                    "rolled back {} file(s) after {why} (cycle starting at turn {first_turn})",
                    restored.len()
                ));
                if let Err(err) = mission.save(project_root) {
                    tracing::warn!("could not record rollback in mission.toml: {err:#}");
                }
            }
        }
        Err(err) => tracing::warn!("cycle rollback failed: {err:#}"),
    }
}

/// Sovereign-mode headless runner: builds an [`Agent`] and drives it in an
/// outer loop. The goal comes from `cli.prompt`, or (on a self-evolve
/// re-exec) from the persisted [`mission::Mission`]. With `--continuous` it
/// runs perpetually — persisting a mission, self-directing the next action
/// after each completed cycle, sleeping-and-waking through transient LLM
/// outages, compacting context, and re-exec'ing itself after a self-evolve —
/// until stopped via `.wizard/loop-control`, `--max-hours`, or the circuit
/// breaker. Otherwise it honors the `--loop N` bound. Prints progress to
/// stdout instead of the TUI (`--output-format` selects the
/// [`crate::output::EventSink`]); the returned exit code encodes the
/// outcome (see [`crate::output::exit_code`]).
pub async fn run_headless(config: Config, cli: Cli) -> Result<i32> {
    let project_root = std::env::current_dir().context("determining project root")?;

    // Goal resolution: an explicit `-p` wins; otherwise resume the standing
    // mission (this is the path taken after a self-evolve re-exec, which
    // relaunches without `-p`); otherwise there is nothing to do.
    let goal = if let Some(prompt) = cli.prompt.clone() {
        prompt
    } else if let Some(existing) = mission::Mission::load(&project_root)? {
        existing.goal
    } else {
        return Err(anyhow::anyhow!(
            "headless mode needs a task: pass -p \"<task>\""
        ));
    };
    // The same preprocessing the TUI applies on submit: custom `/command`
    // expansion and `@file` references.
    let custom_commands = crate::commands::load(&project_root);
    let goal = crate::commands::preprocess(&goal, &custom_commands, &project_root);

    let active = config.active();
    let model = active.model.clone();
    let endpoint = active.base_url.clone();

    let mut agent = build_headless_agent(&config, &project_root, cli.resume).await?;
    agent.set_deadline(
        cli.max_hours
            .map(|hours| Instant::now() + Duration::from_secs_f64(hours * 3600.0)),
    );
    // `--plan` / `plan_first = true`: the first turn starts in plan mode.
    // The model investigates read-only, presents a plan via exit_plan, the
    // printer below auto-approves it, and the same turn proceeds to execute
    // — a natural two-phase turn with no human in the loop.
    if config.plan_first {
        agent.set_plan_mode(true);
    }

    // Dashboard-dispatched background session (`--bg`): register in the session
    // registry and keep a heartbeat ticking so `/dashboard` shows it as a live
    // "Working" row. The terminal state is written once the run ends, below.
    let bg_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut bg_record: Option<crate::session_registry::SessionRecord> = None;
    let mut bg_ticker: Option<tokio::task::JoinHandle<()>> = None;
    if cli.bg {
        let headline = goal
            .lines()
            .next()
            .unwrap_or("background run")
            .chars()
            .take(48)
            .collect::<String>();
        let record = crate::session_registry::SessionRecord {
            id: agent.session().id.clone(),
            name: if headline.is_empty() {
                "background run".to_string()
            } else {
                headline.clone()
            },
            cwd: project_root.display().to_string(),
            model: model.clone(),
            mode: "sovereign".to_string(),
            state: crate::session_registry::SessionState::Working,
            activity: format!("working: {headline}"),
            pid: std::process::id(),
            started_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            updated_unix: 0,
        };
        crate::session_registry::write(&record);
        let stop = Arc::clone(&bg_stop);
        let ticker_record = record.clone();
        bg_ticker = Some(tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(3)).await;
                if stop.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                crate::session_registry::write(&ticker_record);
            }
        }));
        bg_record = Some(record);
    }

    // Busy spinner ("Conjuring…") shown while the model thinks or a tool
    // runs, hidden while output streams. Shared with the text sink; a
    // no-op when stderr is not a terminal. The structured formats never
    // show it and keep stdout pure JSON (`text_mode` gates every plain
    // stdout line below).
    let spinner = Arc::new(crate::progress::TurnSpinner::new());
    let text_mode = cli.output_format == crate::output::OutputFormat::Text;

    let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);
    // The sink consumes every agent event off the run loop; it is returned
    // so `finish` can emit the run summary once the outcome is known.
    let mut sink: Box<dyn crate::output::EventSink> = match cli.output_format {
        crate::output::OutputFormat::Text => {
            Box::new(crate::output::TextSink::new(Arc::clone(&spinner)))
        }
        crate::output::OutputFormat::Json => Box::new(crate::output::JsonSink::stdout()),
        crate::output::OutputFormat::StreamJson => {
            Box::new(crate::output::StreamJsonSink::stdout())
        }
    };
    let printer = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            sink.event(event);
        }
        sink
    });

    if text_mode {
        println!(
            "wizard {} — model {model} @ {endpoint} — task: {goal}",
            config.mode
        );
    }

    // session_start hooks fire once for the whole run.
    agent.fire_session_start(&tx).await;

    // Continuous mode persists a long-lived mission so the loop survives
    // restarts and binary self-replacement (deep evolve re-exec).
    let mut mission_state = if config.continuous {
        let mission = match mission::Mission::load(&project_root)? {
            Some(existing) => existing,
            None => {
                let fresh = mission::Mission::new(goal.clone());
                fresh.save(&project_root)?;
                fresh
            }
        };
        Some(mission)
    } else {
        None
    };

    let max_iterations = cli.loop_limit.unwrap_or(1).max(1);
    let mut input = goal.clone();
    let mut final_reason = DoneReason::Completed;
    let mut run_error: Option<anyhow::Error> = None;
    // Set when a self-evolve marker is consumed: after draining the printer we
    // re-exec into the freshly built/extended binary.
    let mut reexec_after = false;
    let mut iteration: u32 = 0;

    loop {
        iteration += 1;
        if !config.continuous && iteration > max_iterations {
            break;
        }

        // Honor a graceful stop at the top of every cycle.
        if read_loop_control(&project_root) == Some(LoopControl::Stop) {
            clear_loop_control(&project_root);
            final_reason = DoneReason::Stopped;
            break;
        }
        if config.continuous {
            if text_mode {
                spinner.println(&format!("\n=== cycle {iteration} ==="));
            }
            // `plan_each_cycle = true`: every cycle starts by planning again
            // (the previous cycle's exit_plan approval cleared the flag).
            if config.plan_each_cycle {
                agent.set_plan_mode(true);
            }
        } else if max_iterations > 1 && text_mode {
            spinner.println(&format!("\n=== iteration {iteration}/{max_iterations} ==="));
        }

        // Fresh verb per turn (same mechanism as the TUI's busy spinner), so
        // one turn reads as one activity. Structured formats never spin.
        if text_mode {
            let verb_seed = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos() as u64)
                .wrapping_add(u64::from(iteration));
            spinner.set_verb(config.ui.spinner_verb(verb_seed));
            spinner.show();
        }

        // Surface background tasks that finished between cycles (the turn
        // loop also drains at the top of every step).
        agent.drain_background_tasks(&tx).await;

        let turn_started = Instant::now();
        let turn_prompt = input.clone();
        // First checkpoint turn of this cycle, for rollback_failed_cycles
        // (run_turn assigns the next id via begin_turn).
        let cycle_first_turn = agent.checkpoints().current_turn() + 1;
        match agent.run_turn(&input, tx.clone()).await {
            Ok(reason) => {
                final_reason = reason;
                // Record the completed turn as a benchmark candidate
                // (`wizard bench promote`). Infallible and silent inside
                // bench replays, so it can never affect the run itself.
                crate::bench::record::record(
                    &project_root,
                    &turn_prompt,
                    &format!("{reason:?}"),
                    turn_started.elapsed(),
                    &model,
                    if config.continuous {
                        "continuous"
                    } else {
                        "sovereign"
                    },
                )
                .await;
                match reason {
                    DoneReason::MaxSteps => {
                        input = "Continue the task from where you left off. If it is already \
                                 complete, summarize what was done."
                            .to_string();
                    }
                    DoneReason::Completed => {
                        if config.continuous {
                            // Never idle: record the cycle and self-direct the
                            // next most valuable action toward the mission.
                            if let Some(mission) = mission_state.as_mut() {
                                mission.record_cycle(Some(format!("cycle done: {reason:?}")));
                                mission.save(&project_root)?;
                                input = format!(
                                    "You are operating CONTINUOUSLY and autonomously toward this \
                                     standing mission:\n\n{goal}\n\nYou just reported the current \
                                     sub-task complete (cycle {}). Re-examine the project state, \
                                     then choose and carry out the single most valuable next \
                                     action that advances the mission. If the mission itself is \
                                     genuinely and fully complete, instead pick a high-value \
                                     improvement to the project — better tests, docs, \
                                     performance, robustness — or improve your OWN capabilities \
                                     using the `evolve` tool. Never idle; always advance.",
                                    mission.cycles
                                );
                            }
                        } else {
                            break;
                        }
                    }
                    DoneReason::Stopped | DoneReason::TimeLimit => {
                        break;
                    }
                    DoneReason::CircuitBreaker => {
                        rollback_failed_cycle(
                            &config,
                            &agent,
                            mission_state.as_mut(),
                            &project_root,
                            cycle_first_turn,
                            "circuit breaker",
                            text_mode.then_some(&*spinner),
                        );
                        break;
                    }
                }
            }
            Err(err) => {
                rollback_failed_cycle(
                    &config,
                    &agent,
                    mission_state.as_mut(),
                    &project_root,
                    cycle_first_turn,
                    "hard error",
                    text_mode.then_some(&*spinner),
                );
                run_error = Some(err);
                break;
            }
        }

        // After the turn, react to self-evolution markers: a deep rebuild
        // (`evolve-reexec`) or a tier-1 extension (`evolve-reload`) both mean
        // the running image is stale, so we re-exec to reload everything.
        // Only meaningful in continuous mode, where the persisted mission lets
        // the relaunched process resume without a `-p` goal; a one-shot run
        // just finishes and the next launch picks up the new binary.
        let reexec = mission::reexec_marker(&project_root);
        let reload = mission::reload_marker(&project_root);
        if config.continuous && (reexec.exists() || reload.exists()) {
            if let Some(mission) = mission_state.as_ref() {
                mission.save(&project_root)?;
            }
            let _ = std::fs::remove_file(&reexec);
            let _ = std::fs::remove_file(&reload);
            reexec_after = true;
            break;
        }

        if config.cycle_pause_secs > 0 {
            tokio::time::sleep(Duration::from_secs(config.cycle_pause_secs)).await;
        }
    }

    // session_end hooks fire however the run ended (including just before a
    // self-evolve re-exec replaces the process).
    agent.fire_session_end(Some(&tx)).await;

    drop(tx);
    let mut sink = match printer.await {
        Ok(sink) => sink,
        Err(err) => return Err(anyhow::anyhow!("output task panicked: {err}")),
    };
    spinner.finish();

    // Background session: stop the heartbeat and record the terminal state so
    // the dashboard shows the result (completed/failed) rather than the row
    // vanishing. The terminal record is retained (not removed) by the registry.
    if let Some(mut record) = bg_record.take() {
        bg_stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(ticker) = bg_ticker.take() {
            ticker.abort();
        }
        if run_error.is_some() {
            record.state = crate::session_registry::SessionState::Failed;
            record.activity = "failed".to_string();
        } else {
            record.state = crate::session_registry::SessionState::Completed;
            record.activity = "completed".to_string();
        }
        crate::session_registry::write(&record);
    }

    if reexec_after {
        use std::os::unix::process::CommandExt;
        let exe = std::env::current_exe().context("locating current executable for re-exec")?;
        if text_mode {
            println!("[re-exec into evolved binary {}]", exe.display());
        }
        let err = std::process::Command::new(exe)
            .arg("--mode")
            .arg("sovereign")
            .arg("--continuous")
            .arg("--cwd")
            .arg(&project_root)
            .exec(); // never returns on success
        return Err(anyhow::anyhow!("re-exec after evolve failed: {err}"));
    }

    if let Some(err) = run_error {
        return Err(err);
    }
    // The sink emits the run summary (the text trailer line, or the final
    // JSON object / `done` JSONL line) and leaves stdout flushed.
    sink.finish(final_reason);
    Ok(crate::output::exit_code(final_reason))
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use futures_util::stream;

    use super::*;
    use crate::hooks::{HookDef, HookEvent};
    use crate::llm::{ChatChunk, ChatStream};

    /// Temp project dir removed on drop.
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

    /// Provider that replays canned chunk sequences and records the requests
    /// it received, for exercising the agent loop without a server.
    #[derive(Debug)]
    struct ScriptedProvider {
        responses: Mutex<VecDeque<Vec<ChatChunk>>>,
        requests: Mutex<Vec<ChatRequest>>,
        /// Reported context window (None = unknown, like a local model).
        context_window: Option<u32>,
    }

    impl ScriptedProvider {
        fn new(responses: Vec<Vec<ChatChunk>>) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(responses.into()),
                requests: Mutex::new(Vec::new()),
                context_window: None,
            })
        }

        fn with_context_window(responses: Vec<Vec<ChatChunk>>, window: u32) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(responses.into()),
                requests: Mutex::new(Vec::new()),
                context_window: Some(window),
            })
        }
    }

    #[async_trait::async_trait]
    impl LlmProvider for ScriptedProvider {
        async fn health(&self) -> Result<()> {
            Ok(())
        }

        async fn supports_native_tools(&self, _model: &str) -> Result<bool> {
            Ok(true)
        }

        async fn list_models(&self) -> Result<Vec<String>> {
            Ok(Vec::new())
        }

        async fn chat_stream(&self, request: ChatRequest) -> Result<ChatStream> {
            self.requests.lock().unwrap().push(request);
            let chunks = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted response available");
            Ok(futures_util::StreamExt::boxed(stream::iter(
                chunks.into_iter().map(Ok),
            )))
        }

        async fn context_window(&self, _model: &str) -> Option<u32> {
            self.context_window
        }

        fn label(&self) -> String {
            "scripted:test".to_string()
        }
    }

    /// `done: true` chunk; `content` becomes the visible message when
    /// non-empty.
    fn final_chunk(content: &str) -> ChatChunk {
        ChatChunk {
            message: (!content.is_empty()).then(|| ChatMessage::assistant(content)),
            thinking: false,
            done: true,
            done_reason: None,
            eval_count: None,
            prompt_eval_count: None,
        }
    }

    /// `done: true` chunk carrying token counts alongside `content`.
    fn usage_chunk(content: &str, prompt_tokens: u64, completion_tokens: u64) -> ChatChunk {
        ChatChunk {
            eval_count: Some(completion_tokens),
            prompt_eval_count: Some(prompt_tokens),
            ..final_chunk(content)
        }
    }

    fn test_agent(responses: Vec<Vec<ChatChunk>>) -> (Agent, Arc<ScriptedProvider>, TempDir) {
        let tmp = TempDir::new();
        let (agent, provider) = test_agent_in(&tmp, responses, Vec::new(), ToolRegistry::new());
        (agent, provider, tmp)
    }

    /// Build a test agent rooted in `tmp` with injected hook definitions and
    /// a custom registry.
    fn test_agent_in(
        tmp: &TempDir,
        responses: Vec<Vec<ChatChunk>>,
        hook_defs: Vec<HookDef>,
        registry: ToolRegistry,
    ) -> (Agent, Arc<ScriptedProvider>) {
        let provider = ScriptedProvider::new(responses);
        let agent = test_agent_with(tmp, Arc::clone(&provider), hook_defs, registry);
        (agent, provider)
    }

    /// Build a test agent around an existing provider. The usage log is
    /// redirected into the temp dir so tests never touch ~/.wizard.
    fn test_agent_with(
        tmp: &TempDir,
        provider: Arc<ScriptedProvider>,
        hook_defs: Vec<HookDef>,
        registry: ToolRegistry,
    ) -> Agent {
        let session = Session::create(&tmp.0).expect("create session");
        let hooks = Arc::new(HookEngine::new(
            hook_defs,
            tmp.0.clone(),
            session.id.clone(),
        ));
        let mut agent = Agent::new(
            provider,
            registry,
            Config::default(),
            Vec::new(),
            tmp.0.clone(),
            session,
            true,
            hooks,
        )
        .expect("build agent");
        agent.set_usage_log(Some(tmp.0.join("usage.jsonl")));
        agent
    }

    /// Drain a finished turn's events into (text, errors, notices).
    fn drain_events(rx: &mut mpsc::Receiver<AgentEvent>) -> (String, Vec<String>, Vec<String>) {
        let mut text = String::new();
        let mut errors = Vec::new();
        let mut notices = Vec::new();
        while let Ok(event) = rx.try_recv() {
            match event {
                AgentEvent::TextDelta(delta) => text.push_str(&delta),
                AgentEvent::Error(message) => errors.push(message),
                AgentEvent::Notice(message) => notices.push(message),
                _ => {}
            }
        }
        (text, errors, notices)
    }

    /// Test tool that records the arguments of every call it receives.
    struct RecordingTool {
        calls: Arc<Mutex<Vec<Value>>>,
    }

    #[async_trait::async_trait]
    impl crate::tools::Tool for RecordingTool {
        fn name(&self) -> &str {
            "echo"
        }

        fn description(&self) -> &str {
            "Echo the arguments back (test tool)."
        }

        fn parameters(&self) -> Value {
            json!({ "type": "object", "properties": {} })
        }

        async fn execute(
            &self,
            args: Value,
            _ctx: &ToolContext,
        ) -> Result<ToolOutput, crate::tools::ToolError> {
            self.calls.lock().unwrap().push(args.clone());
            Ok(ToolOutput::ok(format!("echoed {args}")))
        }
    }

    /// Registry holding one [`RecordingTool`], plus the shared call log.
    fn recording_registry() -> (ToolRegistry, Arc<Mutex<Vec<Value>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(RecordingTool {
            calls: Arc::clone(&calls),
        }));
        (registry, calls)
    }

    /// `done: true` chunk carrying one tool call.
    fn tool_call_chunk(name: &str, arguments: Value) -> ChatChunk {
        ChatChunk {
            message: Some(ChatMessage {
                role: Role::Assistant,
                content: String::new(),
                tool_calls: vec![ToolCall {
                    function: FunctionCall {
                        name: name.to_string(),
                        arguments,
                    },
                }],
                tool_name: None,
            }),
            thinking: false,
            done: true,
            done_reason: None,
            eval_count: None,
            prompt_eval_count: None,
        }
    }

    /// Write a hook script into `dir` and return the command that runs it
    /// (via `sh`, so no exec bit is needed).
    fn write_script(dir: &Path, name: &str, body: &str) -> String {
        let path = dir.join(name);
        std::fs::write(&path, body).expect("write hook script");
        format!("sh {}", path.display())
    }

    fn hook(event: HookEvent, matcher: Option<&str>, command: String) -> HookDef {
        HookDef {
            event,
            matcher: matcher.map(str::to_string),
            command,
            timeout_secs: None,
        }
    }

    #[test]
    fn completion_is_empty_requires_no_text_and_no_calls() {
        assert!(completion_is_empty("", &[]));
        assert!(completion_is_empty("  \n\t", &[]));
        assert!(!completion_is_empty("done", &[]));
        let call = ToolCall {
            function: FunctionCall {
                name: "execute".to_string(),
                arguments: json!({}),
            },
        };
        assert!(!completion_is_empty("", std::slice::from_ref(&call)));
        assert!(!completion_is_empty("done", &[call]));
    }

    #[tokio::test]
    async fn empty_completion_retries_with_nudge_then_succeeds() {
        let (mut agent, provider, _tmp) = test_agent(vec![
            // First completion: reasoning-only stop, nothing visible.
            vec![final_chunk("")],
            // Retry after the nudge: a real reply.
            vec![final_chunk("Here are my findings.")],
        ]);

        let (tx, mut rx) = mpsc::channel(64);
        let reason = agent.run_turn("hi", tx).await.expect("turn ok");
        assert_eq!(reason, DoneReason::Completed);

        let (text, errors, _notices) = drain_events(&mut rx);
        assert_eq!(text, "Here are my findings.");
        assert!(errors.is_empty(), "no notice on a successful retry");

        // The retry request carried the nudge as its final user message.
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let nudge = requests[1].messages.last().expect("retry has messages");
        assert_eq!(nudge.role, Role::User);
        assert_eq!(nudge.content, EMPTY_COMPLETION_NUDGE);

        // The nudge never lands in history or the persisted session.
        assert!(
            agent
                .history()
                .iter()
                .all(|m| m.content != EMPTY_COMPLETION_NUDGE),
            "nudge is not kept in history"
        );
        let persisted = agent.session().load_messages().expect("session readable");
        assert!(
            persisted
                .iter()
                .all(|m| m.content != EMPTY_COMPLETION_NUDGE),
            "nudge is not persisted"
        );
        assert!(
            persisted
                .iter()
                .any(|m| m.content == "Here are my findings."),
            "the real reply is persisted"
        );
    }

    #[tokio::test]
    async fn double_empty_completion_surfaces_a_notice() {
        let (mut agent, provider, _tmp) =
            test_agent(vec![vec![final_chunk("")], vec![final_chunk("")]]);

        let (tx, mut rx) = mpsc::channel(64);
        let reason = agent.run_turn("hi", tx).await.expect("turn ok");
        assert_eq!(reason, DoneReason::Completed);

        let (text, errors, _notices) = drain_events(&mut rx);
        assert!(text.is_empty());
        assert!(
            errors.iter().any(|e| e.contains("empty response")),
            "visible notice emitted: {errors:?}"
        );

        assert_eq!(provider.requests.lock().unwrap().len(), 2, "retried once");
        // No empty assistant message is recorded.
        assert!(
            agent
                .history()
                .iter()
                .all(|m| m.role != Role::Assistant || !m.content.is_empty()),
            "no empty assistant message in history"
        );
    }

    #[test]
    fn parses_whole_message_protocol_call() {
        let call =
            parse_json_tool_call(r#"{"tool":"read_file","arguments":{"path":"src/lib.rs"}}"#)
                .expect("valid protocol call");
        assert_eq!(call.function.name, "read_file");
        assert_eq!(call.function.arguments["path"], "src/lib.rs");
    }

    #[test]
    fn parses_fenced_json_block_with_prose() {
        let text = "I'll check the diff first.\n```json\n{\"tool\":\"git_diff\",\"arguments\":{\"staged\":true}}\n```\nThen I'll proceed.";
        let call = parse_json_tool_call(text).expect("fenced call parses");
        assert_eq!(call.function.name, "git_diff");
        assert_eq!(call.function.arguments["staged"], true);
    }

    #[test]
    fn parses_fence_without_language_tag() {
        let text = "```\n{\"tool\":\"git_status\"}\n```";
        let call = parse_json_tool_call(text).expect("bare fence parses");
        assert_eq!(call.function.name, "git_status");
    }

    #[test]
    fn parses_single_json_line_inside_prose() {
        let text = "Let me list the files.\n{\"tool\":\"list_files\",\"arguments\":{\"path\":\".\"}}\nThat should do it.";
        let call = parse_json_tool_call(text).expect("inline line parses");
        assert_eq!(call.function.name, "list_files");
    }

    #[test]
    fn missing_arguments_default_to_empty_object() {
        let call = parse_json_tool_call(r#"{"tool":"git_status"}"#).expect("parses");
        assert_eq!(call.function.arguments, json!({}));

        let call =
            parse_json_tool_call(r#"{"tool":"git_status","arguments":null}"#).expect("parses");
        assert_eq!(call.function.arguments, json!({}));
    }

    #[test]
    fn plain_text_and_non_tool_json_are_not_calls() {
        assert!(parse_json_tool_call("I finished the task. All tests pass.").is_none());
        assert!(parse_json_tool_call(r#"{"result": "done"}"#).is_none());
        assert!(parse_json_tool_call("```json\n{\"answer\": 42}\n```").is_none());
        assert!(parse_json_tool_call("").is_none());
    }

    #[test]
    fn normalize_args_handles_null_and_double_encoding() {
        assert_eq!(normalize_args(&Value::Null), json!({}));
        // Some models double-encode arguments as a JSON string.
        assert_eq!(
            normalize_args(&json!("{\"path\":\"a.rs\"}")),
            json!({ "path": "a.rs" })
        );
        // A plain (non-JSON) string is passed through untouched.
        assert_eq!(normalize_args(&json!("not json")), json!("not json"));
        // Objects pass through.
        assert_eq!(normalize_args(&json!({ "k": 1 })), json!({ "k": 1 }));
    }

    #[test]
    fn loop_control_parses_known_commands() {
        let tmp = TempDir::new();
        let control_dir = tmp.0.join(".wizard");
        std::fs::create_dir_all(&control_dir).unwrap();

        for (content, expected) in [
            ("stop", LoopControl::Stop),
            ("  PAUSE \n", LoopControl::Pause),
            ("Skip", LoopControl::Skip),
        ] {
            std::fs::write(control_dir.join("loop-control"), content).unwrap();
            assert_eq!(
                read_loop_control(&tmp.0),
                Some(expected),
                "content {content:?}"
            );
        }

        std::fs::write(control_dir.join("loop-control"), "resume").unwrap();
        assert_eq!(read_loop_control(&tmp.0), None, "resume means no command");
        std::fs::write(control_dir.join("loop-control"), "gibberish").unwrap();
        assert_eq!(read_loop_control(&tmp.0), None);
    }

    #[test]
    fn loop_control_absent_file_is_none() {
        let tmp = TempDir::new();
        assert_eq!(read_loop_control(&tmp.0), None);
    }

    #[tokio::test]
    async fn pre_tool_use_block_feeds_reason_to_model_as_tool_error() {
        let tmp = TempDir::new();
        let command = write_script(
            &tmp.0,
            "block.sh",
            "echo 'no echoing allowed' >&2\nexit 2\n",
        );
        let (registry, calls) = recording_registry();
        let (mut agent, provider) = test_agent_in(
            &tmp,
            vec![
                vec![tool_call_chunk("echo", json!({ "text": "hi" }))],
                vec![final_chunk("understood")],
            ],
            vec![hook(HookEvent::PreToolUse, Some("echo"), command)],
            registry,
        );

        let (tx, _rx) = mpsc::channel(64);
        let reason = agent.run_turn("go", tx).await.expect("turn ok");
        assert_eq!(reason, DoneReason::Completed);
        assert!(calls.lock().unwrap().is_empty(), "blocked tool never ran");

        // The block reason reached the model as an ordinary tool error.
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let feedback = requests[1]
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Tool)
            .expect("tool feedback message");
        assert!(
            feedback.content.contains("blocked by pre_tool_use hook"),
            "{}",
            feedback.content
        );
        assert!(feedback.content.contains("no echoing allowed"));
    }

    #[tokio::test]
    async fn pre_tool_use_updated_args_rewrite_the_call() {
        let tmp = TempDir::new();
        let command = write_script(
            &tmp.0,
            "rewrite.sh",
            "echo '{\"updated_args\": {\"text\": \"rewritten\"}}'\n",
        );
        let (registry, calls) = recording_registry();
        let (mut agent, _provider) = test_agent_in(
            &tmp,
            vec![
                vec![tool_call_chunk("echo", json!({ "text": "original" }))],
                vec![final_chunk("done")],
            ],
            vec![hook(HookEvent::PreToolUse, None, command)],
            registry,
        );

        let (tx, _rx) = mpsc::channel(64);
        let reason = agent.run_turn("go", tx).await.expect("turn ok");
        assert_eq!(reason, DoneReason::Completed);
        assert_eq!(
            *calls.lock().unwrap(),
            vec![json!({ "text": "rewritten" })],
            "the tool ran with the hook's arguments"
        );
    }

    #[tokio::test]
    async fn post_tool_use_stdout_is_appended_to_the_result() {
        let tmp = TempDir::new();
        let command = write_script(&tmp.0, "annotate.sh", "echo 'lint: all clean'\n");
        let (registry, calls) = recording_registry();
        let (mut agent, provider) = test_agent_in(
            &tmp,
            vec![
                vec![tool_call_chunk("echo", json!({ "text": "hi" }))],
                vec![final_chunk("done")],
            ],
            vec![hook(HookEvent::PostToolUse, Some("echo"), command)],
            registry,
        );

        let (tx, _rx) = mpsc::channel(64);
        agent.run_turn("go", tx).await.expect("turn ok");
        assert_eq!(calls.lock().unwrap().len(), 1, "the tool ran normally");

        let requests = provider.requests.lock().unwrap();
        let feedback = requests[1]
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Tool)
            .expect("tool feedback message");
        assert!(feedback.content.contains("echoed"), "{}", feedback.content);
        assert!(
            feedback.content.contains("lint: all clean"),
            "hook stdout appended: {}",
            feedback.content
        );
    }

    #[tokio::test]
    async fn user_prompt_submit_block_ends_the_turn() {
        let tmp = TempDir::new();
        let command = write_script(
            &tmp.0,
            "veto.sh",
            "echo 'not during business hours' >&2\nexit 2\n",
        );
        let (mut agent, provider) = test_agent_in(
            &tmp,
            Vec::new(), // the model must never be asked
            vec![hook(HookEvent::UserPromptSubmit, None, command)],
            ToolRegistry::new(),
        );

        let (tx, mut rx) = mpsc::channel(64);
        let reason = agent.run_turn("do the thing", tx).await.expect("turn ok");
        assert_eq!(reason, DoneReason::Stopped);
        assert!(provider.requests.lock().unwrap().is_empty());
        assert_eq!(agent.history().len(), 1, "the prompt never entered history");

        let (_text, errors, _notices) = drain_events(&mut rx);
        assert!(
            errors
                .iter()
                .any(|e| e.contains("blocked") && e.contains("not during business hours")),
            "notice emitted: {errors:?}"
        );
    }

    #[tokio::test]
    async fn user_prompt_submit_stdout_is_appended_to_the_message() {
        let tmp = TempDir::new();
        let command = write_script(&tmp.0, "context.sh", "echo 'remember: deploy is frozen'\n");
        let (mut agent, provider) = test_agent_in(
            &tmp,
            vec![vec![final_chunk("noted")]],
            vec![hook(HookEvent::UserPromptSubmit, None, command)],
            ToolRegistry::new(),
        );

        let (tx, _rx) = mpsc::channel(64);
        agent.run_turn("do the thing", tx).await.expect("turn ok");

        let requests = provider.requests.lock().unwrap();
        let prompt = requests[0].messages.last().expect("user message");
        assert_eq!(prompt.role, Role::User);
        assert!(
            prompt.content.contains("do the thing"),
            "{}",
            prompt.content
        );
        assert!(
            prompt.content.contains("remember: deploy is frozen"),
            "hook context appended: {}",
            prompt.content
        );
    }

    /// Run a turn while a reviewer task answers every [`AgentEvent::PlanReady`]
    /// with `verdict`. Returns (done reason, plans that were presented).
    async fn run_turn_with_reviewer(
        agent: &mut Agent,
        input: &str,
        verdict: PlanVerdict,
    ) -> (DoneReason, Vec<String>) {
        let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);
        let reviewer = async move {
            let mut plans = Vec::new();
            while let Some(event) = rx.recv().await {
                if let AgentEvent::PlanReady { plan, respond } = event {
                    plans.push(plan);
                    respond.send(verdict.clone()).expect("verdict delivered");
                }
            }
            plans
        };
        let (reason, plans) = tokio::join!(agent.run_turn(input, tx), reviewer);
        (reason.expect("turn ok"), plans)
    }

    /// Last tool-result message of request `index`, as fed to the model.
    fn tool_feedback_of(provider: &ScriptedProvider, index: usize) -> String {
        let requests = provider.requests.lock().unwrap();
        requests[index]
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Tool)
            .expect("tool feedback message")
            .content
            .clone()
    }

    #[tokio::test]
    async fn plan_mode_blocks_non_read_only_tools_but_the_turn_continues() {
        let tmp = TempDir::new();
        let (mut agent, provider) = test_agent_in(
            &tmp,
            vec![
                vec![tool_call_chunk(
                    "write_file",
                    json!({ "path": "a.txt", "content": "x" }),
                )],
                vec![final_chunk("understood, planning instead")],
            ],
            Vec::new(),
            ToolRegistry::with_native_tools(),
        );
        agent.set_plan_mode(true);

        let (tx, _rx) = mpsc::channel(64);
        let reason = agent.run_turn("go", tx).await.expect("turn ok");
        assert_eq!(reason, DoneReason::Completed);
        assert!(!tmp.0.join("a.txt").exists(), "the write never happened");

        let feedback = tool_feedback_of(&provider, 1);
        assert!(
            feedback.contains("blocked by plan mode"),
            "block reason fed to the model: {feedback}"
        );
        assert!(feedback.contains("exit_plan"), "{feedback}");
        assert!(agent.plan_mode(), "plan mode stays on");

        // The system prompt carried the plan-mode instructions.
        let requests = provider.requests.lock().unwrap();
        assert!(requests[0].messages[0].content.contains("PLAN MODE"));
    }

    #[tokio::test]
    async fn plan_mode_allows_read_only_tools() {
        let tmp = TempDir::new();
        std::fs::write(tmp.0.join("notes.txt"), "remember the milk\n").unwrap();
        let (mut agent, provider) = test_agent_in(
            &tmp,
            vec![
                vec![tool_call_chunk("read_file", json!({ "path": "notes.txt" }))],
                vec![final_chunk("read it")],
            ],
            Vec::new(),
            ToolRegistry::with_native_tools(),
        );
        agent.set_plan_mode(true);

        let (tx, _rx) = mpsc::channel(64);
        let reason = agent.run_turn("go", tx).await.expect("turn ok");
        assert_eq!(reason, DoneReason::Completed);
        let feedback = tool_feedback_of(&provider, 1);
        assert!(
            feedback.contains("remember the milk"),
            "read-only tools run normally: {feedback}"
        );
    }

    #[tokio::test]
    async fn plan_mode_blocks_are_exempt_from_the_identical_failure_breaker() {
        // Sovereign's breaker trips after 3 identical failures; a planning
        // model probing the same write repeatedly must not end the turn.
        let tmp = TempDir::new();
        let write = || {
            vec![tool_call_chunk(
                "write_file",
                json!({ "path": "a", "content": "x" }),
            )]
        };
        let (mut agent, _provider) = test_agent_in(
            &tmp,
            vec![
                write(),
                write(),
                write(),
                write(),
                vec![final_chunk("fine, I will plan")],
            ],
            Vec::new(),
            ToolRegistry::with_native_tools(),
        );
        agent.set_mode(Mode::Sovereign);
        agent.set_plan_mode(true);

        let (tx, _rx) = mpsc::channel(256);
        let reason = agent.run_turn("go", tx).await.expect("turn ok");
        assert_eq!(reason, DoneReason::Completed, "no circuit breaker");
    }

    #[tokio::test]
    async fn exit_plan_approval_writes_the_plan_and_clears_plan_mode() {
        let tmp = TempDir::new();
        let (mut agent, provider) = test_agent_in(
            &tmp,
            vec![
                vec![tool_call_chunk(
                    "exit_plan",
                    json!({ "plan": "# Plan\n1. do x" }),
                )],
                vec![final_chunk("executing the plan")],
            ],
            Vec::new(),
            ToolRegistry::new(),
        );
        agent.set_plan_mode(true);

        let (reason, plans) =
            run_turn_with_reviewer(&mut agent, "go", PlanVerdict::approve()).await;
        assert_eq!(reason, DoneReason::Completed);
        assert_eq!(plans, ["# Plan\n1. do x"]);
        assert!(!agent.plan_mode(), "approval clears plan mode");

        let saved =
            std::fs::read_to_string(tmp.0.join(".wizard").join("plan.md")).expect("plan persisted");
        assert_eq!(saved, "# Plan\n1. do x");

        let feedback = tool_feedback_of(&provider, 1);
        assert!(
            feedback.contains("Plan approved"),
            "the model is told to execute: {feedback}"
        );
        // After approval, the system prompt no longer carries the plan block.
        let requests = provider.requests.lock().unwrap();
        assert!(requests[0].messages[0].content.contains("PLAN MODE"));
        assert!(!requests[1].messages[0].content.contains("PLAN MODE"));
    }

    #[tokio::test]
    async fn exit_plan_rejection_keeps_plan_mode_and_feeds_back_the_feedback() {
        let tmp = TempDir::new();
        let (mut agent, provider) = test_agent_in(
            &tmp,
            vec![
                vec![tool_call_chunk("exit_plan", json!({ "plan": "# v1" }))],
                vec![final_chunk("revising the plan")],
            ],
            Vec::new(),
            ToolRegistry::new(),
        );
        agent.set_plan_mode(true);

        let (reason, plans) =
            run_turn_with_reviewer(&mut agent, "go", PlanVerdict::reject("add tests first")).await;
        assert_eq!(reason, DoneReason::Completed);
        assert_eq!(plans.len(), 1);
        assert!(agent.plan_mode(), "rejection keeps plan mode on");

        let feedback = tool_feedback_of(&provider, 1);
        assert!(feedback.starts_with("Error:"), "{feedback}");
        assert!(feedback.contains("add tests first"), "{feedback}");
        assert!(
            feedback.contains("call exit_plan again"),
            "the model is told to retry: {feedback}"
        );
    }

    #[tokio::test]
    async fn exit_plan_outside_plan_mode_is_an_error() {
        let tmp = TempDir::new();
        let (mut agent, provider) = test_agent_in(
            &tmp,
            vec![
                vec![tool_call_chunk("exit_plan", json!({ "plan": "# p" }))],
                vec![final_chunk("ok")],
            ],
            Vec::new(),
            ToolRegistry::new(),
        );

        let (tx, _rx) = mpsc::channel(64);
        let reason = agent.run_turn("go", tx).await.expect("turn ok");
        assert_eq!(reason, DoneReason::Completed);
        let feedback = tool_feedback_of(&provider, 1);
        assert!(feedback.contains("not in plan mode"), "{feedback}");
        assert!(
            !tmp.0.join(".wizard").join("plan.md").exists(),
            "no plan file written"
        );
    }

    #[tokio::test]
    async fn headless_two_phase_turn_blocks_then_plans_then_executes() {
        // The --plan shape: write blocked while planning → exit_plan
        // auto-approved → the same write succeeds in the same turn.
        let tmp = TempDir::new();
        let write_args = json!({ "path": "result.txt", "content": "done" });
        let (mut agent, provider) = test_agent_in(
            &tmp,
            vec![
                vec![tool_call_chunk("write_file", write_args.clone())],
                vec![tool_call_chunk(
                    "exit_plan",
                    json!({ "plan": "# write result.txt" }),
                )],
                vec![tool_call_chunk("write_file", write_args)],
                vec![final_chunk("all done")],
            ],
            Vec::new(),
            ToolRegistry::with_native_tools(),
        );
        agent.set_plan_mode(true);

        let (reason, plans) =
            run_turn_with_reviewer(&mut agent, "go", PlanVerdict::approve()).await;
        assert_eq!(reason, DoneReason::Completed);
        assert_eq!(plans, ["# write result.txt"]);
        assert!(!agent.plan_mode());

        // The phases happened in order: blocked, approved, executed.
        assert!(
            tool_feedback_of(&provider, 1).contains("blocked by plan mode"),
            "phase 1: the write is blocked"
        );
        assert!(
            tool_feedback_of(&provider, 2).contains("Plan approved"),
            "phase 2: the plan is approved"
        );
        let executed = tool_feedback_of(&provider, 3);
        assert!(
            !executed.contains("blocked") && !executed.starts_with("Error:"),
            "phase 3: the write succeeds: {executed}"
        );
        let written = std::fs::read_to_string(tmp.0.join("result.txt")).expect("file written");
        assert_eq!(written, "done");
    }

    #[test]
    fn exit_plan_is_always_registered() {
        let tmp = TempDir::new();
        let (mut agent, _provider) =
            test_agent_in(&tmp, Vec::new(), Vec::new(), ToolRegistry::new());
        let has_exit_plan = |agent: &Agent| {
            agent
                .dispatcher
                .registry()
                .get(crate::tools::plan::EXIT_PLAN_TOOL_NAME)
                .is_some()
        };
        assert!(has_exit_plan(&agent), "registered at construction");
        // A registry swap (/reload, /evolve) re-registers it.
        agent.set_registry(ToolRegistry::new());
        assert!(has_exit_plan(&agent), "re-registered after set_registry");
    }

    #[tokio::test]
    async fn rewind_restores_an_overwritten_file_and_truncates_history() {
        let tmp = TempDir::new();
        let file = tmp.0.join("notes.txt");
        std::fs::write(&file, "before").unwrap();
        let (mut agent, _provider) = test_agent_in(
            &tmp,
            vec![
                vec![tool_call_chunk(
                    "write_file",
                    json!({ "path": "notes.txt", "content": "after" }),
                )],
                vec![final_chunk("overwritten")],
            ],
            Vec::new(),
            ToolRegistry::with_native_tools(),
        );

        let (tx, mut rx) = mpsc::channel(256);
        let reason = agent.run_turn("overwrite notes.txt", tx).await.unwrap();
        drain_events(&mut rx);
        assert_eq!(reason, DoneReason::Completed);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "after");
        assert!(agent.history().len() > 1);

        let candidates = agent.rewind_candidates(10);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].prompt, "overwrite notes.txt");
        assert_eq!(candidates[0].files, vec![file.clone()]);

        let restored = agent.rewind_to(candidates[0].turn).unwrap();
        assert_eq!(restored, vec![file.clone()]);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "before");
        assert_eq!(
            agent.history().len(),
            1,
            "only the system prompt survives a full rewind"
        );
        assert!(
            agent.session().load_messages().unwrap().is_empty(),
            "the session file was truncated"
        );
    }

    #[tokio::test]
    async fn rewind_deletes_a_file_the_turn_created() {
        let tmp = TempDir::new();
        let file = tmp.0.join("created.txt");
        let (mut agent, _provider) = test_agent_in(
            &tmp,
            vec![
                vec![tool_call_chunk(
                    "write_file",
                    json!({ "path": "created.txt", "content": "fresh" }),
                )],
                vec![final_chunk("created")],
            ],
            Vec::new(),
            ToolRegistry::with_native_tools(),
        );

        let (tx, mut rx) = mpsc::channel(256);
        agent.run_turn("create created.txt", tx).await.unwrap();
        drain_events(&mut rx);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "fresh");

        let candidates = agent.rewind_candidates(10);
        assert_eq!(candidates.len(), 1);
        agent.rewind_to(candidates[0].turn).unwrap();
        assert!(!file.exists(), "rewind deletes a file that did not exist");
    }

    #[tokio::test]
    async fn rewind_to_a_later_turn_keeps_earlier_turns() {
        let tmp = TempDir::new();
        let file = tmp.0.join("notes.txt");
        std::fs::write(&file, "v0").unwrap();
        let (mut agent, _provider) = test_agent_in(
            &tmp,
            vec![
                vec![tool_call_chunk(
                    "write_file",
                    json!({ "path": "notes.txt", "content": "v1" }),
                )],
                vec![final_chunk("first done")],
                vec![tool_call_chunk(
                    "write_file",
                    json!({ "path": "notes.txt", "content": "v2" }),
                )],
                vec![final_chunk("second done")],
            ],
            Vec::new(),
            ToolRegistry::with_native_tools(),
        );

        let (tx, mut rx) = mpsc::channel(256);
        agent.run_turn("write v1", tx.clone()).await.unwrap();
        agent.run_turn("write v2", tx).await.unwrap();
        drain_events(&mut rx);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v2");

        let candidates = agent.rewind_candidates(10);
        assert_eq!(candidates.len(), 2, "newest first");
        assert!(candidates[0].turn > candidates[1].turn);

        agent.rewind_to(candidates[0].turn).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v1");
        let messages = agent.session().load_messages().unwrap();
        assert_eq!(
            messages.first().map(|m| m.content.as_str()),
            Some("write v1"),
            "the first turn's history survives"
        );
        assert!(
            messages.iter().all(|m| m.content != "write v2"),
            "the second turn's history is gone"
        );
    }

    #[tokio::test]
    async fn rollback_failed_cycle_restores_files_and_notes_the_mission() {
        let tmp = TempDir::new();
        let file = tmp.0.join("data.txt");
        std::fs::write(&file, "good").unwrap();
        let (mut agent, _provider) = test_agent_in(
            &tmp,
            vec![
                vec![tool_call_chunk(
                    "write_file",
                    json!({ "path": "data.txt", "content": "broken" }),
                )],
                vec![final_chunk("changed it")],
            ],
            Vec::new(),
            ToolRegistry::with_native_tools(),
        );

        let cycle_first_turn = agent.checkpoints().current_turn() + 1;
        let (tx, mut rx) = mpsc::channel(256);
        agent.run_turn("break the data", tx).await.unwrap();
        drain_events(&mut rx);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "broken");

        let spinner = crate::progress::TurnSpinner::new();
        let mut mission = mission::Mission::new("keep the data good");

        // Disabled: a no-op.
        let config = Config::default();
        rollback_failed_cycle(
            &config,
            &agent,
            Some(&mut mission),
            &tmp.0,
            cycle_first_turn,
            "circuit breaker",
            Some(&spinner),
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "broken");
        assert!(mission.notes.is_empty());

        // Enabled: the cycle's edits are restored and the mission notes it.
        let config = Config {
            rollback_failed_cycles: true,
            ..Config::default()
        };
        rollback_failed_cycle(
            &config,
            &agent,
            Some(&mut mission),
            &tmp.0,
            cycle_first_turn,
            "circuit breaker",
            Some(&spinner),
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "good");
        assert!(
            mission
                .notes
                .last()
                .is_some_and(|note| note.contains("rolled back 1 file(s)")
                    && note.contains("circuit breaker")),
            "rollback noted in the mission: {:?}",
            mission.notes
        );
        // The note was persisted to mission.toml.
        let loaded = mission::Mission::load(&tmp.0).unwrap().expect("saved");
        assert_eq!(loaded.notes, mission.notes);
    }

    #[tokio::test]
    async fn usage_counts_accumulate_emit_events_and_land_in_the_jsonl_log() {
        let tmp = TempDir::new();
        let provider = ScriptedProvider::new(vec![
            vec![usage_chunk("first", 100, 20)],
            vec![usage_chunk("second", 150, 30)],
        ]);
        let mut agent =
            test_agent_with(&tmp, Arc::clone(&provider), Vec::new(), ToolRegistry::new());

        let (tx, mut rx) = mpsc::channel(64);
        agent.run_turn("one", tx).await.expect("turn ok");
        let mut usage_events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let AgentEvent::Usage {
                prompt_tokens,
                completion_tokens,
            } = event
            {
                usage_events.push((prompt_tokens, completion_tokens));
            }
        }
        assert_eq!(usage_events, [(100, 20)], "one Usage event per model call");

        let (tx, _rx) = mpsc::channel(64);
        agent.run_turn("two", tx).await.expect("turn ok");

        assert_eq!(agent.usage().session_totals(), (250, 50));
        assert_eq!(agent.usage().turn_totals(), (150, 30), "last turn only");
        assert_eq!(agent.usage().last_prompt_tokens(), Some(150));

        // One JSONL record per turn, in order.
        let raw = std::fs::read_to_string(tmp.0.join("usage.jsonl")).expect("log written");
        let records: Vec<crate::usage::UsageRecord> = raw
            .lines()
            .map(|line| serde_json::from_str(line).expect("valid json"))
            .collect();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].prompt_tokens, 100);
        assert_eq!(records[0].completion_tokens, 20);
        assert_eq!(records[1].prompt_tokens, 150);
        assert_eq!(records[1].completion_tokens, 30);
        assert_eq!(records[0].mode, "genie");
        assert!(records[0].ts > 0);
        assert_eq!(records[0].project, tmp.0.display().to_string());
    }

    #[tokio::test]
    async fn turns_without_reported_counts_write_no_usage_records() {
        let (mut agent, _provider, tmp) = test_agent(vec![vec![final_chunk("plain")]]);
        let (tx, _rx) = mpsc::channel(64);
        agent.run_turn("go", tx).await.expect("turn ok");
        assert_eq!(agent.usage().session_totals(), (0, 0));
        assert!(!tmp.0.join("usage.jsonl").exists(), "no counts, no log");
    }

    #[tokio::test]
    async fn prompt_tokens_near_the_context_window_trigger_compaction() {
        let tmp = TempDir::new();
        // Window 1000 → compaction at >800 prompt tokens. The byte threshold
        // (48k) is never reached: the messages are tiny.
        let provider = ScriptedProvider::with_context_window(
            vec![
                // Turn 1: reports a prompt size of 900 tokens.
                vec![usage_chunk("ok", 900, 10)],
                // Turn 2: the compaction summary, then the actual reply.
                vec![final_chunk("progress so far")],
                vec![final_chunk("done")],
            ],
            1000,
        );
        let mut agent =
            test_agent_with(&tmp, Arc::clone(&provider), Vec::new(), ToolRegistry::new());
        for i in 0..14 {
            agent.history.push(ChatMessage::user(format!("filler {i}")));
        }

        let (tx, _rx) = mpsc::channel(64);
        agent.run_turn("one", tx).await.expect("turn ok");
        assert!(
            !agent
                .history
                .iter()
                .any(|m| m.content.contains("[Compacted progress summary]")),
            "no compaction before a token count arrives"
        );

        let (tx, mut rx) = mpsc::channel(64);
        agent.run_turn("two", tx).await.expect("turn ok");
        assert!(
            agent
                .history
                .iter()
                .any(|m| m.content.contains("[Compacted progress summary]")),
            "token threshold compacted the history"
        );
        let (_text, errors, notices) = drain_events(&mut rx);
        assert!(errors.is_empty(), "a successful compaction is not an error");
        assert!(
            notices.iter().any(|n| n.contains("compacted")),
            "compaction surfaced: {notices:?}"
        );
        assert_eq!(
            agent.usage().last_prompt_tokens(),
            None,
            "stale prompt size cleared so compaction does not re-trigger"
        );

        // The summarization request carried the extended preservation
        // instructions (todo list + plan file).
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        let summarize_prompt = &requests[1].messages[0].content;
        assert!(summarize_prompt.contains("todo"), "{summarize_prompt}");
        assert!(
            summarize_prompt.contains(".wizard/plan.md"),
            "{summarize_prompt}"
        );
    }

    /// Test tool returning a large fixed blob, to cross the byte threshold
    /// mid-turn.
    struct BigOutputTool;

    #[async_trait::async_trait]
    impl crate::tools::Tool for BigOutputTool {
        fn name(&self) -> &str {
            "big"
        }

        fn description(&self) -> &str {
            "Return a large blob (test tool)."
        }

        fn parameters(&self) -> Value {
            json!({ "type": "object", "properties": {} })
        }

        async fn execute(
            &self,
            _args: Value,
            _ctx: &ToolContext,
        ) -> Result<ToolOutput, crate::tools::ToolError> {
            Ok(ToolOutput::ok("B".repeat(5_000)))
        }
    }

    #[tokio::test]
    async fn compact_now_force_summarizes_and_keeps_the_recent_tail() {
        // One scripted response: the summarization call.
        let (mut agent, _provider, _tmp) =
            test_agent(vec![vec![final_chunk("a terse progress note")]]);
        // history[0] is the system prompt; add a middle span + recent tail.
        let extra = KEEP_RECENT + 5;
        for i in 0..extra {
            agent.history.push(ChatMessage::user(format!("msg {i}")));
        }
        let before = agent.history.len();

        let outcome = agent.compact_now().await;

        assert_eq!(
            outcome,
            CompactOutcome::Summarized(before - 1 - KEEP_RECENT)
        );
        assert!(
            agent
                .history
                .iter()
                .any(|m| m.content.contains("[Compacted progress summary]")),
            "the middle span became a summary note"
        );
        // The system prompt and the last KEEP_RECENT messages survive verbatim.
        assert_eq!(agent.history[0].role, Role::System);
        assert_eq!(
            agent.history.last().unwrap().content,
            format!("msg {}", extra - 1)
        );
    }

    #[tokio::test]
    async fn compact_now_is_a_noop_with_little_history() {
        let (mut agent, _provider, _tmp) = test_agent(vec![]);
        // Only the system prompt plus a couple messages: nothing to compact.
        agent.history.push(ChatMessage::user("hi"));
        let outcome = agent.compact_now().await;
        assert_eq!(outcome, CompactOutcome::Nothing);
    }

    #[tokio::test]
    async fn byte_threshold_compacts_between_steps_keeping_the_turn_tail() {
        let tmp = TempDir::new();
        let provider = ScriptedProvider::new(vec![
            vec![tool_call_chunk("big", json!({}))],
            vec![final_chunk("progress so far")], // compaction summary
            vec![final_chunk("done")],
        ]);
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(BigOutputTool));
        let mut agent = test_agent_with(&tmp, Arc::clone(&provider), Vec::new(), registry);
        for i in 0..13 {
            agent.history.push(ChatMessage::user(format!("filler {i}")));
        }
        // Threshold just above the current size: crossed only once the 5k
        // tool result lands, so the compaction must happen mid-turn.
        let base: usize = agent.history.iter().map(|m| m.content.len()).sum();
        agent.config.compact_threshold_bytes = base + 1_000;

        let (tx, _rx) = mpsc::channel(256);
        let reason = agent.run_turn("go", tx).await.expect("turn ok");
        assert_eq!(reason, DoneReason::Completed);

        assert!(
            agent
                .history
                .iter()
                .any(|m| m.content.contains("[Compacted progress summary]")),
            "mid-turn compaction happened"
        );
        // The in-flight turn's tail — the big tool result the model is
        // reasoning about — survived verbatim.
        assert!(
            agent
                .history
                .iter()
                .any(|m| m.role == Role::Tool && m.content.contains("BBBB")),
            "tool feedback preserved through compaction"
        );
        // The final completion saw the compacted history.
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(
            requests[2]
                .messages
                .iter()
                .any(|m| m.content.contains("[Compacted progress summary]"))
        );
    }

    #[tokio::test]
    async fn todo_writes_update_shared_state_and_emit_events() {
        let tmp = TempDir::new();
        let items = json!([
            { "content": "investigate", "status": "completed" },
            { "content": "implement", "status": "in_progress" },
            { "content": "test", "status": "pending" }
        ]);
        let (mut agent, provider) = test_agent_in(
            &tmp,
            vec![
                vec![tool_call_chunk(
                    "todo",
                    json!({ "action": "write", "items": items }),
                )],
                vec![final_chunk("noted")],
            ],
            Vec::new(),
            ToolRegistry::with_native_tools(),
        );

        let (tx, mut rx) = mpsc::channel(64);
        agent.run_turn("go", tx).await.expect("turn ok");

        let mut updates = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let AgentEvent::TodoUpdated(items) = event {
                updates.push(items);
            }
        }
        assert_eq!(updates.len(), 1, "one TodoUpdated per write");
        assert_eq!(updates[0].len(), 3);
        assert_eq!(updates[0][1].content, "implement");
        assert_eq!(
            updates[0][1].status,
            crate::tools::todo::TodoStatus::InProgress
        );

        // The shared state holds the list for later read calls.
        assert_eq!(agent.ctx.todos.lock().unwrap().len(), 3);
        let feedback = tool_feedback_of(&provider, 1);
        assert!(feedback.contains("1/3 done"), "{feedback}");
    }

    #[tokio::test]
    async fn todo_tool_stays_usable_in_plan_mode() {
        let tmp = TempDir::new();
        let (mut agent, provider) = test_agent_in(
            &tmp,
            vec![
                vec![tool_call_chunk(
                    "todo",
                    json!({ "action": "write", "items": [
                        { "content": "draft plan", "status": "in_progress" }
                    ] }),
                )],
                vec![final_chunk("planning")],
            ],
            Vec::new(),
            ToolRegistry::with_native_tools(),
        );
        agent.set_plan_mode(true);

        let (tx, _rx) = mpsc::channel(64);
        agent.run_turn("go", tx).await.expect("turn ok");
        let feedback = tool_feedback_of(&provider, 1);
        assert!(
            feedback.contains("todo list updated"),
            "todo runs under the plan gate: {feedback}"
        );
        assert!(agent.plan_mode(), "plan mode untouched");
    }

    #[test]
    fn todo_instruction_appears_only_when_the_tool_is_registered() {
        let tmp = TempDir::new();
        let (agent, _provider) = test_agent_in(
            &tmp,
            Vec::new(),
            Vec::new(),
            ToolRegistry::with_native_tools(),
        );
        assert!(agent.history[0].content.contains("## Working todo list"));

        let (agent, _provider) = test_agent_in(&tmp, Vec::new(), Vec::new(), ToolRegistry::new());
        assert!(
            !agent.history[0].content.contains("## Working todo list"),
            "no instruction without the tool"
        );
    }

    #[tokio::test]
    async fn background_task_finish_is_injected_into_the_next_step() {
        use crate::tools::tasks::TaskStatus;

        let tmp = TempDir::new();
        let (mut agent, provider) = test_agent_in(
            &tmp,
            vec![
                // Turn 1: start a background task, then stop.
                vec![tool_call_chunk(
                    "execute",
                    json!({ "command": "echo task-payload", "run_in_background": true }),
                )],
                vec![final_chunk("started it")],
                // Turn 2: plain reply (the notification precedes it).
                vec![final_chunk("noted the finish")],
            ],
            Vec::new(),
            ToolRegistry::with_native_tools(),
        );

        let (tx, _rx) = mpsc::channel(64);
        agent
            .run_turn("run it in the background", tx)
            .await
            .expect("turn ok");
        let feedback = tool_feedback_of(&provider, 1);
        assert!(
            feedback.contains("Background task #1 started: echo task-payload"),
            "spawn returns immediately with the id: {feedback}"
        );

        // Wait for the echo to actually finish in the registry.
        let deadline = Instant::now() + Duration::from_secs(10);
        while agent.ctx.tasks.status(1) == Some(TaskStatus::Running) {
            assert!(Instant::now() < deadline, "background task finished");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let (tx, mut rx) = mpsc::channel(64);
        agent.run_turn("anything new?", tx).await.expect("turn ok");

        // The next step's request carried the finished-task notification
        // (with the output tail) ahead of the model call.
        {
            let requests = provider.requests.lock().unwrap();
            let request = requests.last().expect("turn 2 request");
            let note = request
                .messages
                .iter()
                .find(|m| {
                    m.role == Role::System && m.content.contains("background task #1 finished")
                })
                .expect("notification in history");
            assert!(note.content.contains("(exit 0)"), "{}", note.content);
            assert!(
                note.content.contains("task-payload"),
                "output tail included: {}",
                note.content
            );
        }

        // The surfaces saw a TaskFinished event.
        let mut finished = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let AgentEvent::TaskFinished {
                id,
                command,
                status,
            } = event
            {
                finished.push((id, command, status));
            }
        }
        assert_eq!(
            finished,
            [(1, "echo task-payload".to_string(), TaskStatus::Done(0))]
        );

        // Drained exactly once: nothing left for later steps.
        assert!(agent.ctx.tasks.drain_completed().is_empty());
        assert_eq!(
            agent
                .history()
                .iter()
                .filter(|m| m.content.contains("background task #1 finished"))
                .count(),
            1,
            "the notification appears exactly once in history"
        );
    }

    #[tokio::test]
    async fn hook_timeout_does_not_hang_the_turn() {
        let tmp = TempDir::new();
        let command = write_script(&tmp.0, "slow.sh", "sleep 5\n");
        let (registry, calls) = recording_registry();
        let (mut agent, _provider) = test_agent_in(
            &tmp,
            vec![
                vec![tool_call_chunk("echo", json!({}))],
                vec![final_chunk("done")],
            ],
            vec![HookDef {
                event: HookEvent::PreToolUse,
                matcher: None,
                command,
                timeout_secs: Some(1),
            }],
            registry,
        );

        let started = Instant::now();
        let (tx, _rx) = mpsc::channel(64);
        let reason = agent.run_turn("go", tx).await.expect("turn ok");
        assert_eq!(reason, DoneReason::Completed);
        assert_eq!(calls.lock().unwrap().len(), 1, "the tool still ran");
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "the hook was killed at its 1s timeout (took {:?})",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn stream_json_sink_renders_a_scripted_run_as_jsonl_ending_in_done() {
        use crate::output::{EventSink, StreamJsonSink, tests::SharedBuf};

        let tmp = TempDir::new();
        let (registry, _calls) = recording_registry();
        let (mut agent, _provider) = test_agent_in(
            &tmp,
            vec![
                vec![tool_call_chunk("echo", json!({ "text": "hi" }))],
                vec![usage_chunk("all wrapped up", 42, 7)],
            ],
            Vec::new(),
            registry,
        );

        let (tx, mut rx) = mpsc::channel(256);
        let reason = agent.run_turn("go", tx).await.expect("turn ok");
        assert_eq!(reason, DoneReason::Completed);

        // Feed the turn's real event stream through the stream-json sink,
        // exactly as run_headless wires it.
        let buf = SharedBuf::default();
        let mut sink = StreamJsonSink::new(buf.clone());
        while let Ok(event) = rx.try_recv() {
            sink.event(event);
        }
        sink.finish(reason);

        let out = buf.contents();
        let values: Vec<serde_json::Value> = out
            .lines()
            .map(|line| serde_json::from_str(line).expect("every line is valid JSON"))
            .collect();
        assert!(values.len() >= 4, "got: {out}");
        let types: Vec<&str> = values
            .iter()
            .filter_map(|value| value["type"].as_str())
            .collect();
        assert!(types.contains(&"tool_call"), "got: {types:?}");
        assert!(types.contains(&"tool_result"), "got: {types:?}");
        assert!(types.contains(&"text_delta"), "got: {types:?}");
        assert!(types.contains(&"usage"), "got: {types:?}");
        let done = values.last().expect("at least the done line");
        assert_eq!(done["type"], "done");
        assert_eq!(done["reason"], "completed");
        assert_eq!(done["usage"]["prompt_tokens"], 42);
        assert_eq!(done["usage"]["completion_tokens"], 7);
    }

    #[tokio::test]
    async fn spawn_subagent_background_returns_immediately_and_reports_on_a_later_turn() {
        let tmp = TempDir::new();

        // The subagent gets its own scripted provider so its chat_stream
        // calls can't race the parent's — they're decoupled queues.
        let sub_provider = ScriptedProvider::new(vec![vec![final_chunk("found the answer")]]);
        let sub_hooks = Arc::new(HookEngine::new(
            Vec::new(),
            tmp.0.clone(),
            "sub-session".to_string(),
        ));
        let spawn_tool = subagent::SpawnSubagentTool::new(
            subagent::builtin_configs(),
            sub_provider,
            Arc::new(ToolRegistry::new()),
            sub_hooks,
        );
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(spawn_tool));

        let (mut agent, provider) = test_agent_in(
            &tmp,
            vec![
                vec![tool_call_chunk(
                    "spawn_subagent",
                    json!({"subagent": "worker", "task": "investigate X", "background": true}),
                )],
                vec![final_chunk("kicked it off, anything else?")],
                // Second turn's response, below.
                vec![final_chunk("got it")],
            ],
            Vec::new(),
            registry,
        );

        let (tx, mut rx) = mpsc::channel(64);
        let reason = agent.run_turn("delegate this", tx).await.expect("turn ok");
        assert_eq!(reason, DoneReason::Completed);

        // The turn did not wait on the subagent: both of the parent's
        // scripted responses were already consumed.
        assert_eq!(provider.requests.lock().unwrap().len(), 2);

        let mut started = None;
        let mut tool_result = None;
        while let Ok(event) = rx.try_recv() {
            match event {
                AgentEvent::SubagentStarted { id, name, task } => {
                    started = Some((id, name, task));
                }
                AgentEvent::ToolFinished { name, output } if name == "spawn_subagent" => {
                    tool_result = Some(output);
                }
                _ => {}
            }
        }
        let (id, name, task) = started.expect("SubagentStarted was emitted");
        assert_eq!(id, 1);
        assert_eq!(name, "worker");
        assert_eq!(task, "investigate X");
        let tool_result = tool_result.expect("spawn_subagent's tool result was observed");
        assert!(!tool_result.is_error);
        assert!(
            tool_result.content.contains("Running in the background"),
            "{}",
            tool_result.content
        );

        // Let the detached subagent actually finish before the next turn.
        let deadline = Instant::now() + Duration::from_secs(10);
        while agent.ctx.subagents.pending_count() > 0 {
            assert!(
                Instant::now() < deadline,
                "background subagent did not finish in time"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // A follow-up turn's top-of-loop drain picks up the report: it's
        // injected into history and surfaced as SubagentFinished, without
        // the model ever having to ask for it.
        let (tx2, mut rx2) = mpsc::channel(64);
        agent
            .run_turn("anything happen?", tx2)
            .await
            .expect("second turn ok");

        let mut finished = None;
        while let Ok(event) = rx2.try_recv() {
            if let AgentEvent::SubagentFinished {
                id,
                name,
                completed,
                output,
                ..
            } = event
            {
                finished = Some((id, name, completed, output));
            }
        }
        let (id, name, completed, output) = finished.expect("SubagentFinished was emitted");
        assert_eq!(id, 1);
        assert_eq!(name, "worker");
        assert!(completed);
        assert_eq!(output, "found the answer");

        assert_eq!(
            agent
                .history()
                .iter()
                .filter(|m| m
                    .content
                    .contains("background subagent #1 'worker' completed"))
                .count(),
            1,
            "the report appears exactly once in history"
        );
    }

    #[test]
    fn error_classification_prefers_typed_provider_errors() {
        let permanent: anyhow::Error = crate::llm::ProviderError::http(401, "bad key").into();
        assert!(!error_is_transient(&permanent));
        let rate_limited: anyhow::Error = crate::llm::ProviderError::http(429, "slow down").into();
        assert!(error_is_transient(&rate_limited));
        let server: anyhow::Error = crate::llm::ProviderError::http(500, "oops").into();
        assert!(error_is_transient(&server));
        let transport: anyhow::Error = crate::llm::ProviderError::transport("reset").into();
        assert!(error_is_transient(&transport));
        // Context wrapping must not hide the classification.
        let wrapped = permanent.context("starting chat completion");
        assert!(!error_is_transient(&wrapped));
        // Unknown errors stay transient for robustness.
        assert!(error_is_transient(&anyhow::anyhow!("mid-stream drop")));
    }

    #[test]
    fn failed_background_subagents_are_labeled_failed() {
        let note = subagent_note(&crate::tools::subagent_tasks::SubagentTaskResult {
            id: 3,
            name: "worker".to_string(),
            task: "doomed".to_string(),
            completed: false,
            output: "subagent failed: connection refused".to_string(),
            steps_used: 0,
            error: Some("connection refused".to_string()),
        });
        assert!(
            note.contains("'worker' failed: connection refused after 0 step(s)"),
            "{note}"
        );
        assert!(
            !note.contains("step budget"),
            "a hard error is not a budget stop: {note}"
        );
    }

    /// Test tool that fires the agent's cancel handle when executed, to
    /// exercise mid-batch interruption deterministically.
    struct CancelingTool {
        handle: Arc<Mutex<Option<CancelHandle>>>,
    }

    #[async_trait::async_trait]
    impl crate::tools::Tool for CancelingTool {
        fn name(&self) -> &str {
            "cancel_me"
        }

        fn description(&self) -> &str {
            "Cancel the turn (test tool)."
        }

        fn parameters(&self) -> Value {
            json!({ "type": "object", "properties": {} })
        }

        async fn execute(
            &self,
            _args: Value,
            _ctx: &ToolContext,
        ) -> Result<ToolOutput, crate::tools::ToolError> {
            self.handle
                .lock()
                .unwrap()
                .as_ref()
                .expect("handle bound")
                .cancel();
            Ok(ToolOutput::ok("cancelling"))
        }
    }

    /// `done: true` chunk carrying several tool calls in one batch.
    fn multi_tool_chunk(names: &[&str]) -> ChatChunk {
        ChatChunk {
            message: Some(ChatMessage {
                role: Role::Assistant,
                content: String::new(),
                tool_calls: names
                    .iter()
                    .map(|name| ToolCall {
                        function: FunctionCall {
                            name: name.to_string(),
                            arguments: json!({}),
                        },
                    })
                    .collect(),
                tool_name: None,
            }),
            thinking: false,
            done: true,
            done_reason: None,
            eval_count: None,
            prompt_eval_count: None,
        }
    }

    #[tokio::test]
    async fn cancel_mid_batch_stops_the_turn_and_answers_skipped_calls() {
        let tmp = TempDir::new();
        let handle_slot = Arc::new(Mutex::new(None));
        let (mut registry, echo_calls) = recording_registry();
        registry.register(Arc::new(CancelingTool {
            handle: Arc::clone(&handle_slot),
        }));
        let (mut agent, provider) = test_agent_in(
            &tmp,
            // One completion: a two-call batch. The turn must stop before
            // asking for another.
            vec![vec![multi_tool_chunk(&["cancel_me", "echo"])]],
            Vec::new(),
            registry,
        );
        *handle_slot.lock().unwrap() = Some(agent.cancel_handle());

        let (tx, _rx) = mpsc::channel(64);
        let reason = agent.run_turn("go", tx).await.expect("turn ok");
        assert_eq!(reason, DoneReason::Stopped);
        assert_eq!(provider.requests.lock().unwrap().len(), 1);
        assert!(
            echo_calls.lock().unwrap().is_empty(),
            "the call after the cancel never ran"
        );

        // Both tool calls are answered — the second synthetically — so the
        // persisted history carries no dangling tool_use.
        let persisted = agent.session().load_history().expect("session readable");
        let assistant = persisted
            .iter()
            .position(|m| m.role == Role::Assistant && m.tool_calls.len() == 2)
            .expect("assistant batch persisted");
        assert_eq!(persisted[assistant + 1].role, Role::Tool);
        assert_eq!(persisted[assistant + 2].role, Role::Tool);
        assert_eq!(persisted[assistant + 2].tool_name.as_deref(), Some("echo"));
        assert!(
            persisted[assistant + 2]
                .content
                .contains("interrupted by user"),
            "{}",
            persisted[assistant + 2].content
        );

        // The next turn is not poisoned by the stale cancel request.
        assert!(agent.cancel_handle().is_cancelled());
        agent.cancel.clear();
        assert!(!agent.cancel_handle().is_cancelled());
    }

    #[tokio::test]
    async fn compaction_never_splits_a_tool_call_group() {
        // One scripted response: the summarization call.
        let (mut agent, _provider, _tmp) =
            test_agent(vec![vec![final_chunk("a terse progress note")]]);
        // Arrange history so the naive cut (len - KEEP_RECENT) would land on
        // a tool result, splitting it from its assistant tool-call message.
        for i in 0..4 {
            agent.history.push(ChatMessage::user(format!("filler {i}")));
        }
        let mut assistant = ChatMessage::assistant("running a tool");
        assistant.tool_calls.push(ToolCall {
            function: FunctionCall {
                name: "execute".to_string(),
                arguments: json!({}),
            },
        });
        agent.history.push(assistant); // index 5
        agent
            .history
            .push(ChatMessage::tool_result("execute", "output")); // index 6
        for i in 0..9 {
            agent.history.push(ChatMessage::user(format!("tail {i}")));
        }
        assert_eq!(agent.history.len(), 16, "naive cut would be index 6");

        let outcome = agent.compact_now().await;
        // Snapped back to the user message at index 4: only 3 messages went.
        assert_eq!(outcome, CompactOutcome::Summarized(3));
        let assistant = agent
            .history
            .iter()
            .position(|m| !m.tool_calls.is_empty())
            .expect("tool-call message survived");
        assert_eq!(
            agent.history[assistant + 1].role,
            Role::Tool,
            "the tool call kept its result"
        );
    }

    #[tokio::test]
    async fn drain_finished_notifications_reports_and_persists_once() {
        let (mut agent, _provider, _tmp) = test_agent(vec![]);
        agent.ctx.subagents.spawn("worker", "doomed", async {
            crate::tools::subagent_tasks::SubagentRunResult {
                completed: false,
                output: "subagent failed: boom".to_string(),
                steps_used: 0,
                error: Some("boom".to_string()),
            }
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while agent.ctx.subagents.pending_count() > 0 {
            assert!(Instant::now() < deadline, "background subagent finished");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let notifications = agent.drain_finished_notifications();
        assert_eq!(notifications.len(), 1);
        match &notifications[0] {
            FinishedNotification::Subagent(task) => {
                assert_eq!(task.error.as_deref(), Some("boom"));
            }
            other => panic!("expected a subagent notification, got {other:?}"),
        }
        let note = agent.history().last().expect("note in history");
        assert_eq!(note.role, Role::System);
        assert!(note.content.contains("failed: boom"), "{}", note.content);

        // Persisted as a system note: a resume replays it.
        let replayed = agent.session().load_history().expect("session readable");
        assert!(
            replayed
                .iter()
                .any(|m| m.role == Role::System && m.content.contains("failed: boom")),
            "note persisted for resume"
        );

        assert!(
            agent.drain_finished_notifications().is_empty(),
            "each finish is reported exactly once"
        );
    }

    #[tokio::test]
    async fn clear_kills_background_work_and_resets_todos() {
        let (mut agent, _provider, _tmp) = test_agent(vec![]);
        agent
            .ctx
            .todos
            .lock()
            .unwrap()
            .push(crate::tools::todo::TodoItem {
                content: "stale item".to_string(),
                status: crate::tools::todo::TodoStatus::Pending,
            });
        agent.ctx.subagents.spawn("worker", "slow", async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            crate::tools::subagent_tasks::SubagentRunResult {
                completed: true,
                output: "never".to_string(),
                steps_used: 1,
                error: None,
            }
        });
        let old_session = agent.session().path().to_path_buf();

        agent.clear().expect("clear ok");
        let new_session_path = agent.session().path().to_path_buf();

        assert_ne!(new_session_path, old_session, "fresh session file");
        assert!(agent.ctx.todos.lock().unwrap().is_empty(), "todos reset");
        assert_eq!(agent.ctx.subagents.pending_count(), 0);
        assert!(
            agent.ctx.subagents.list().is_empty(),
            "old subagents detached"
        );
        assert!(agent.ctx.tasks.list().is_empty(), "old tasks detached");
        assert!(
            agent.drain_finished_notifications().is_empty(),
            "nothing from the old conversation leaks into the new one"
        );

        // The real sessions dir was touched: clean up the empty file.
        let _ = std::fs::remove_file(new_session_path);
    }

    /// A one-lens, no-judge ultra engine. One candidate makes the scripted
    /// provider's queue deterministic: the pre-phase takes exactly one response
    /// (the draft), the main loop the next.
    fn ultra_engine() -> Arc<ultra::UltraEngine> {
        Arc::new(ultra::UltraEngine {
            lenses: vec![subagent::SubagentConfig {
                name: "implementer".to_string(),
                description: "drafts".to_string(),
                system_prompt: "draft it".to_string(),
                tool_scope: None,
                max_steps: 1,
            }],
            judge: ultra::builtin_judge(),
            judges: 0,
            timeout: Duration::from_secs(30),
            max_draft_chars: 6_000,
        })
    }

    #[tokio::test]
    async fn ultra_guidance_lives_for_one_turn_and_is_never_persisted() {
        let tmp = TempDir::new();
        let provider = ScriptedProvider::new(vec![
            vec![final_chunk("draft: rename the flag in cli.rs")], // turn 1 candidate
            vec![final_chunk("renamed it")],                       // turn 1 main loop
            vec![final_chunk("draft: and the docs too")],          // turn 2 candidate
            vec![final_chunk("done")],                             // turn 2 main loop
        ]);
        let mut agent = test_agent_with(&tmp, provider.clone(), Vec::new(), ToolRegistry::new());
        agent.set_ultra(Some(ultra_engine()));

        let (tx, mut rx) = mpsc::channel(64);
        agent.run_turn("rename the flag", tx.clone()).await.unwrap();

        // The drafts reached the model that acts on them...
        let main_turn = provider.requests.lock().unwrap()[1].clone();
        let injected: Vec<&ChatMessage> = main_turn
            .messages
            .iter()
            .filter(|message| ultra::is_guidance(message))
            .collect();
        assert_eq!(injected.len(), 1, "exactly one guidance block");
        assert!(injected[0].content.contains("rename the flag in cli.rs"));

        // ...and the surface got them too, or they would be readable nowhere:
        // the candidate's pane retires within seconds and a system message is
        // never rendered in the transcript.
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        assert!(
            events.iter().any(|event| matches!(
                event,
                AgentEvent::UltraGuidance { guidance, .. }
                    if guidance.contains("rename the flag in cli.rs")
            )),
            "the drafts are surfaced for the user to keep"
        );

        // The turn is over, so the advice about it is over.
        assert!(
            !agent.history.iter().any(ultra::is_guidance),
            "guidance is turn-scoped: left in, one block per ultra turn accumulates in the window \
             and every later turn re-sends drafts about requests that were already answered"
        );
        assert!(
            !agent
                .session()
                .load_messages()
                .expect("session loads")
                .iter()
                .any(ultra::is_guidance),
            "and it is not in the session either, so /resume does not bring it back"
        );

        // A second ultra turn sees its own drafts and none of the last turn's.
        agent.run_turn("now the docs", tx).await.unwrap();
        let second_turn = provider.requests.lock().unwrap()[3].clone();
        let injected: Vec<&ChatMessage> = second_turn
            .messages
            .iter()
            .filter(|message| ultra::is_guidance(message))
            .collect();
        assert_eq!(injected.len(), 1, "still exactly one, not two");
        assert!(injected[0].content.contains("and the docs too"));
        assert!(
            !injected[0].content.contains("rename the flag in cli.rs"),
            "last turn's drafts are gone"
        );
    }

    /// Provider that raises the parent's cancel handle as soon as it is asked
    /// for a completion, and then never answers — the interrupt that arrives
    /// while the ultra fan-out is mid-stream, which is when a user is most
    /// likely to press Ctrl-C (nothing streams during the pre-phase).
    struct CancelOnCallProvider {
        handle: Arc<Mutex<Option<CancelHandle>>>,
    }

    #[async_trait::async_trait]
    impl LlmProvider for CancelOnCallProvider {
        async fn health(&self) -> Result<()> {
            Ok(())
        }

        async fn supports_native_tools(&self, _model: &str) -> Result<bool> {
            Ok(true)
        }

        async fn list_models(&self) -> Result<Vec<String>> {
            Ok(Vec::new())
        }

        async fn chat_stream(&self, _request: ChatRequest) -> Result<crate::llm::ChatStream> {
            self.handle
                .lock()
                .unwrap()
                .as_ref()
                .expect("handle bound")
                .cancel();
            // Never answers: the run can only end by being cancelled.
            tokio::time::sleep(Duration::from_secs(3_600)).await;
            unreachable!("the cancelled run is dropped long before this")
        }

        async fn context_window(&self, _model: &str) -> Option<u32> {
            None
        }

        fn label(&self) -> String {
            "cancel-on-call".to_string()
        }
    }

    #[tokio::test]
    async fn cancelling_the_turn_stops_the_ultra_fanout_and_closes_its_panes() {
        let tmp = TempDir::new();
        let slot = Arc::new(Mutex::new(None));
        let provider = Arc::new(CancelOnCallProvider {
            handle: Arc::clone(&slot),
        });
        let session = Session::create(&tmp.0).expect("create session");
        let hooks = Arc::new(HookEngine::new(
            Vec::new(),
            tmp.0.clone(),
            session.id.clone(),
        ));
        let mut agent = Agent::new(
            provider,
            ToolRegistry::new(),
            Config::default(),
            Vec::new(),
            tmp.0.clone(),
            session,
            true,
            hooks,
        )
        .expect("build agent");
        agent.set_usage_log(None);
        agent.set_ultra(Some(ultra_engine()));
        // Exactly what the TUI does before it hands the agent to the turn task:
        // it keeps this handle, and Ctrl-C raises it (see `AppAction::Interrupt`).
        *slot.lock().unwrap() = Some(agent.cancel_handle());

        let (tx, mut rx) = mpsc::channel(64);
        let reason = agent
            .run_turn("something slow", tx)
            .await
            .expect("no error");
        assert_eq!(
            reason,
            DoneReason::Stopped,
            "the turn ends, and ends stopped"
        );

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        let opened: Vec<u64> = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::SubagentRunStarted { run, .. } => Some(*run),
                _ => None,
            })
            .collect();
        assert_eq!(opened.len(), 1, "the candidate's pane opened");
        let closed: Vec<(u64, bool)> = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::SubagentRunDone { run, completed, .. } => Some((*run, *completed)),
                _ => None,
            })
            .collect();
        assert_eq!(
            closed,
            [(opened[0], false)],
            "and it was closed out on the way through: a pane the fan-out leaves at 'running' \
             never retires off the rail, because retirement keys off `finished`"
        );
        assert!(
            !agent.history.iter().any(ultra::is_guidance),
            "a cancelled pre-phase injects nothing"
        );
    }

    #[tokio::test]
    async fn ultra_candidates_bill_the_parents_usage_log() {
        let tmp = TempDir::new();
        let provider = ScriptedProvider::new(vec![
            vec![usage_chunk("draft: do it", 500, 80)], // the candidate
            vec![usage_chunk("did it", 200, 30)],       // the main loop
        ]);
        let mut agent = test_agent_with(&tmp, provider, Vec::new(), ToolRegistry::new());
        agent.set_ultra(Some(ultra_engine()));

        let (tx, _rx) = mpsc::channel(64);
        agent.run_turn("do it", tx).await.unwrap();

        assert_eq!(
            agent.usage().turn_totals(),
            (700, 110),
            "the candidate's tokens are the turn's tokens — an ultra turn that reported only the \
             main agent's spend would understate itself several times over, under a chip that \
             advertises exactly that multiplier"
        );
        let log = std::fs::read_to_string(tmp.0.join("usage.jsonl")).expect("usage log written");
        assert!(log.contains("\"prompt_tokens\":700"), "{log}");
    }
}
