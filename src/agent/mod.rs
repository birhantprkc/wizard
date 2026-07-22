//! Agent loop: build messages → stream completion → parse tool calls →
//! execute tools → repeat until the model is done (or a configured `max_steps`
//! cap, the time limit, the circuit breaker, or an interrupt ends the turn).
//!
//! The loop is UI-agnostic: it emits [`AgentEvent`]s over a channel that the
//! Ratatui TUI (genie) or the headless runner (sovereign) consumes.

pub mod breaker;
pub mod mission;
pub mod prompts;
pub mod session;
pub mod subagent;
mod turn;
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
use crate::hooks::HookEngine;
use crate::images::{ImageRef, ImageStore};
use crate::llm::provider::LlmProvider;
use crate::llm::{ChatMessage, ChatOptions, ChatRequest, FunctionCall, Image, Role, ToolCall};
use crate::mcp::{McpConfig, McpManager};
use crate::skills::Skill;
use crate::tools::{CommandDispatch, ToolContext, ToolOutput, registry::ToolRegistry};

use session::Session;

/// Everything a `/btw` side question needs, owned so it can run without
/// borrowing the live [`Agent`]. Surfaces snapshot this before parking the
/// agent in a turn task so a side question can still fire mid-turn.
#[derive(Clone)]
pub struct SideQuestionContext {
    pub client: Arc<dyn LlmProvider>,
    pub model: String,
    /// Conversation the answer is grounded in (system prompt at index 0).
    pub messages: Vec<ChatMessage>,
    /// Reasoning effort forwarded on the forked call, when set.
    pub reasoning_effort: Option<String>,
}

/// Everything a `/fork` side quest needs, owned so it can spawn without
/// borrowing the live [`Agent`]. Surfaces snapshot this before parking the
/// agent in a turn task so a fork can still fire mid-turn — same pattern as
/// [`SideQuestionContext`], but with tools, hooks, and the background-subagent
/// registry so the fork can work and report back.
#[derive(Clone)]
pub struct ForkContext {
    pub client: Arc<dyn LlmProvider>,
    pub model: String,
    /// Snapshot of the parent conversation (system prompt at index 0).
    pub messages: Vec<ChatMessage>,
    /// Parent tool set at snapshot time (shallow `Arc` clone of each tool).
    pub registry: ToolRegistry,
    /// Lifecycle hooks shared with the parent.
    pub hooks: Arc<HookEngine>,
    /// Parent tool context (cwd, tasks, subagents, usage, images, …). The fork
    /// registers on `subagents` so its report drains into the parent history.
    pub ctx: ToolContext,
    /// Restrict the fork to read-only tools (parent was in plan mode).
    pub read_only: bool,
}

/// System reminder prepended to a `/btw` user message. Mirrors Claude Code's
/// side-question constraints: one shot, no tools, answer from context only.
const SIDE_QUESTION_REMINDER: &str = "\
This is a side question from the user (\"/btw\"). Answer it directly in a \
single response.\n\
\n\
CRITICAL CONSTRAINTS:\n\
- You have NO tools — you cannot read files, run commands, search, or take \
any actions.\n\
- This is a one-off response — there will be no follow-up turns.\n\
- Answer only from what you already know in the conversation context and \
your own knowledge.\n\
- NEVER say things like \"Let me try…\", \"I'll now…\", \"Let me check…\", \
or promise to take any action.\n\
- If you don't know, say so — do not offer to look it up or investigate.\n\
\n\
Simply answer the question with the information you have.";

impl SideQuestionContext {
    /// Fork a single tool-less completion over a copy of `messages` plus the
    /// side question. The main conversation is never written.
    pub async fn ask(&self, question: &str) -> Result<String> {
        let question = question.trim();
        anyhow::ensure!(!question.is_empty(), "empty side question");

        let mut messages = self.messages.clone();
        messages.push(ChatMessage::user(format!(
            "{SIDE_QUESTION_REMINDER}\n\n{question}"
        )));

        let request = ChatRequest {
            model: self.model.clone(),
            messages,
            tools: Vec::new(),
            stream: true,
            options: Some(ChatOptions {
                // Slightly cooler than a normal turn: side questions are
                // factual asides, not creative work.
                temperature: Some(0.3),
                num_ctx: None,
                reasoning_effort: self.reasoning_effort.clone(),
            }),
        };

        let mut stream = self
            .client
            .chat_stream(request)
            .await
            .context("starting /btw side question")?;
        let mut answer = String::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("reading /btw stream")?;
            if let Some(message) = chunk.message
                && !chunk.thinking
            {
                answer.push_str(&message.content);
            }
            if chunk.done {
                break;
            }
        }
        let answer = answer.trim().to_string();
        if answer.is_empty() {
            anyhow::bail!("empty /btw reply");
        }
        Ok(answer)
    }
}

impl ForkContext {
    /// Detach a `/fork` side quest against this snapshot. Registers on the
    /// parent's background-subagent registry and streams progress through
    /// `events` (when provided) so the surface can open a pane. Returns the
    /// background-registry id immediately; the report lands in the parent
    /// history the next time background subagents are drained.
    ///
    /// `events` should be a channel the surface is already listening on
    /// (turn-forwarded or a dedicated idle collector). When `None`, the fork
    /// still runs and still reports via the registry — it just has no pane.
    pub async fn spawn(self, task: &str, events: Option<mpsc::Sender<AgentEvent>>) -> Result<u32> {
        let task = task.trim();
        anyhow::ensure!(!task.is_empty(), "empty fork task");

        let run = subagent::next_run_id();
        let options = subagent::SpawnOptions {
            model: Some(self.model.clone()),
            read_only: self.read_only,
            inherited_history: None, // spawn_fork sets this itself
        };

        let name = subagent::FORK_NAME.to_string();
        let task_owned = task.to_string();
        let client = Arc::clone(&self.client);
        let registry = self.registry;
        let hooks = Arc::clone(&self.hooks);
        let history = self.messages;
        // Carry the surface's event channel into the fork's tool context so
        // SubagentRun* progress streams to the same place the parent does.
        let mut fut_ctx = self.ctx.clone();
        fut_ctx.events = events.clone();
        let fut_options = options;
        let fut_task = task_owned.clone();
        let fut = async move {
            match subagent::spawn_fork(
                run,
                &fut_task,
                history,
                &fut_options,
                &client,
                &registry,
                &hooks,
                &fut_ctx,
            )
            .await
            {
                Ok(result) => crate::tools::subagent_tasks::SubagentRunResult {
                    completed: result.completed,
                    output: result.output,
                    steps_used: result.steps_used,
                    error: None,
                },
                Err(err) => crate::tools::subagent_tasks::SubagentRunResult {
                    completed: false,
                    output: format!("fork failed: {err:#}"),
                    steps_used: 0,
                    error: Some(format!("{err:#}")),
                },
            }
        };

        let id = self.ctx.subagents.reserve(&name, task);
        if let Some(events) = &events {
            emit(
                events,
                AgentEvent::SubagentRunStarted {
                    run,
                    bg: Some(id),
                    name: name.clone(),
                    task: task_owned.clone(),
                },
            )
            .await;
            emit(
                events,
                AgentEvent::SubagentStarted {
                    id,
                    name: name.clone(),
                    task: task_owned.clone(),
                },
            )
            .await;
        }
        self.ctx.subagents.attach(id, fut);
        Ok(id)
    }
}

/// Why an agent turn (or sovereign run) ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoneReason {
    /// Model finished without requesting more tools.
    Completed,
    /// A configured `max_steps` cap was exhausted. Never reached on the default
    /// unlimited budget.
    MaxSteps,
    /// `--max-hours` elapsed (sovereign).
    TimeLimit,
    /// Stopped via the loop-control file or user interrupt.
    Stopped,
    /// Circuit breaker: the LLM endpoint breaker tripped (provider down), or
    /// repeated identical failures (sovereign), or too many consecutive
    /// failures of one tool.
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

/// Where an image came from, on an [`AgentEvent::Images`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageSource {
    /// The model produced it inline in its reply
    /// ([`ChatChunk::images`](crate::llm::ChatChunk::images)).
    Assistant,
    /// A tool returned it ([`ToolOutput::images`]); the name of the tool.
    Tool(String),
}

impl ImageSource {
    /// Stable tag for the structured surfaces (`stream-json`, the GUI's
    /// protocol frames).
    pub fn as_str(&self) -> &'static str {
        match self {
            ImageSource::Assistant => "assistant",
            ImageSource::Tool(_) => "tool",
        }
    }

    /// The tool that produced the image, if a tool did.
    pub fn tool(&self) -> Option<&str> {
        match self {
            ImageSource::Assistant => None,
            ImageSource::Tool(name) => Some(name),
        }
    }
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
    /// A tool call finished. `output.images` carries any images the tool
    /// produced, as base64; the [`AgentEvent::Images`] that follows says where
    /// they landed on disk, which is what a renderer wants.
    ToolFinished { name: String, output: ToolOutput },
    /// Images produced during this turn — by a tool, or by the model itself —
    /// and written to the session's image directory
    /// (`~/.wizard/images/<session>/`).
    ///
    /// This is the event the surfaces render off. Each [`ImageRef`] names a
    /// file on disk plus its media type and size: the TUI prints the path when
    /// the terminal cannot draw the image, the GUI links to it for "open full
    /// size". No base64 rides on this event — the payload the model needs stays
    /// in history, and a transcript frame references the image rather than
    /// embedding it.
    ///
    /// Ordering: for a tool's images this arrives immediately after that tool's
    /// [`AgentEvent::ToolFinished`]; for the model's own images, immediately
    /// after the last [`AgentEvent::TextDelta`] of the reply that produced them.
    Images {
        source: ImageSource,
        images: Vec<ImageRef>,
    },
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
    /// counts. Surfaces accumulate these (status bar lifetime totals via
    /// `/cost`, headless summary). The TUI context meter uses
    /// `prompt_tokens` as the size of the next call until compaction or
    /// `/clear` replaces it with an estimate via [`AgentEvent::ContextSize`].
    /// Emitted for the parent's own calls and for every subagent call made
    /// under it (`spawn_subagent`, `/ultra`'s candidates and judges), so the
    /// counter reflects what the turn actually spent.
    Usage {
        prompt_tokens: u64,
        completion_tokens: u64,
    },
    /// Tokens that will load into the next model call after history shrank
    /// (`/compact`, auto-compaction). Replaces the context meter without
    /// touching session lifetime totals.
    ContextSize { tokens: u64 },
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
    /// list; the TUI mirrors it in a compact overlay above the composer,
    /// headless prints a one-line summary, the gateway ignores it.
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
    /// [`AgentEvent::Images`], scoped to a subagent run — images produced
    /// inside a run land in the same session directory and are announced the
    /// same way, so a run's pane can render them instead of losing them.
    SubagentRunImages {
        run: u64,
        source: ImageSource,
        images: Vec<ImageRef>,
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

/// Take custody of images produced during a turn — the one seam every image
/// passes through, from either direction: a tool's [`ToolOutput::images`] or
/// the model's own [`ChatChunk::images`](crate::llm::ChatChunk::images).
///
/// Images over [`crate::llm::MAX_IMAGE_BYTES`] are dropped here with a notice:
/// an absurd image must not reach history, where it would melt the context
/// window and bloat the session file. The rest are written to the session's
/// image store and announced to the surfaces as `announce(refs)` — an event
/// carrying paths, never base64. Persistence is best-effort (see
/// [`ImageStore::save_all`]); the model's copy is the base64 this returns, for
/// the caller to attach to the message it is about to push.
pub(crate) async fn absorb_images(
    images: Vec<Image>,
    store: Option<&Arc<ImageStore>>,
    events: Option<&mpsc::Sender<AgentEvent>>,
    announce: impl FnOnce(Vec<ImageRef>) -> AgentEvent,
) -> Vec<Image> {
    if images.is_empty() {
        return images;
    }
    let (kept, dropped) = crate::images::split_oversized(images);
    if !dropped.is_empty() {
        let notice = crate::images::oversized_notice(&dropped);
        tracing::warn!("{notice}");
        if let Some(events) = events {
            emit(events, AgentEvent::Notice(notice)).await;
        }
    }
    let Some(store) = store else {
        // No store (a registry driven directly, outside an agent): the images
        // still reach the model, they just land nowhere for the surfaces.
        return kept;
    };
    // Each surviving image comes back tagged with its path, so the session file
    // records where it went and a replayed transcript needs no re-derivation.
    let (kept, saved) = store.save_all(kept);
    if !saved.is_empty()
        && let Some(events) = events
    {
        emit(events, announce(saved)).await;
    }
    kept
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

/// Cooperative "background the foreground command" handle. Cloneable and
/// thread-safe: the TUI keeps a clone (see [`Agent::background_gate`]) and
/// calls [`BackgroundGate::request`] on Ctrl-B; a running `execute` selects
/// on it and promotes the child into the background task registry without
/// interrupting the turn. One-shot per arming — after a promote (or a
/// clear at the next tool call) the gate is inert until the next request.
///
/// Unlike [`CancelHandle`], firing this does **not** end the turn: the tool
/// returns immediately with a background-task notice and the agent keeps
/// working (or finishes its step) while the command keeps running.
#[derive(Clone, Default)]
pub struct BackgroundGate(Arc<BackgroundGateState>);

impl std::fmt::Debug for BackgroundGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BackgroundGate")
    }
}

#[derive(Default)]
struct BackgroundGateState {
    flag: std::sync::atomic::AtomicBool,
    notify: tokio::sync::Notify,
}

impl BackgroundGate {
    /// Ask the in-flight foreground `execute` (if any) to promote itself to
    /// a background task. No-op when nothing is listening.
    pub fn request(&self) {
        self.0.flag.store(true, std::sync::atomic::Ordering::SeqCst);
        self.0.notify.notify_waiters();
    }

    /// Whether a promote has been requested since the last clear.
    pub fn is_requested(&self) -> bool {
        self.0.flag.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Resolves once a promote is requested (immediately if it already was).
    pub async fn requested(&self) {
        loop {
            let notified = self.0.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_requested() {
                return;
            }
            notified.await;
        }
    }

    /// Clear a stale request so the next tool call starts clean. Called at
    /// the start of every foreground `execute` and at the start of every
    /// turn (alongside [`CancelHandle::clear`]).
    pub fn clear(&self) {
        self.0
            .flag
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }
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
    // `/fork` side quests share the background-subagent drain path; label them
    // so the main model (and the user reading the transcript) can tell a
    // user-spawned fork from a `spawn_subagent` delegation at a glance.
    let kind = if task.name == subagent::FORK_NAME {
        "fork"
    } else {
        "background subagent"
    };
    format!(
        "[{kind} #{} '{}' {} after {} step(s)] {}\n\n{}",
        task.id, task.name, status, task.steps_used, task.task, task.output
    )
}

/// The tool-calling agent. Owns the conversation history, the model client,
/// the tool dispatcher, and session persistence.
pub struct Agent {
    client: Arc<dyn LlmProvider>,
    /// Circuit breaker over the model endpoint (see [`breaker`]): bounds the
    /// streaming retry loop when a provider is down instead of retrying it
    /// forever, and recovers on its own. Reset on a provider switch.
    llm_breaker: breaker::LlmBreaker,
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
    /// Cooperative "background the foreground command" gate (see
    /// [`Agent::background_gate`]). Cleared at the start of every turn; the
    /// TUI fires it on Ctrl-B while an `execute` is in flight.
    background: BackgroundGate,
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

/// Prefix of the system note carrying `session_start` hook output.
///
/// The note is context for the model, not conversation: surfaces that replay a
/// session from disk (the GUI transcript) match on this to drop it, the way the
/// TUI drops every system message when it reloads a transcript. Hook *events*
/// are still reported, as one-line [`AgentEvent::HookFired`] notices.
pub const SESSION_START_HOOK_NOTE: &str = "[session_start hook]";

/// Number of most-recent messages preserved verbatim when compacting history.
const KEEP_RECENT: usize = 10;

/// Heading of the note [`Agent::compact_now`] leaves in place of the span it
/// summarized. Public because it is the only handle anything downstream has on
/// that note: it is a `Role::System` message like any other, and
/// [`ultra::render_context`] has to be able to tell "everything older than the
/// tail, summarized" apart from an ordinary injected note when it briefs a
/// candidate.
pub const COMPACT_SUMMARY_HEADING: &str = "[Compacted progress summary]";

/// Prefix of the ephemeral per-step pressure line injected (in memory only)
/// before each model completion. Surfaces and compaction can recognize it; it
/// is never persisted to the session file.
pub const CONTEXT_PRESSURE_HEADING: &str = "[context pressure]";

/// Fraction of the provider's context window the last prompt may fill before
/// token-aware compaction kicks in.
const COMPACT_WINDOW_FRACTION: f64 = 0.8;

/// Soft pressure band: the model is nudged to call `compact` once fill crosses
/// this fraction of the known window (auto-compact still waits for
/// [`COMPACT_WINDOW_FRACTION`]).
const PRESSURE_ELEVATED_FRACTION: f64 = 0.5;

/// Strong pressure band: the model is told to compact *before* more tool work.
const PRESSURE_HIGH_FRACTION: f64 = 0.7;

/// Chunk size (chars) fed to one rolling-summary pass during compaction.
const COMPACT_CHUNK_CHARS: usize = 20_000;

/// How full the next model call's prompt is, for the live pressure signal and
/// the `compact` tool's reply. Built from the last reported prompt size (or a
/// char/4 estimate) plus the provider's known context window when available.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextPressure {
    /// Tokens that will load into the next model call.
    pub tokens: u64,
    /// Provider context window in tokens, when known.
    pub window: Option<u32>,
    /// `tokens / window` when the window is known; otherwise a byte-threshold
    /// proxy so headless runs without a reported window still get a signal.
    pub fill: f64,
    pub level: PressureLevel,
}

/// Coarse pressure band shown to the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PressureLevel {
    /// Comfortable headroom.
    Ok,
    /// Crossing ~50% of the window (or half the byte threshold) — compact when
    /// convenient.
    Elevated,
    /// Crossing ~70% — compact before more tool work.
    High,
    /// At or past the auto-compact trigger (~80% / byte threshold).
    Critical,
}

impl PressureLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            PressureLevel::Ok => "ok",
            PressureLevel::Elevated => "elevated",
            PressureLevel::High => "high",
            PressureLevel::Critical => "critical",
        }
    }
}

impl ContextPressure {
    /// One-line note the model sees each step (ephemeral, not persisted).
    pub fn signal_line(&self) -> String {
        let tokens = crate::usage::format_tokens(self.tokens);
        let window = match self.window {
            Some(w) => crate::usage::format_tokens(u64::from(w)),
            None => "unknown window".to_string(),
        };
        let pct = (self.fill * 100.0).round() as i32;
        let advice = match self.level {
            PressureLevel::Ok => "headroom ok",
            PressureLevel::Elevated => "consider calling compact soon",
            PressureLevel::High => "call compact before more tool work",
            PressureLevel::Critical => "auto-compact imminent — call compact now",
        };
        format!(
            "{CONTEXT_PRESSURE_HEADING} {} · {tokens} / {window} ({pct}%) — {advice}",
            self.level.as_str()
        )
    }
}

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

        // The agent's token counters, shared into the tool context so a
        // subagent's model calls bill the parent (see `ToolContext::usage`).
        let usage = Arc::new(crate::usage::UsageTracker::new());

        // Images produced this session (by a tool or by the model) land under
        // `~/.wizard/images/<session>/`, so every surface has a real file to
        // render or link to.
        let background = BackgroundGate::default();
        let mut ctx = ToolContext::new(project_root)
            .with_web(web)
            .with_checkpoints(Arc::clone(&checkpoints))
            .with_usage(Arc::clone(&usage))
            .with_background(background.clone());
        if let Some(images) = open_image_store(&session.id) {
            ctx = ctx.with_images(images);
        }

        let mut agent = Self {
            client,
            llm_breaker: breaker::LlmBreaker::new(),
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
            ctx,
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
            background,
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

    /// Declare which slash commands the surface behind this agent will run when
    /// the agent queues one via `run_command` (see [`CommandDispatch`]). Only a
    /// surface that drains the queue makes the tool useful; headless and gateway
    /// runs leave it at `None` so the tool refuses rather than report success for
    /// a command nothing applies. Preserved across per-turn context clones
    /// (`ToolContext::with_events`).
    pub fn set_command_dispatch(&mut self, dispatch: CommandDispatch) {
        self.ctx.command_dispatch = dispatch;
    }

    /// Conversation history (system prompt included).
    pub fn history(&self) -> &[ChatMessage] {
        &self.history
    }

    /// Tokens that will load into the next model call: the backend's last
    /// reported prompt size when known, otherwise a char/4 estimate of the
    /// current history (used right after `/clear` or compaction, when the
    /// real count is stale). This is the number the TUI status bar shows —
    /// *not* the session-lifetime sum of every past prompt.
    pub fn context_tokens(&self) -> u64 {
        match self.usage.last_prompt_tokens() {
            Some(n) => n,
            None => crate::llm::estimate_history_tokens(&self.history),
        }
    }

    /// Live fill of the next model call against the provider window (or a
    /// byte-threshold proxy when the window is unknown). Powers the per-step
    /// pressure signal and the `compact` tool's reply.
    ///
    /// Soft bands (`elevated` / `high`) may use a char/4 estimate when the
    /// backend has not reported a prompt size yet. The `critical` band — which
    /// drives auto-compaction — matches the historical gates exactly: bytes
    /// over threshold, or a *reported* last prompt over 80% of a known window.
    /// Estimates never trip auto-compact on their own (they would fire on the
    /// system prompt alone and steal the first completion of a short turn).
    pub async fn context_pressure(&self) -> ContextPressure {
        let tokens = self.context_tokens();
        let window = self.client.context_window(&self.model).await;
        let byte_total: usize = self.history.iter().map(|msg| msg.content.len()).sum();
        let threshold = self.config.compact_threshold_bytes.max(1);
        let last_prompt = self.usage.last_prompt_tokens();

        let fill = match window {
            Some(w) if w > 0 => tokens as f64 / f64::from(w),
            _ => byte_total as f64 / threshold as f64,
        };

        let auto_critical = byte_total > threshold
            || match (last_prompt, window) {
                (Some(prompt), Some(w)) if w > 0 => {
                    prompt as f64 > f64::from(w) * COMPACT_WINDOW_FRACTION
                }
                _ => false,
            };

        let level = if auto_critical {
            PressureLevel::Critical
        } else if fill >= PRESSURE_HIGH_FRACTION {
            PressureLevel::High
        } else if fill >= PRESSURE_ELEVATED_FRACTION {
            PressureLevel::Elevated
        } else {
            PressureLevel::Ok
        };
        ContextPressure {
            tokens,
            window,
            fill,
            level,
        }
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
    /// is reset. Session token counters go to zero with the history so the
    /// TUI context meter and `/cost` do not keep the wiped conversation.
    pub fn clear(&mut self) -> Result<()> {
        self.ctx.tasks.kill_all();
        self.ctx.subagents.kill_all();
        self.ctx.tasks = Arc::new(crate::tools::tasks::TaskRegistry::new());
        self.ctx.subagents = Arc::new(crate::tools::subagent_tasks::SubagentTaskRegistry::new());
        self.ctx.todos = Arc::new(std::sync::Mutex::new(Vec::new()));
        self.session = Session::create(&Config::sessions_dir()?)?;
        // Images follow the session: the fresh conversation writes into its own
        // directory, and the old one's files stay where its transcript points.
        self.ctx.images = open_image_store(&self.session.id);
        self.hooks.set_session_id(self.session.id.clone());
        self.history.truncate(1);
        self.dispatcher.reset_failures();
        self.usage.clear_session();
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

    /// Handle for backgrounding a foreground `execute` mid-flight (Ctrl-B).
    /// The surface clones it before spawning `run_turn` and calls
    /// [`BackgroundGate::request`]; the running command promotes itself into
    /// the background task registry and returns immediately so the turn can
    /// keep going. No-op when nothing is listening.
    pub fn background_gate(&self) -> BackgroundGate {
        self.background.clone()
    }

    /// Shared handle on the background-shell-task registry, so a surface can
    /// kill a task or open its live output while a turn holds the agent —
    /// same pattern as [`Self::subagent_registry`].
    pub fn task_registry(&self) -> Arc<crate::tools::tasks::TaskRegistry> {
        Arc::clone(&self.ctx.tasks)
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
                "{SESSION_START_HOOK_NOTE}\n{extra}"
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

    /// The agent's working todo list, for `/status` on a surface that does not
    /// mirror the `TodoUpdated` events itself.
    pub fn todos(&self) -> Vec<crate::tools::todo::TodoItem> {
        self.ctx
            .todos
            .lock()
            .map(|list| list.clone())
            .unwrap_or_default()
    }

    /// The model client this agent talks to. A surface rebuilding the tool
    /// registry (`/reload`) has to hand the subagent spawner the same client
    /// the parent runs on, or its subagents answer from a different model than
    /// the one `/model` and `/fusion` last set.
    pub fn client(&self) -> &Arc<dyn LlmProvider> {
        &self.client
    }

    /// Active model tag (what the next completion will call).
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Snapshot of everything a [`SideQuestionContext`] needs to answer a
    /// `/btw` without borrowing the agent. Surfaces clone this before parking
    /// the agent in a turn task so a side question can run *while* the turn is
    /// in flight — the whole point of `/btw`.
    pub fn side_question_context(&self) -> SideQuestionContext {
        SideQuestionContext {
            client: Arc::clone(&self.client),
            model: self.model.clone(),
            messages: self.history.clone(),
            reasoning_effort: self
                .config
                .reasoning_effort
                .map(|effort| effort.as_str().to_string()),
        }
    }

    /// Snapshot of everything a [`ForkContext`] needs to spawn a `/fork` side
    /// quest without borrowing the agent. Same mid-turn pattern as
    /// [`Self::side_question_context`]: surfaces clone this before the agent
    /// leaves its slot so a fork can still fire while a turn is running.
    pub fn fork_context(&self) -> ForkContext {
        ForkContext {
            client: Arc::clone(&self.client),
            model: self.model.clone(),
            messages: self.history.clone(),
            registry: self.dispatcher.registry().snapshot(),
            hooks: Arc::clone(&self.hooks),
            ctx: self.ctx.clone(),
            read_only: self.plan_mode(),
        }
    }

    /// Answer a one-shot side question (`/btw`) against the live history. Does
    /// **not** push the exchange into history or the session file — that is
    /// the feature. Prefer [`SideQuestionContext`] when the agent is out of
    /// its slot mid-turn.
    pub async fn answer_side_question(&self, question: &str) -> Result<String> {
        self.side_question_context().ask(question).await
    }

    /// Spawn a `/fork` side quest against the live history. Detaches into the
    /// background-subagent registry and returns its id immediately; the report
    /// is injected into history the next time background subagents drain.
    /// Prefer [`ForkContext`] when the agent is out of its slot mid-turn.
    pub async fn spawn_fork(
        &self,
        task: &str,
        events: Option<mpsc::Sender<AgentEvent>>,
    ) -> Result<u32> {
        self.fork_context().spawn(task, events).await
    }

    /// Swap the model client mid-session (`/fusion`: the panel answers every
    /// turn; toggling back restores the configured provider). The conversation
    /// and the session file are untouched — on a surface whose chat *is* its
    /// session file (the GUI), rotating either to change what answers would
    /// strand the page on a session nothing writes to any more.
    ///
    /// The caller must also rebuild the tool registry against the new client
    /// ([`build_tool_registry`]), or subagents keep spawning on the old one.
    pub fn set_client(&mut self, client: Arc<dyn LlmProvider>, native_tools: bool) {
        self.client = client;
        self.native_tools = native_tools;
        // A new endpoint starts with a clean breaker — don't inherit the old
        // provider's failure history.
        self.llm_breaker = breaker::LlmBreaker::new();
        self.refresh_system_prompt();
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
        // Always teach context stewardship: auto-compaction + session JSONL
        // are always on, and the agent should compact / reset deliberately
        // rather than wait for the window to overflow.
        prompt.push_str("\n\n");
        prompt.push_str(prompts::CONTEXT_PROMPT);
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

    /// Whether the history is close enough to overflowing to warrant
    /// compaction: either the serialized history exceeds the byte threshold,
    /// or the last model call's reported prompt size exceeds
    /// [`COMPACT_WINDOW_FRACTION`] of the provider's known context window.
    async fn should_compact(&self) -> bool {
        let pressure = self.context_pressure().await;
        pressure.level == PressureLevel::Critical
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
                let _ = emit(
                    events,
                    AgentEvent::ContextSize {
                        tokens: self.context_tokens(),
                    },
                )
                .await;
            }
            outcome @ CompactOutcome::Truncated { .. } => {
                let _ = emit(events, AgentEvent::Error(outcome.describe())).await;
                let _ = emit(
                    events,
                    AgentEvent::ContextSize {
                        tokens: self.context_tokens(),
                    },
                )
                .await;
            }
        }
    }

    /// Summarize the middle span (everything between the system prompt and the
    /// last [`KEEP_RECENT`] messages) into a single note, unconditionally —
    /// the `/compact` command's force path, the `compact` tool, and the shared
    /// core of [`compact_if_needed`]. A summarization failure falls back to
    /// dropping the middle span. Never aborts a turn.
    ///
    /// On success the progress note is appended to the session as a system note
    /// so resume and the model both see that stewardship happened (the full
    /// pre-compact transcript remains earlier in the JSONL).
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
                // Persist the note so resume replays the stewardship breadcrumb;
                // the in-memory middle span is replaced (full transcript stays
                // earlier in the append-only JSONL).
                if let Err(err) = self.session.append_system_note(&replacement) {
                    tracing::warn!("session append failed for compact note: {err}");
                }
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

/// The image store for session `id` (`~/.wizard/images/<id>/`). A store that
/// cannot be opened (no home directory) costs the surfaces their copy of an
/// image, never the turn — the model still gets the base64 — so the failure is
/// logged, not fatal.
fn open_image_store(id: &str) -> Option<Arc<ImageStore>> {
    match ImageStore::open(id) {
        Ok(store) => Some(Arc::new(store)),
        Err(err) => {
            tracing::warn!("could not open the session image store: {err:#}");
            None
        }
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
/// starting a new one. Each builds exactly one agent, so each lets this path
/// connect the MCP servers for it.
pub async fn build_headless_agent(
    config: &Config,
    project_root: &Path,
    resume: bool,
) -> Result<Agent> {
    build_headless_agent_inner(config, project_root, resume, None, None).await
}

/// [`build_headless_agent`] with an explicit session instead of the
/// latest-or-new resolution — the GUI server manages one session per task
/// (created for a chosen workspace, or reopened by id) and hands it in.
///
/// `mcp` is the caller's already-connected manager. A process that builds more
/// than one agent — the GUI, one per warm task — must connect its servers once
/// and pass them here: connecting per build would run one copy of every
/// configured MCP server *per agent*, each a real OS process. `None` connects a
/// manager for this agent alone.
pub async fn build_headless_agent_for_session(
    config: &Config,
    project_root: &Path,
    session: Session,
    mcp: Option<&McpManager>,
) -> Result<Agent> {
    build_headless_agent_inner(config, project_root, false, Some(session), mcp).await
}

/// The agent's whole tool set, freshly composed: native tools, scripted tools
/// (`~/.wizard/tools`), the MCP tools `manager` is connected to, the subagent
/// spawner, and the config-dependent `evolve` / `publish` tools.
///
/// Returns the registry and the spawn tool's shared model slot, which the caller
/// must hand to [`Agent::bind_subagent_model`] — a fresh spawn tool reads the
/// *configured* model until it is bound, and would quietly ignore `/model`.
///
/// This is what a build composes and what `/reload` recomposes, so a reloaded
/// session has exactly the tools a fresh one does — no more (a second copy of
/// every MCP server) and no fewer (`evolve` and `publish` silently dropped).
pub async fn build_tool_registry(
    config: &Config,
    client: &Arc<dyn LlmProvider>,
    hooks: &Arc<HookEngine>,
    manager: &McpManager,
) -> Result<(ToolRegistry, subagent::SharedActiveModel)> {
    let mut base = ToolRegistry::with_native_tools();
    match Config::scripted_tools_dir() {
        Ok(dir) => {
            if let Err(err) = base.load_scripted(&dir) {
                tracing::warn!("loading scripted tools failed: {err}");
            }
        }
        Err(err) => tracing::warn!("scripted tools dir unavailable: {err}"),
    }
    if let Err(err) = base.attach_mcp(manager).await {
        tracing::warn!("attaching MCP tools failed: {err}");
    }
    base.apply_harness_overrides();

    let subagents_dir = Config::subagents_dir()?;
    let subagent_configs = subagent::available_configs(&subagents_dir);
    let base = Arc::new(base);
    let mut registry = subagent::scoped_registry(&base, None);
    let spawn_tool = Arc::new(subagent::SpawnSubagentTool::new(
        subagent_configs,
        Arc::clone(client),
        Arc::clone(&base),
        Arc::clone(hooks),
    ));
    let subagent_model = spawn_tool.model_handle();
    registry.register(spawn_tool);
    registry.register(Arc::new(crate::tools::evolve::EvolveTool::new(
        config.clone(),
    )));
    registry.register(Arc::new(crate::tools::publish::PublishTool::new(
        config.clone(),
    )));
    Ok((registry, subagent_model))
}

/// Skills from the repo/bundled roots plus `~/.wizard/skills` (user shadowing).
/// A skill tree that will not load costs its skills, never the session.
pub fn load_skills() -> Vec<Skill> {
    let roots = crate::skills::default_roots();
    crate::skills::load_skills(&roots).unwrap_or_else(|err| {
        tracing::warn!("loading skills failed: {err}");
        Vec::new()
    })
}

/// Connect every server in `~/.wizard/mcp.toml`. Never hard-fails: a missing or
/// broken config, or a server that will not come up, costs its tools — not the
/// session.
pub async fn connect_mcp() -> McpManager {
    let config = match Config::mcp_config_path().and_then(|path| McpConfig::load(&path)) {
        Ok(config) => config,
        Err(err) => {
            tracing::warn!("could not load mcp.toml: {err:#}");
            return McpManager::empty();
        }
    };
    match McpManager::connect_all(&config).await {
        Ok(manager) => manager,
        Err(err) => {
            tracing::warn!("MCP startup failed: {err:#}");
            McpManager::empty()
        }
    }
}

async fn build_headless_agent_inner(
    config: &Config,
    project_root: &Path,
    resume: bool,
    session: Option<Session>,
    mcp: Option<&McpManager>,
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
    // Ollama's analog: a configured tag that is not pulled yet is pulled now
    // (loopback hosts only — never download onto a remote server).
    if active.kind == ProviderKind::Ollama && crate::server::local_port(&active.base_url).is_some()
    {
        let wait =
            crate::progress::ServerSpinner::start_with("Checking the local model…", "model ready");
        let outcome = crate::llm::ollama::OllamaClient::new(active.base_url.clone())
            .ensure_model(&model, &wait)
            .await;
        wait.finish(outcome.is_ok());
        outcome?;
    }
    client
        .health()
        .await
        .with_context(|| format!("LLM health check failed for {}", client.label()))?;

    let native_tools = crate::llm::provider::probe_native_tools(client.as_ref(), &model).await;
    if !native_tools {
        println!("using the JSON tool protocol for '{model}'");
    }

    // Session first: the hook engine carries its id in every payload. An
    // explicit session (GUI) wins; otherwise resolve latest-or-new here.
    let session = match session {
        Some(session) => session,
        None => {
            let sessions_dir = Config::sessions_dir()?;
            if resume {
                match Session::open_latest(&sessions_dir)? {
                    Some(session) => session,
                    None => Session::create(&sessions_dir)?,
                }
            } else {
                Session::create(&sessions_dir)?
            }
        }
    };

    // Lifecycle hooks, shared by the agent's dispatcher and the subagent
    // spawner so subagent tool calls fire the same hooks.
    let hooks = Arc::new(HookEngine::new(
        crate::hooks::load(project_root),
        project_root.to_path_buf(),
        session.id.clone(),
    ));

    // Tools: natives + scripted + MCP, then the subagent spawner on top.
    let connected;
    let manager = match mcp {
        Some(manager) => manager,
        None => {
            connected = connect_mcp().await;
            &connected
        }
    };
    let (registry, subagent_model) = build_tool_registry(config, &client, &hooks, manager).await?;

    let skills = load_skills();

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
    // expansion and `@file` references (including image attachments).
    let custom_commands = crate::commands::load(&project_root);
    let prepared = crate::commands::preprocess(&goal, &custom_commands, &project_root);
    let goal = prepared.text;
    let goal_images = prepared.images;

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

        // First checkpoint turn of this cycle, for rollback_failed_cycles
        // (run_turn assigns the next id via begin_turn).
        let cycle_first_turn = agent.checkpoints().current_turn() + 1;
        // Images only ride on the first cycle's initial user prompt; later
        // continuation prompts are pure text.
        let turn_images = if iteration == 1 {
            goal_images.clone()
        } else {
            Vec::new()
        };
        match agent
            .run_turn_with_images(&input, turn_images, tx.clone())
            .await
        {
            Ok(reason) => {
                final_reason = reason;
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
    use crate::config::StepBudget;
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
        /// Upcoming chat_stream calls that fail with a transient transport
        /// error before the scripted responses resume.
        fail: Mutex<u32>,
    }

    impl ScriptedProvider {
        fn new(responses: Vec<Vec<ChatChunk>>) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(responses.into()),
                requests: Mutex::new(Vec::new()),
                context_window: None,
                fail: Mutex::new(0),
            })
        }

        fn with_context_window(responses: Vec<Vec<ChatChunk>>, window: u32) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(responses.into()),
                requests: Mutex::new(Vec::new()),
                context_window: Some(window),
                fail: Mutex::new(0),
            })
        }

        fn flaky(failures: u32, responses: Vec<Vec<ChatChunk>>) -> Arc<Self> {
            let provider = Self::new(responses);
            *provider.fail.lock().unwrap() = failures;
            provider
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
            {
                let mut fail = self.fail.lock().unwrap();
                if *fail > 0 {
                    *fail -= 1;
                    return Err(crate::llm::ProviderError::transport("scripted flake").into());
                }
            }
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

    /// Provider whose every call fails with a transient error, to exercise the
    /// endpoint circuit breaker's fail-fast.
    #[derive(Debug)]
    struct FailingProvider {
        calls: Mutex<u32>,
    }

    #[async_trait::async_trait]
    impl LlmProvider for FailingProvider {
        async fn health(&self) -> Result<()> {
            Ok(())
        }
        async fn supports_native_tools(&self, _model: &str) -> Result<bool> {
            Ok(true)
        }
        async fn list_models(&self) -> Result<Vec<String>> {
            Ok(Vec::new())
        }
        async fn chat_stream(&self, _request: ChatRequest) -> Result<ChatStream> {
            *self.calls.lock().unwrap() += 1;
            // A transport failure (status None) is transient, so the retry loop
            // keeps trying — until the breaker trips.
            Err(crate::llm::ProviderError::transport("simulated outage").into())
        }
        async fn context_window(&self, _model: &str) -> Option<u32> {
            None
        }
        fn label(&self) -> String {
            "failing:test".to_string()
        }
    }

    #[tokio::test]
    async fn a_down_provider_trips_the_breaker_instead_of_retrying_forever() {
        let tmp = TempDir::new();
        let session = Session::create(&tmp.0).expect("create session");
        let hooks = Arc::new(HookEngine::new(
            Vec::new(),
            tmp.0.clone(),
            session.id.clone(),
        ));
        let provider = Arc::new(FailingProvider {
            calls: Mutex::new(0),
        });
        let client: Arc<dyn LlmProvider> = provider.clone();

        // Continuous mode has no per-turn attempt cap: without the breaker this
        // turn would retry forever. Zero backoff so the test never sleeps.
        let config = Config {
            continuous: true,
            retry_base_secs: 0,
            retry_max_secs: 0,
            ..Config::default()
        };

        let mut agent = Agent::new(
            client,
            ToolRegistry::new(),
            config,
            Vec::new(),
            tmp.0.clone(),
            session,
            true,
            hooks,
        )
        .expect("build agent");
        agent.set_usage_log(Some(tmp.0.join("usage.jsonl")));

        let (tx, _rx) = mpsc::channel(256);
        // If the breaker were absent this would hang; reaching an assertion at
        // all is half the proof.
        let reason = agent
            .run_turn("do something", tx)
            .await
            .expect("turn resolves");
        assert_eq!(
            reason,
            DoneReason::CircuitBreaker,
            "a down provider must end the turn as a circuit breaker, not hang"
        );
        // The loop stops dialing once the breaker trips at its threshold (8),
        // rather than retrying without bound.
        assert_eq!(
            *provider.calls.lock().unwrap(),
            8,
            "stopped at the trip threshold"
        );
    }

    /// `done: true` chunk; `content` becomes the visible message when
    /// non-empty.
    fn final_chunk(content: &str) -> ChatChunk {
        ChatChunk {
            message: (!content.is_empty()).then(|| ChatMessage::assistant(content)),
            images: Vec::new(),
            thinking: false,
            done: true,
            done_reason: None,
            eval_count: None,
            prompt_eval_count: None,
        }
    }

    /// A tiny PNG (a real magic number and some bytes behind it).
    fn test_png() -> Image {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        bytes.extend_from_slice(b"a few pixels");
        Image::from_bytes(&bytes).expect("a PNG")
    }

    /// A live chunk carrying a generated image — what an image-capable provider
    /// emits on [`ChatChunk::images`].
    fn image_chunk(images: Vec<Image>) -> ChatChunk {
        ChatChunk {
            message: None,
            images,
            thinking: false,
            done: false,
            done_reason: None,
            eval_count: None,
            prompt_eval_count: None,
        }
    }

    /// Every [`AgentEvent::Images`] a turn emitted, flattened.
    fn drain_images(rx: &mut mpsc::Receiver<AgentEvent>) -> Vec<(ImageSource, ImageRef)> {
        let mut out = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let AgentEvent::Images { source, images } = event {
                out.extend(images.into_iter().map(|image| (source.clone(), image)));
            }
        }
        out
    }

    /// A tool that returns `images` alongside its text.
    struct ImageTool {
        images: Vec<Image>,
    }

    #[async_trait::async_trait]
    impl crate::tools::Tool for ImageTool {
        fn name(&self) -> &str {
            "generate_image"
        }
        fn description(&self) -> &str {
            "Generate an image."
        }
        fn parameters(&self) -> Value {
            json!({ "type": "object", "properties": {} })
        }
        async fn execute(
            &self,
            _args: Value,
            _ctx: &crate::tools::ToolContext,
        ) -> Result<ToolOutput, crate::tools::ToolError> {
            Ok(ToolOutput::ok_with_images(
                "rendered 1 image",
                self.images.clone(),
            ))
        }
    }

    /// One assistant message whose only tool call is `generate_image`.
    fn calls_image_tool() -> ChatChunk {
        let mut assistant = ChatMessage::assistant("");
        assistant.tool_calls.push(ToolCall {
            function: FunctionCall {
                name: "generate_image".to_string(),
                arguments: json!({}),
            },
        });
        ChatChunk {
            message: Some(assistant),
            images: Vec::new(),
            thinking: false,
            done: true,
            done_reason: None,
            eval_count: None,
            prompt_eval_count: None,
        }
    }

    #[tokio::test]
    async fn model_generated_images_reach_history_disk_and_the_surfaces() {
        // A provider that streams text and then an image, exactly as an
        // image-capable endpoint does through `ChatChunk::images`.
        let image = test_png();
        let (mut agent, _provider, _tmp) = test_agent(vec![vec![
            ChatChunk {
                message: Some(ChatMessage::assistant("here you go")),
                ..image_chunk(Vec::new())
            },
            image_chunk(vec![image.clone()]),
            final_chunk(""),
        ]]);

        let (tx, mut rx) = mpsc::channel(64);
        agent.run_turn("draw a cat", tx).await.expect("turn ok");

        // In history, on the assistant message, as base64 — a vision model
        // needs it there on the next turn.
        let assistant = agent
            .history()
            .iter()
            .rev()
            .find(|message| message.role == Role::Assistant)
            .expect("an assistant message");
        assert_eq!(assistant.content, "here you go");
        assert_eq!(assistant.images.len(), 1);
        assert_eq!(assistant.images[0].b64, image.b64);
        assert_eq!(assistant.images[0].mime, "image/png");

        // Announced to the surfaces as a path, not base64.
        let announced = drain_images(&mut rx);
        assert_eq!(announced.len(), 1);
        let (source, saved) = &announced[0];
        assert_eq!(*source, ImageSource::Assistant);
        assert_eq!(saved.mime, "image/png");
        assert_eq!(
            assistant.images[0].path.as_ref(),
            Some(&saved.path),
            "history records the same path the surfaces were given — a replayed \
             transcript re-derives nothing"
        );

        // And on disk, under this session's image directory.
        assert_eq!(
            std::fs::read(&saved.path).expect("the image file"),
            image.decode().unwrap()
        );
        assert!(
            saved
                .path
                .starts_with(Config::images_dir().unwrap().join(&agent.session().id)),
            "images are session-scoped: {}",
            saved.path.display()
        );
    }

    #[tokio::test]
    async fn tool_images_ride_back_on_a_following_user_message() {
        // The convention every provider tolerates: the tool message carries the
        // text, the images follow on a user message (a `tool` result cannot
        // carry image blocks on OpenAI).
        let image = test_png();
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(ImageTool {
            images: vec![image.clone()],
        }));

        let tmp = TempDir::new();
        let (mut agent, _provider) = test_agent_in(
            &tmp,
            vec![vec![calls_image_tool()], vec![final_chunk("done")]],
            Vec::new(),
            registry,
        );

        let (tx, mut rx) = mpsc::channel(64);
        agent.run_turn("make me a picture", tx).await.expect("turn");

        let history = agent.history();
        let tool_index = history
            .iter()
            .position(|message| message.role == Role::Tool)
            .expect("a tool result");
        assert_eq!(history[tool_index].content, "rendered 1 image");
        assert!(
            history[tool_index].images.is_empty(),
            "the tool message carries the text only"
        );
        let follow_up = &history[tool_index + 1];
        assert_eq!(follow_up.role, Role::User);
        assert!(follow_up.content.contains("generate_image"));
        assert_eq!(follow_up.images.len(), 1, "the model sees it");
        assert_eq!(follow_up.images[0].b64, image.b64);

        // The surfaces get the tool's images twice over: on ToolFinished (as
        // base64, for free) and on Images (as a path, which is what they use).
        let mut finished_images = Vec::new();
        let mut announced = Vec::new();
        while let Ok(event) = rx.try_recv() {
            match event {
                AgentEvent::ToolFinished { output, .. } => finished_images.extend(output.images),
                AgentEvent::Images { source, images } => announced.push((source, images)),
                _ => {}
            }
        }
        assert_eq!(finished_images.len(), 1);
        assert_eq!(finished_images[0].b64, image.b64);
        assert_eq!(announced.len(), 1);
        assert_eq!(announced[0].0, ImageSource::Tool("generate_image".into()));
        let saved = &announced[0].1[0];
        assert_eq!(
            std::fs::read(&saved.path).expect("the image file"),
            image.decode().unwrap()
        );
    }

    #[tokio::test]
    async fn an_oversized_image_is_dropped_with_a_notice_and_never_enters_history() {
        // A runaway image must not melt the context window or the session file.
        let huge = Image::new(
            "A".repeat(crate::llm::MAX_IMAGE_BYTES / 3 * 4 + 8),
            "image/png",
        );
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(ImageTool {
            images: vec![huge, test_png()],
        }));

        let tmp = TempDir::new();
        let (mut agent, _provider) = test_agent_in(
            &tmp,
            vec![vec![calls_image_tool()], vec![final_chunk("done")]],
            Vec::new(),
            registry,
        );

        let (tx, mut rx) = mpsc::channel(64);
        agent.run_turn("make me a picture", tx).await.expect("turn");

        let follow_up = agent
            .history()
            .iter()
            .find(|message| message.role == Role::User && !message.images.is_empty())
            .expect("the images user message");
        assert_eq!(
            follow_up.images.len(),
            1,
            "the oversized image never reaches the model; the sane one does"
        );
        assert_eq!(follow_up.images[0].b64, test_png().b64);

        let (_text, _errors, notices) = drain_events(&mut rx);
        assert!(
            notices.iter().any(|notice| notice.contains("oversized")),
            "the drop is surfaced, not silent: {notices:?}"
        );
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
                images: Vec::new(),
            }),
            images: Vec::new(),
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
        // With last_prompt cleared, context_tokens falls back to a char/4
        // estimate of the remaining history (not the pre-compact 850).
        assert_eq!(
            agent.context_tokens(),
            crate::llm::estimate_history_tokens(agent.history()),
            "post-compact meter uses the remaining-history estimate"
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
        // Progress note is session-persisted so resume / session readers see it.
        let session = agent.session().load_history().expect("session readable");
        assert!(
            session
                .iter()
                .any(|m| m.role == Role::System && m.content.contains(COMPACT_SUMMARY_HEADING)),
            "compact note must land in the session JSONL as a system note"
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
    async fn context_pressure_bands_follow_window_fill() {
        let (agent, _provider, _tmp) = test_agent(vec![]);
        // No window, tiny history → ok via byte proxy.
        let pressure = agent.context_pressure().await;
        assert_eq!(pressure.level, PressureLevel::Ok);
        assert!(pressure.fill < PRESSURE_ELEVATED_FRACTION);

        // Known window + last prompt at 60% → elevated.
        let provider = ScriptedProvider::with_context_window(vec![], 10_000);
        let tmp = TempDir::new();
        let agent = test_agent_with(
            &tmp,
            provider,
            Vec::new(),
            ToolRegistry::with_native_tools(),
        );
        agent.usage.record(Some(6_000), Some(1));
        let pressure = agent.context_pressure().await;
        assert_eq!(pressure.level, PressureLevel::Elevated);
        assert!(pressure.signal_line().contains("elevated"));
        assert!(pressure.signal_line().starts_with(CONTEXT_PRESSURE_HEADING));

        // 75% → high.
        agent.usage.record(Some(7_500), Some(1));
        let pressure = agent.context_pressure().await;
        assert_eq!(pressure.level, PressureLevel::High);

        // 85% → critical (auto-compact band).
        agent.usage.record(Some(8_500), Some(1));
        let pressure = agent.context_pressure().await;
        assert_eq!(pressure.level, PressureLevel::Critical);
        assert!(pressure.signal_line().contains("critical"));
    }

    #[tokio::test]
    async fn compact_tool_runs_mid_turn_and_feeds_result_back() {
        let tmp = TempDir::new();
        let provider = ScriptedProvider::new(vec![
            vec![tool_call_chunk("compact", json!({}))],
            vec![final_chunk("a terse progress note")], // summarization
            vec![final_chunk("done after compact")],
        ]);
        let mut agent = test_agent_with(
            &tmp,
            provider.clone(),
            Vec::new(),
            ToolRegistry::with_native_tools(),
        );
        // Enough history that compact has a middle span.
        for i in 0..(KEEP_RECENT + 5) {
            agent.history.push(ChatMessage::user(format!("old {i}")));
        }

        let (tx, mut rx) = mpsc::channel(64);
        let reason = agent.run_turn("please compact", tx).await.expect("turn ok");
        assert_eq!(reason, DoneReason::Completed);

        // Tool result reached the model as a tool message.
        assert!(
            agent
                .history
                .iter()
                .any(|m| m.role == Role::Tool && m.content.contains("compacted")),
            "compact tool result missing from history"
        );
        assert!(
            agent
                .history
                .iter()
                .any(|m| m.content.contains(COMPACT_SUMMARY_HEADING)),
            "summary note in history"
        );

        // Surfaces saw tool start/finish and a context-size refresh.
        let mut saw_started = false;
        let mut saw_finished = false;
        let mut saw_context = false;
        while let Ok(event) = rx.try_recv() {
            match event {
                AgentEvent::ToolStarted { name, .. } if name == "compact" => saw_started = true,
                AgentEvent::ToolFinished { name, output } if name == "compact" => {
                    saw_finished = true;
                    assert!(!output.is_error, "{}", output.content);
                    assert!(output.content.contains("compacted"), "{}", output.content);
                }
                AgentEvent::ContextSize { .. } => saw_context = true,
                _ => {}
            }
        }
        assert!(saw_started, "ToolStarted for compact");
        assert!(saw_finished, "ToolFinished for compact");
        assert!(saw_context, "ContextSize after compact");
    }

    #[tokio::test]
    async fn elevated_pressure_is_injected_into_the_completion_request() {
        let provider =
            ScriptedProvider::with_context_window(vec![vec![final_chunk("ok")]], 10_000);
        let tmp = TempDir::new();
        let mut agent = test_agent_with(&tmp, provider.clone(), Vec::new(), ToolRegistry::new());
        // 60% fill → elevated signal on the next completion.
        agent.usage.record(Some(6_000), Some(1));

        let (tx, _rx) = mpsc::channel(8);
        agent.run_turn("hi", tx).await.expect("turn ok");

        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let saw_pressure = requests[0]
            .messages
            .iter()
            .any(|m| m.content.starts_with(CONTEXT_PRESSURE_HEADING));
        assert!(saw_pressure, "pressure line must ride the completion request");
        drop(requests);

        // Ephemeral: not left in agent history after the step.
        assert!(
            agent
                .history
                .iter()
                .all(|m| !m.content.starts_with(CONTEXT_PRESSURE_HEADING)),
            "pressure must not linger in history"
        );
        // And never session-persisted.
        let session = agent.session().load_history().expect("session");
        assert!(
            session
                .iter()
                .all(|m| !m.content.starts_with(CONTEXT_PRESSURE_HEADING)),
            "pressure must not hit the session file"
        );
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

    /// Context stewardship is always on: every agent needs to know how to
    /// compact and reset on task change, whether or not `run_command` is in the
    /// registry (headless still auto-compacts and can use subagents/memory).
    #[test]
    fn context_management_instruction_is_always_injected() {
        let tmp = TempDir::new();
        for registry in [ToolRegistry::with_native_tools(), ToolRegistry::new()] {
            let (agent, _provider) = test_agent_in(&tmp, Vec::new(), Vec::new(), registry);
            assert!(
                agent.history[0]
                    .content
                    .contains("## Context management (you own your window)"),
                "context block missing from system prompt"
            );
            assert!(
                agent.history[0].content.contains("`compact`"),
                "must teach the compact tool"
            );
            assert!(
                agent.history[0].content.contains("[context pressure]"),
                "must mention the live pressure signal"
            );
        }
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
                images: Vec::new(),
            }),
            images: Vec::new(),
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
    async fn side_question_answers_without_touching_history_or_session() {
        let (agent, provider, _tmp) = test_agent(vec![vec![final_chunk("forty-two")]]);
        let before = agent.history().len();
        let session_bytes_before = std::fs::metadata(agent.session().path())
            .map(|m| m.len())
            .unwrap_or(0);

        let answer = agent
            .answer_side_question("what is 6 * 7?")
            .await
            .expect("side question answers");
        assert_eq!(answer, "forty-two");

        // History and the session file are untouched — that is the whole point.
        assert_eq!(agent.history().len(), before, "history unchanged");
        let session_bytes_after = std::fs::metadata(agent.session().path())
            .map(|m| m.len())
            .unwrap_or(0);
        assert_eq!(
            session_bytes_after, session_bytes_before,
            "session file unchanged"
        );

        // The forked call carried no tools and included the conversation.
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].tools.is_empty(), "side questions are tool-less");
        assert!(
            requests[0]
                .messages
                .iter()
                .any(|m| m.role == Role::User && m.content.contains("what is 6 * 7?")),
            "question reached the model"
        );
        assert!(
            requests[0]
                .messages
                .iter()
                .any(|m| m.role == Role::User && m.content.contains("NO tools")),
            "system reminder constrains the model"
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
        assert_eq!(
            agent.usage().session_totals(),
            (0, 0),
            "session token counters zeroed with the wiped conversation"
        );
        // context_tokens falls back to an estimate of the remaining system
        // prompt (history was truncated to 1).
        assert!(
            agent.context_tokens() > 0,
            "post-clear meter reflects the system prompt only"
        );

        // The real sessions dir was touched: clean up the empty file.
        let _ = std::fs::remove_file(new_session_path);
    }

    #[tokio::test]
    async fn default_budget_runs_past_the_old_step_ceiling() {
        // 30 tool-calling steps, then a final answer. The budget used to stop
        // this turn at 25; unlimited (the default) carries it to the end.
        let tmp = TempDir::new();
        let (registry, calls) = recording_registry();
        let mut responses: Vec<Vec<ChatChunk>> = (0..30)
            .map(|_| vec![tool_call_chunk("echo", json!({}))])
            .collect();
        responses.push(vec![final_chunk("done")]);
        let (mut agent, _provider) = test_agent_in(&tmp, responses, Vec::new(), registry);
        assert_eq!(agent.config.max_steps, StepBudget::UNLIMITED);

        let (tx, _rx) = mpsc::channel(256);
        let reason = agent.run_turn("go", tx).await.expect("turn ok");

        assert_eq!(reason, DoneReason::Completed);
        assert_eq!(calls.lock().unwrap().len(), 30, "every step ran");
    }

    #[tokio::test]
    async fn configured_cap_still_ends_the_turn() {
        let tmp = TempDir::new();
        let (registry, calls) = recording_registry();
        // More tool calls than the cap allows: the loop must stop at the cap.
        let responses: Vec<Vec<ChatChunk>> = (0..3)
            .map(|_| vec![tool_call_chunk("echo", json!({}))])
            .collect();
        let (mut agent, _provider) = test_agent_in(&tmp, responses, Vec::new(), registry);
        agent.config.max_steps = StepBudget::new(3);

        let (tx, _rx) = mpsc::channel(256);
        let reason = agent.run_turn("go", tx).await.expect("turn ok");

        assert_eq!(reason, DoneReason::MaxSteps);
        assert_eq!(calls.lock().unwrap().len(), 3, "stopped at the cap");
    }

    /// Streaming (not-done) chunk carrying a text or thinking delta.
    fn delta_chunk(content: &str, thinking: bool) -> ChatChunk {
        ChatChunk {
            message: Some(ChatMessage::assistant(content)),
            images: Vec::new(),
            thinking,
            done: false,
            done_reason: None,
            eval_count: None,
            prompt_eval_count: None,
        }
    }

    #[tokio::test]
    async fn streaming_assembles_split_deltas_and_keeps_thinking_out_of_history() {
        let (mut agent, _provider, _tmp) = test_agent(vec![vec![
            delta_chunk("pondering deeply", true),
            delta_chunk("Hel", false),
            delta_chunk("lo world", false),
            final_chunk(""),
        ]]);

        let (tx, mut rx) = mpsc::channel(64);
        let reason = agent.run_turn("hi", tx).await.expect("turn ok");
        assert_eq!(reason, DoneReason::Completed);

        let mut text = String::new();
        let mut thinking = String::new();
        while let Ok(event) = rx.try_recv() {
            match event {
                AgentEvent::TextDelta(delta) => text.push_str(&delta),
                AgentEvent::ThinkingDelta(delta) => thinking.push_str(&delta),
                _ => {}
            }
        }
        assert_eq!(text, "Hello world", "split deltas reassemble in order");
        assert_eq!(thinking, "pondering deeply", "reasoning is surfaced");

        let assistant = agent
            .history()
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant)
            .expect("assistant message");
        assert_eq!(assistant.content, "Hello world");
        let persisted = agent.session().load_messages().expect("session readable");
        assert!(
            persisted.iter().all(|m| !m.content.contains("pondering")),
            "thinking never reaches history or disk"
        );
    }

    #[tokio::test]
    async fn a_transient_stream_failure_emits_stream_retrying_then_recovers() {
        let tmp = TempDir::new();
        let provider = ScriptedProvider::flaky(1, vec![vec![final_chunk("second try")]]);
        let mut agent =
            test_agent_with(&tmp, Arc::clone(&provider), Vec::new(), ToolRegistry::new());
        agent.config.retry_base_secs = 0;
        agent.config.retry_max_secs = 0;

        let (tx, mut rx) = mpsc::channel(256);
        let reason = agent.run_turn("go", tx).await.expect("turn ok");
        assert_eq!(reason, DoneReason::Completed);
        assert_eq!(provider.requests.lock().unwrap().len(), 2, "retried once");

        let mut retrying = 0;
        let mut text = String::new();
        let mut errors = Vec::new();
        while let Ok(event) = rx.try_recv() {
            match event {
                AgentEvent::StreamRetrying => retrying += 1,
                AgentEvent::TextDelta(delta) => text.push_str(&delta),
                AgentEvent::Error(message) => errors.push(message),
                _ => {}
            }
        }
        assert_eq!(
            retrying, 1,
            "consumers are told to drop their partial buffer exactly once"
        );
        assert_eq!(text, "second try");
        assert!(
            errors.iter().any(|e| e.contains("retrying")),
            "the outage is surfaced: {errors:?}"
        );
        assert!(
            !agent.llm_breaker.is_open(),
            "one flake never trips the breaker"
        );
    }

    #[tokio::test]
    async fn a_failed_summary_falls_back_to_truncating_the_middle() {
        // The summarization call streams an empty reply, which counts as a
        // summary failure — the middle span is dropped instead.
        let (mut agent, _provider, _tmp) = test_agent(vec![vec![final_chunk("")]]);
        let extra = KEEP_RECENT + 5;
        for i in 0..extra {
            agent.history.push(ChatMessage::user(format!("msg {i}")));
        }
        let before = agent.history.len();

        let outcome = agent.compact_now().await;
        match &outcome {
            CompactOutcome::Truncated { count, error } => {
                assert_eq!(*count, before - 1 - KEEP_RECENT);
                assert!(error.contains("empty summary"), "{error}");
            }
            other => panic!("expected truncation, got {other:?}"),
        }
        assert!(outcome.describe().contains("truncation"));

        assert_eq!(agent.history.len(), 1 + KEEP_RECENT);
        assert!(
            agent
                .history
                .iter()
                .all(|m| !m.content.contains("[Compacted progress summary]")),
            "no summary note on the fallback path"
        );
        assert_eq!(agent.history[0].role, Role::System);
        assert_eq!(
            agent.history.last().unwrap().content,
            format!("msg {}", extra - 1),
            "the recent tail survives verbatim"
        );
        assert_eq!(
            agent.usage().last_prompt_tokens(),
            None,
            "stale prompt size cleared even on the fallback path"
        );
    }

    #[tokio::test]
    async fn rolling_summarization_chains_oversized_spans_through_chunk_summaries() {
        let (mut agent, provider, _tmp) = test_agent(vec![
            vec![final_chunk("summary of part one")],
            vec![final_chunk("summary of everything")],
        ]);
        // One middle message larger than a single summarization chunk, made of
        // multibyte characters so the split must respect char boundaries.
        agent.history.push(ChatMessage::user("é".repeat(15_000)));
        for i in 0..KEEP_RECENT {
            agent.history.push(ChatMessage::user(format!("tail {i}")));
        }

        let outcome = agent.compact_now().await;
        assert_eq!(outcome, CompactOutcome::Summarized(1));

        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2, "one summarization pass per chunk");
        let second_blob = &requests[1].messages[1].content;
        assert!(
            second_blob.contains("[Progress summary of the transcript so far]"),
            "the second pass sees the first pass's summary"
        );
        assert!(second_blob.contains("summary of part one"));
        assert!(second_blob.contains("[Transcript continues]"));
        drop(requests);

        assert!(
            agent.history.iter().any(|m| m
                .content
                .contains("[Compacted progress summary]\nsummary of everything")),
            "the final rolling summary is what lands in history"
        );
    }

    #[test]
    fn leaving_plan_mode_also_leaves_omakase() {
        let tmp = TempDir::new();
        let (mut agent, _provider) =
            test_agent_in(&tmp, Vec::new(), Vec::new(), ToolRegistry::new());

        agent.set_omakase(true);
        assert!(agent.plan_mode(), "omakase implies plan mode");
        assert!(agent.omakase());
        assert!(agent.history[0].content.contains("Omakase"));
        assert!(agent.history[0].content.contains("PLAN MODE"));

        agent.set_plan_mode(false);
        assert!(!agent.omakase(), "no omakase without the plan phase");
        assert!(!agent.history[0].content.contains("Omakase"));
        assert!(!agent.history[0].content.contains("PLAN MODE"));
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
                max_steps: StepBudget::new(1),
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
