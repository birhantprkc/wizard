//! Agent loop: build messages → stream completion → parse tool calls →
//! execute tools → repeat until done or `max_steps`.
//!
//! The loop is UI-agnostic: it emits [`AgentEvent`]s over a channel that the
//! Ratatui TUI (genie) or the headless runner (sovereign) consumes.

pub mod mission;
pub mod prompts;
pub mod session;
pub mod subagent;

use std::io::Write as _;
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

/// The tool-calling agent. Owns the conversation history, the model client,
/// the tool dispatcher, and session persistence.
pub struct Agent {
    client: Arc<dyn LlmProvider>,
    /// Active model tag (from `config.active().model`); switched by
    /// [`Agent::set_model`].
    model: String,
    /// Tool-call pipeline; owns the registry and the failure breakers.
    dispatcher: Dispatcher,
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
    /// Project `AGENTS.md` / `WIZARD.md` contents, if present.
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
}

/// Number of most-recent messages preserved verbatim when compacting history.
const KEEP_RECENT: usize = 10;

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
    /// prompt).
    pub fn new(
        client: Arc<dyn LlmProvider>,
        registry: ToolRegistry,
        config: Config,
        skills: Vec<Skill>,
        project_root: PathBuf,
        session: Session,
        native_tools: bool,
    ) -> Result<Self> {
        let agents_md = read_project_instructions(&project_root);
        let memory_index = read_memory_index(&project_root);
        let model = config.active().model;
        let mut load_warning = None;
        let prior = session
            .load_messages()
            .unwrap_or_else(|err| {
                tracing::warn!("could not load session {}: {err}", session.path().display());
                load_warning = Some(format!(
                    "previous session {} could not be read ({err}); starting fresh",
                    session.path().display()
                ));
                Vec::new()
            })
            .into_iter()
            .filter(|message| message.role != Role::System)
            .collect::<Vec<_>>();

        let mut agent = Self {
            client,
            model,
            dispatcher: Dispatcher::new(registry, config.mode),
            mode: config.mode,
            config,
            history: Vec::new(),
            session,
            ctx: ToolContext::new(project_root),
            native_tools,
            skills,
            agents_md,
            memory_index,
            deadline: None,
            load_warning,
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

    /// Conversation history (system prompt included).
    pub fn history(&self) -> &[ChatMessage] {
        &self.history
    }

    /// Session this agent persists to.
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Set (or clear) the wall-clock deadline for this run (`--max-hours`).
    pub fn set_deadline(&mut self, deadline: Option<Instant>) {
        self.deadline = deadline;
    }

    /// `/clear`: drop everything but the system prompt and start a fresh
    /// session file.
    pub fn clear(&mut self) -> Result<()> {
        self.session = Session::create(&Config::sessions_dir()?)?;
        self.history.truncate(1);
        self.dispatcher.reset_failures();
        Ok(())
    }

    /// Swap the tool registry (after `/reload` or `/evolve`). Refreshes the
    /// system prompt so the JSON tool protocol's tool list stays current.
    pub fn set_registry(&mut self, registry: ToolRegistry) {
        self.dispatcher.set_registry(registry);
        self.refresh_system_prompt();
    }

    /// Switch models mid-session (`/model`) without resetting conversation
    /// context. `native_tools` is the new model's tool-calling capability
    /// (probe with [`OllamaClient::supports_native_tools`]); the system
    /// prompt is recomposed so the JSON tool protocol section matches.
    pub fn set_model(&mut self, model: String, native_tools: bool) {
        self.config.model = model.clone();
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

    /// Append to history and persist (system messages are not persisted —
    /// they are recomposed on resume).
    fn push(&mut self, message: ChatMessage) {
        if message.role != Role::System
            && let Err(err) = self.session.append(&message)
        {
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
        match self.turn_inner(input, &events).await {
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
        }
    }

    async fn turn_inner(
        &mut self,
        input: &str,
        events: &mpsc::Sender<AgentEvent>,
    ) -> Result<DoneReason> {
        self.push(ChatMessage::user(input));
        self.compact_if_needed(events).await;
        let max_steps = self.config.max_steps.max(1);

        for step in 1..=max_steps {
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

            let (mut content, mut tool_calls) = self.stream_completion_with_retry(events).await?;

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
                let (retry_content, retry_calls) = retried?;
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

            for call in &tool_calls {
                match self.dispatch_call(call, events).await {
                    None => {}
                    Some(reason) => return Ok(reason),
                }
            }

            if !emit(events, AgentEvent::StepCompleted { step }).await {
                return Ok(DoneReason::Stopped);
            }
        }

        Ok(DoneReason::MaxSteps)
    }

    /// Stream one completion, forwarding text deltas and collecting tool
    /// calls.
    async fn stream_completion(
        &self,
        events: &mpsc::Sender<AgentEvent>,
    ) -> Result<(String, Vec<ToolCall>)> {
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
            }),
        };

        let mut stream = self
            .client
            .chat_stream(request)
            .await
            .context("starting chat completion")?;

        let mut content = String::new();
        let mut tool_calls = Vec::new();
        while let Some(chunk) = stream.next().await {
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
            if chunk.done {
                break;
            }
        }
        Ok((content, tool_calls))
    }

    /// [`stream_completion`] with sleep-and-wake exponential backoff so a
    /// transient LLM outage (server down, rate-limited, mid-stream drop)
    /// pauses and retries instead of aborting the run. In continuous mode it
    /// retries indefinitely; otherwise it gives up after ~6 attempts. A
    /// non-transient error (e.g. missing model) returns immediately.
    async fn stream_completion_with_retry(
        &self,
        events: &mpsc::Sender<AgentEvent>,
    ) -> Result<(String, Vec<ToolCall>)> {
        let mut attempt: u32 = 0;
        loop {
            match self.stream_completion(events).await {
                Ok(result) => return Ok(result),
                Err(err) => {
                    // Default to transient so mid-stream interruptions (which
                    // are not typed `OllamaError`) also retry.
                    let transient = err
                        .downcast_ref::<crate::llm::ollama::OllamaError>()
                        .map(|e| e.is_transient())
                        .unwrap_or(true);
                    if !transient {
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
                    let _ = emit(
                        events,
                        AgentEvent::Error(format!(
                            "LLM unavailable ({err:#}); sleeping {secs}s then retrying (attempt {n})"
                        )),
                    )
                    .await;
                    tokio::time::sleep(Duration::from_secs(secs)).await;
                    attempt += 1;
                }
            }
        }
    }

    /// Keep history bounded so the agent can run indefinitely. When the
    /// serialized history exceeds `compact_threshold_bytes`, summarize the
    /// middle span (everything between the system prompt and the last
    /// [`KEEP_RECENT`] messages) into a single progress note. Best-effort:
    /// a summarization failure falls back to dropping the middle span. Never
    /// aborts the turn.
    async fn compact_if_needed(&mut self, events: &mpsc::Sender<AgentEvent>) {
        let total: usize = self.history.iter().map(|msg| msg.content.len()).sum();
        if total <= self.config.compact_threshold_bytes {
            return;
        }
        // Need history[0] (system prompt) + a non-empty middle + the recent tail.
        if self.history.len() <= KEEP_RECENT + 1 {
            return;
        }
        let start = 1;
        let end = self.history.len() - KEEP_RECENT;
        if start >= end {
            return;
        }
        let middle_count = end - start;

        // Render the middle span as one text blob, capped to ~20k chars.
        let mut blob = String::new();
        for msg in &self.history[start..end] {
            let role = match msg.role {
                Role::System => "system",
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "tool",
            };
            blob.push_str(role);
            blob.push_str(": ");
            blob.push_str(&msg.content);
            blob.push('\n');
            if blob.len() >= 20_000 {
                blob.truncate(20_000);
                break;
            }
        }

        match self.summarize_transcript(&blob).await {
            Ok(summary) => {
                let replacement =
                    ChatMessage::system(format!("[Compacted progress summary]\n{summary}"));
                self.history
                    .splice(start..end, std::iter::once(replacement));
                let _ = emit(
                    events,
                    AgentEvent::Error(format!("compacted {middle_count} messages → summary")),
                )
                .await;
            }
            Err(err) => {
                // Fall back to truncation: drop the middle span outright.
                self.history.drain(start..end);
                let _ = emit(
                    events,
                    AgentEvent::Error(format!(
                        "compacted {middle_count} messages by truncation (summary LLM failed: {err:#})"
                    )),
                )
                .await;
            }
        }
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
                     run, what worked/failed, and open next steps. Be terse and factual.",
                ),
                ChatMessage::user(blob.to_string()),
            ],
            tools: Vec::new(),
            stream: true,
            options: Some(ChatOptions {
                temperature: Some(0.2),
                num_ctx: None,
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
        if let Some(output) = &outcome.output {
            self.push(self.tool_feedback(&call.function.name, output));
        }
        if let Some(nudge) = outcome.nudge {
            self.push(ChatMessage::system(nudge));
        }
        outcome.done
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
}

/// Read project-level instructions: `AGENTS.md`, falling back to
/// `WIZARD.md`.
fn read_project_instructions(project_root: &Path) -> Option<String> {
    for name in ["AGENTS.md", "WIZARD.md"] {
        let path = project_root.join(name);
        match std::fs::read_to_string(&path) {
            Ok(contents) if !contents.trim().is_empty() => return Some(contents),
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => tracing::warn!("could not read {}: {err}", path.display()),
        }
    }
    None
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
        let outcome = crate::server::ensure_running(&active, &|line: &str| wait.update(line)).await;
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

    let subagents_dir = Config::subagents_dir()?;
    let subagent_configs = subagent::available_configs(&subagents_dir);
    let base = Arc::new(base);
    let mut registry = subagent::scoped_registry(&base, None);
    registry.register(Arc::new(subagent::SpawnSubagentTool::new(
        subagent_configs,
        Arc::clone(&client),
        Arc::clone(&base),
    )));
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

    let sessions_dir = Config::sessions_dir()?;
    let session = if resume {
        match Session::open_latest(&sessions_dir)? {
            Some(session) => session,
            None => Session::create(&sessions_dir)?,
        }
    } else {
        Session::create(&sessions_dir)?
    };

    Agent::new(
        client,
        registry,
        config.clone(),
        skills,
        project_root.to_path_buf(),
        session,
        native_tools,
    )
}

/// Sovereign-mode headless runner: builds an [`Agent`] and drives it in an
/// outer loop. The goal comes from `cli.prompt`, or (on a self-evolve
/// re-exec) from the persisted [`mission::Mission`]. With `--continuous` it
/// runs perpetually — persisting a mission, self-directing the next action
/// after each completed cycle, sleeping-and-waking through transient LLM
/// outages, compacting context, and re-exec'ing itself after a self-evolve —
/// until stopped via `.wizard/loop-control`, `--max-hours`, or the circuit
/// breaker. Otherwise it honors the `--loop N` bound. Prints progress to
/// stdout instead of the TUI.
pub async fn run_headless(config: Config, cli: Cli) -> Result<()> {
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

    let active = config.active();
    let model = active.model.clone();
    let endpoint = active.base_url.clone();

    let mut agent = build_headless_agent(&config, &project_root, cli.resume).await?;
    agent.set_deadline(
        cli.max_hours
            .map(|hours| Instant::now() + Duration::from_secs_f64(hours * 3600.0)),
    );

    // Busy spinner ("Conjuring…") shown while the model thinks or a tool
    // runs, hidden while output streams. Shared with the printer task; a
    // no-op when stderr is not a terminal.
    let spinner = Arc::new(crate::progress::TurnSpinner::new());

    let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);
    let printer_spinner = Arc::clone(&spinner);
    let printer = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            match event {
                AgentEvent::TextDelta(delta) => {
                    printer_spinner.hide();
                    print!("{delta}");
                    let _ = std::io::stdout().flush();
                }
                AgentEvent::ThinkingDelta(delta) => {
                    // ANSI faint so reasoning reads as background noise.
                    printer_spinner.hide();
                    print!("\x1b[2m{delta}\x1b[0m");
                    let _ = std::io::stdout().flush();
                }
                AgentEvent::ToolStarted { name, args } => {
                    printer_spinner.println(&format!("\n→ {name} {args}"));
                    // The tool may run for a while: keep the verb spinning.
                    printer_spinner.show();
                }
                AgentEvent::ToolFinished { name, output } => {
                    let status = if output.is_error { "error" } else { "ok" };
                    printer_spinner.println(&format!("← {name} [{status}]"));
                    // Back to the model: it is thinking about the result.
                    printer_spinner.show();
                }
                AgentEvent::StepCompleted { step } => {
                    tracing::debug!("step {step} completed");
                }
                AgentEvent::Error(message) => {
                    printer_spinner.hide();
                    eprintln!("\nwizard error: {message}");
                }
                AgentEvent::Done { reason } => {
                    printer_spinner.hide();
                    println!("\n[turn done: {reason:?}]");
                }
            }
        }
    });

    println!(
        "wizard {} — model {model} @ {endpoint} — task: {goal}",
        config.mode
    );

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
            spinner.println(&format!("\n=== cycle {iteration} ==="));
        } else if max_iterations > 1 {
            spinner.println(&format!("\n=== iteration {iteration}/{max_iterations} ==="));
        }

        // Fresh verb per turn (same mechanism as the TUI's busy spinner), so
        // one turn reads as one activity.
        let verb_seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos() as u64)
            .wrapping_add(u64::from(iteration));
        spinner.set_verb(config.ui.spinner_verb(verb_seed));
        spinner.show();

        let turn_started = Instant::now();
        let turn_prompt = input.clone();
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
                    DoneReason::Stopped | DoneReason::TimeLimit | DoneReason::CircuitBreaker => {
                        break;
                    }
                }
            }
            Err(err) => {
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

    drop(tx);
    let _ = printer.await;
    spinner.finish();

    if reexec_after {
        use std::os::unix::process::CommandExt;
        let exe = std::env::current_exe().context("locating current executable for re-exec")?;
        println!("[re-exec into evolved binary {}]", exe.display());
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
    println!("[run finished: {final_reason:?}]");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use futures_util::stream;

    use super::*;
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
    }

    impl ScriptedProvider {
        fn new(responses: Vec<Vec<ChatChunk>>) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(responses.into()),
                requests: Mutex::new(Vec::new()),
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

    fn test_agent(responses: Vec<Vec<ChatChunk>>) -> (Agent, Arc<ScriptedProvider>, TempDir) {
        let tmp = TempDir::new();
        let provider = ScriptedProvider::new(responses);
        let session = Session::create(&tmp.0).expect("create session");
        let agent = Agent::new(
            provider.clone(),
            ToolRegistry::new(),
            Config::default(),
            Vec::new(),
            tmp.0.clone(),
            session,
            true,
        )
        .expect("build agent");
        (agent, provider, tmp)
    }

    /// Drain a finished turn's events into (text, errors).
    fn drain_events(rx: &mut mpsc::Receiver<AgentEvent>) -> (String, Vec<String>) {
        let mut text = String::new();
        let mut errors = Vec::new();
        while let Ok(event) = rx.try_recv() {
            match event {
                AgentEvent::TextDelta(delta) => text.push_str(&delta),
                AgentEvent::Error(message) => errors.push(message),
                _ => {}
            }
        }
        (text, errors)
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

        let (text, errors) = drain_events(&mut rx);
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

        let (text, errors) = drain_events(&mut rx);
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
}
