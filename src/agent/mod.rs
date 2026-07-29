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
    /// drives auto-compaction — needs a *reported* last prompt over 80% of a
    /// known window; the byte threshold is only the fallback gate when the
    /// window is unknown. A known window makes tokens the authoritative
    /// measure — the byte proxy (48 KB default, sized for small local models)
    /// would otherwise scream "critical" at a few percent of a large window.
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

        let auto_critical = match window {
            Some(w) if w > 0 => match last_prompt {
                Some(prompt) => prompt as f64 > f64::from(w) * COMPACT_WINDOW_FRACTION,
                None => false,
            },
            _ => byte_total > threshold,
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
    /// compaction: the last model call's reported prompt size exceeds
    /// [`COMPACT_WINDOW_FRACTION`] of the provider's known context window,
    /// or — when no window is known — the serialized history exceeds the
    /// byte threshold.
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
mod tests;
