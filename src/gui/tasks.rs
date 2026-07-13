//! In-process task manager: one lazily-built [`Agent`] per GUI task.
//!
//! A task is a wizard session; the manager keeps a keep-warm map of live
//! agents (LRU-bounded — sessions persist on disk, so an evicted agent is
//! rebuilt on demand). Each task runs on a dedicated worker that owns the
//! agent and executes one turn at a time; its [`AgentEvent`]s fan out as
//! the protocol's JSON [`Frame`]s through [`TaskShared`], which buffers the
//! current turn for replay when no WebSocket is attached and holds the
//! plan/interview gates until a client frame (or a socket drop) resolves
//! them.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::agent::session::Session;
use crate::agent::{
    Agent, AgentEvent, CancelHandle, DoneReason, PlanVerdict, build_headless_agent_for_session,
};
use crate::config::{Config, Mode};
use crate::gui::settings::ConfigStore;
use crate::gui::transcript::summarize_tool;
use crate::images::ImageRef;
use crate::llm::provider::NATIVE_TOOLS_ON_PROBE_FAILURE;
use crate::mcp::McpManager;
use crate::session_registry::{self, SessionRecord, SessionState};
use crate::tools::CommandDispatch;
use crate::tools::todo::{TodoItem, TodoStatus};

/// Keep at most this many agents warm; beyond it the least-recently-used
/// idle task is retired (its session persists, so it rebuilds on demand).
const MAX_WARM_TASKS: usize = 4;

/// Cap on buffered frames per turn, so a runaway turn with no socket
/// attached cannot grow without bound (the oldest frames are dropped).
const MAX_BUFFERED_FRAMES: usize = 10_000;

/// How often every live task's registry heartbeat is refreshed. Must stay well
/// under [`session_registry::STALE_SECS`], or a task that sits idle between
/// turns ages out of `/dashboard` while it is still there. The TUI refreshes on
/// the same cadence, from its draw loop.
const HEARTBEAT: Duration = Duration::from_secs(3);

/// The slash commands the GUI's own executor runs — the `server` rows of
/// [`crate::gui::server::COMMANDS`], which a test holds it to. `run_command`
/// refuses anything outside this set at *call* time, so a command with nowhere
/// to land in a browser comes back to the model as a tool error rather than a
/// no-op it never hears about.
pub(crate) const AGENT_COMMANDS: &[&str] = &["compact", "cost", "model", "mode", "help"];

/// A server→client WebSocket frame (see `docs/gui-protocol.md`).
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Frame {
    TextDelta {
        text: String,
    },
    ThinkingDelta {
        text: String,
    },
    ToolStarted {
        call_id: u64,
        name: String,
        args: Value,
    },
    ToolFinished {
        call_id: u64,
        name: String,
        ok: bool,
        summary: String,
    },
    /// Images the turn produced, already written to disk by the agent
    /// (`~/.wizard/images/<session>/`). `source` is `"assistant"` (the model
    /// generated them) or `"tool"`, in which case `tool` names it. Each entry
    /// carries `path`, `mime` and `bytes` — a reference, never base64, so a
    /// buffered turn of frames cannot balloon; the client fetches the file
    /// itself to display it or link to it full size.
    Images {
        source: &'static str,
        tool: Option<String>,
        images: Vec<ImageRef>,
    },
    /// [`Frame::Images`], scoped to a subagent run — the run's pane renders
    /// them where the parent's chat renders its own.
    SubagentRunImages {
        run: u64,
        source: &'static str,
        tool: Option<String>,
        images: Vec<ImageRef>,
    },
    Todo {
        items: Vec<TodoRow>,
    },
    /// Session-lifetime token totals, as the backend reported them. Every model
    /// call adds to these; they are what `/cost` bills on.
    Usage {
        prompt_tokens: u64,
        completion_tokens: u64,
    },
    /// Tokens that will load into the *next* model call, against the active
    /// model's context window when the provider names one. Not a running total —
    /// compaction and `/clear` make it fall — so it is a separate frame from
    /// [`Frame::Usage`] rather than a field on it.
    Context {
        tokens: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        window: Option<u32>,
    },
    State {
        state: &'static str,
    },
    PlanReady {
        plan: String,
    },
    Interview {
        questions: Vec<String>,
    },
    TaskEvent {
        phase: &'static str,
        label: String,
    },
    Subagent {
        phase: &'static str,
        name: String,
        task: String,
    },
    /// A subagent run began. Every `subagent_run_*` frame below carries the
    /// same `run`, so concurrent runs — even two of the same subagent — demux
    /// into separate panes instead of interleaving. `bg` is the
    /// background-registry id of a detached run.
    SubagentRunStarted {
        run: u64,
        bg: Option<u32>,
        name: String,
        task: String,
    },
    /// One of the subagent's own messages (its narration between tool calls).
    SubagentRunText {
        run: u64,
        text: String,
    },
    /// The subagent started a tool call. `call_id` pairs it with its
    /// `subagent_run_tool_finished`, exactly as the parent's tool frames pair.
    SubagentRunToolStarted {
        run: u64,
        call_id: u64,
        name: String,
        args: Value,
    },
    SubagentRunToolFinished {
        run: u64,
        call_id: u64,
        name: String,
        ok: bool,
        summary: String,
    },
    /// The subagent completed one step (model round-trip), 1-based.
    SubagentRunStep {
        run: u64,
        step: u32,
    },
    /// The run ended. `completed` is false when it hit its step budget;
    /// `error` is set when it died — so a failed run is distinguishable from
    /// one that merely ran out of steps.
    SubagentRunDone {
        run: u64,
        completed: bool,
        output: String,
        steps_used: u32,
        error: Option<String>,
    },
    Notice {
        text: String,
    },
    Error {
        message: String,
    },
    Retrying {
        attempt: u32,
    },
    Done {
        reason: &'static str,
    },
}

/// One todo item in a [`Frame::Todo`].
#[derive(Debug, Serialize)]
pub struct TodoRow {
    pub text: String,
    pub done: bool,
    pub active: bool,
}

impl TodoRow {
    fn from_item(item: &TodoItem) -> Self {
        Self {
            text: item.content.clone(),
            done: item.status == TodoStatus::Completed,
            active: item.status == TodoStatus::InProgress,
        }
    }
}

/// What a managed task is doing, for `/api/tasks` and the `state` frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Working,
    NeedsInput,
    Idle,
    /// The agent could not be built (unreachable provider, bad session) or
    /// the last turn ended in an error. A later `user_message` retries.
    Failed,
}

impl TaskState {
    /// The `/api/tasks` and `state`-frame state string.
    pub fn as_str(self) -> &'static str {
        match self {
            TaskState::Working => "working",
            TaskState::NeedsInput => "needs_input",
            TaskState::Idle => "idle",
            TaskState::Failed => "failed",
        }
    }
}

/// The `done` frame's reason string for a finished turn. The agent maps a
/// mid-turn provider failure to `Error` + `Done{Stopped}` — the same shape
/// as a client cancel — so a stop after an error, with no cancel requested,
/// reads as `error` rather than `cancelled`.
fn done_reason(reason: DoneReason, error_seen: bool, cancel_requested: bool) -> &'static str {
    match reason {
        DoneReason::Completed => "completed",
        DoneReason::Stopped if error_seen && !cancel_requested => "error",
        DoneReason::Stopped => "cancelled",
        DoneReason::MaxSteps => "max_steps",
        DoneReason::TimeLimit | DoneReason::CircuitBreaker => "error",
    }
}

/// One queued turn: the user text, an optional model override, and the
/// attachments the client uploaded for it. Both path lists have already been
/// verified to sit inside the stores wizard wrote them to
/// ([`crate::gui::server::verify_attachments`]) — the worker takes them as
/// given.
#[derive(Debug, Default)]
pub struct TurnRequest {
    pub text: String,
    pub model: Option<String>,
    /// Images to attach to the user message (the vision path).
    pub images: Vec<PathBuf>,
    /// Non-image attachments. Appended to the text as `@/abs/path` tokens, so
    /// the `@file` expansion every surface shares is what reads them.
    pub files: Vec<PathBuf>,
}

/// One queued server-side slash command (`GET /api/commands`, `where: "server"`).
#[derive(Debug)]
pub struct CommandRequest {
    pub name: String,
    pub args: String,
}

/// What the worker takes off its queue. Commands and turns share one channel,
/// and one slot, because both need `&mut Agent`: `/compact` running beside a
/// turn would be two mutable borrows of the same conversation.
#[derive(Debug)]
enum WorkerRequest {
    Turn(TurnRequest),
    Command(CommandRequest),
}

/// State shared between a task's worker, the WebSocket handler, and the
/// HTTP handlers. All mutation goes through the inner mutex; the async
/// sides never hold it across an await.
pub struct TaskShared {
    pub id: String,
    pub cwd: PathBuf,
    /// When this task went live, for its registry record.
    started_unix: u64,
    /// Where this task heartbeats (`~/.wizard/running/`), so `/dashboard` and
    /// every other Wizard on the machine sees it while it is alive. `None` for a
    /// task no manager owns — it has no session anyone could attach to, and must
    /// not advertise one.
    registry: Option<PathBuf>,
    state: Mutex<SharedState>,
}

struct SharedState {
    task_state: TaskState,
    /// Dashboard label: the first line of the task's first message, or the
    /// workspace name until it has one. The TUI names its session the same way.
    name: String,
    /// The first message has landed, so `name` is the task's own and no later
    /// turn overwrites it.
    named: bool,
    /// The posture the agent runs in, mirrored here for the registry record;
    /// `/mode` moves it.
    mode: String,
    /// One-line summary of what the task is doing, for the dashboard row.
    activity: String,
    /// Slash commands the agent asked for through `run_command` during the
    /// current turn. `run_turn` holds `&mut Agent` for its whole duration, so
    /// nothing can be applied until the borrow ends; the worker drains this the
    /// moment the turn returns, which is where the TUI applies its own queue and
    /// for the same reason.
    pending_commands: Vec<String>,
    /// A turn is queued or running; set by the enqueuer, cleared by the
    /// worker, so "one in-flight turn per task" holds across the gap.
    turn_active: bool,
    /// Serialized frames of the current turn, replayed on WS attach.
    buffer: VecDeque<String>,
    /// The attached socket, if any (one per task; a new attach replaces it).
    subscriber: Option<mpsc::UnboundedSender<String>>,
    /// Bumped per attach so a stale socket's detach cannot clear its
    /// replacement.
    subscriber_gen: u64,
    pending_plan: Option<oneshot::Sender<PlanVerdict>>,
    pending_interview: Option<oneshot::Sender<Option<Vec<String>>>>,
    cancel: Option<CancelHandle>,
    next_call_id: u64,
    /// In-flight tool calls by name (FIFO per name), pairing `tool_finished`
    /// frames — which carry no id of their own — with their `tool_started`
    /// call id and arguments.
    open_calls: HashMap<String, VecDeque<(u64, Value)>>,
    /// The same, per subagent run: a subagent's calls pair among themselves,
    /// never with the parent's or another run's. Not cleared at turn start —
    /// a background run's calls can straddle the end of the turn that spawned
    /// it — but dropped when its run ends.
    open_subagent_calls: HashMap<(u64, String), VecDeque<(u64, Value)>>,
    /// The subagent runs still going, in start order (see [`LiveRun`]).
    live_runs: Vec<LiveRun>,
    /// Bumped per turn, so [`TaskShared::attach`] can tell which live runs the
    /// replay buffer still carries the `started` frame of.
    turn_seq: u64,
    /// Stream retries within the current turn (`retrying` frame's attempt
    /// counter).
    retries: u32,
    /// An `error` frame was emitted during the current turn; with no cancel
    /// requested, its `Done{Stopped}` reads as `error` and the task ends
    /// failed rather than idle.
    turn_error_seen: bool,
    /// A client `cancel` frame arrived during the current turn, so its stop
    /// really is a cancellation.
    turn_cancel_requested: bool,
    /// The current turn's `done` frame carried reason `error`, so
    /// [`TaskShared::finish_turn`] ends it failed rather than idle.
    turn_failed: bool,
    model: String,
    /// Context window of the active model, when the provider names one. Read
    /// once per model (the probe can be an HTTP round trip) and carried on
    /// every [`Frame::Context`]; `None` stays absent from the frame rather
    /// than becoming a guessed default.
    context_window: Option<u32>,
    /// The last context reading this task emitted. Kept so a client attaching
    /// between turns is told the size of the history it is looking at: the
    /// reading is otherwise only produced *during* a turn, and the replay
    /// buffer it lived in is cleared at the start of the next one.
    context_tokens: Option<u64>,
}

/// One subagent run that has not reported back yet. A background run outlives
/// the turn that spawned it, and the next turn clears its `started` frame out
/// of the replay buffer — so the frame is kept here, and [`TaskShared::attach`]
/// re-announces the runs the replay it just sent does not cover. Without it a
/// client that reconnects mid-run has no row to stream the run into.
struct LiveRun {
    run: u64,
    /// The turn it started in, i.e. the buffer generation its `started` frame
    /// belongs to.
    turn: u64,
    /// Its serialized `subagent_run_started` frame.
    frame: String,
}

impl SharedState {
    /// Whether the replay this attach is about to send already carries a
    /// context reading — in which case the snapshot would only duplicate it.
    fn buffer_carries_context(&self) -> bool {
        self.turn_active
            && self
                .buffer
                .iter()
                .any(|frame| frame.starts_with(r#"{"type":"context""#))
    }
}

impl TaskShared {
    pub(crate) fn new(
        id: String,
        cwd: PathBuf,
        model: String,
        mode: String,
        registry: Option<PathBuf>,
    ) -> Arc<Self> {
        let name = cwd
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "session".to_string());
        Arc::new(Self {
            id,
            cwd,
            started_unix: now_unix(),
            registry,
            state: Mutex::new(SharedState {
                task_state: TaskState::Idle,
                name,
                named: false,
                mode,
                activity: "idle".to_string(),
                pending_commands: Vec::new(),
                turn_active: false,
                buffer: VecDeque::new(),
                subscriber: None,
                subscriber_gen: 0,
                pending_plan: None,
                pending_interview: None,
                cancel: None,
                next_call_id: 0,
                open_calls: HashMap::new(),
                open_subagent_calls: HashMap::new(),
                live_runs: Vec::new(),
                turn_seq: 0,
                retries: 0,
                turn_error_seen: false,
                turn_cancel_requested: false,
                turn_failed: false,
                model,
                context_window: None,
                context_tokens: None,
            }),
        })
    }

    fn lock(&self) -> MutexGuard<'_, SharedState> {
        self.state.lock().expect("gui task state lock poisoned")
    }

    pub fn state(&self) -> TaskState {
        self.lock().task_state
    }

    pub fn model(&self) -> String {
        self.lock().model.clone()
    }

    fn set_model(&self, model: &str) {
        self.lock().model = model.to_string();
    }

    fn set_mode(&self, mode: Mode) {
        self.lock().mode = mode.to_string();
    }

    /// Name the task after its first message, as the TUI names a session after
    /// the prompt it launched with. Only the first one: later turns are the
    /// conversation, not its title.
    fn name_after_first_message(&self, text: &str) {
        let Some(line) = text
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(|line| line.chars().take(48).collect::<String>())
        else {
            return;
        };
        let mut state = self.lock();
        if !state.named {
            state.named = true;
            state.name = line;
        }
    }

    /// This task's heartbeat record. A GUI chat is a running Wizard session like
    /// any other: it belongs in `/dashboard` and in every other instance's task
    /// list, not just in the browser that opened it.
    fn record(&self) -> SessionRecord {
        let state = self.lock();
        SessionRecord {
            id: self.id.clone(),
            name: state.name.clone(),
            cwd: self.cwd.display().to_string(),
            model: state.model.clone(),
            mode: state.mode.clone(),
            state: registry_state(state.task_state),
            activity: state.activity.clone(),
            pid: std::process::id(),
            started_unix: self.started_unix,
            updated_unix: 0, // stamped by session_registry::write
        }
    }

    /// Publish (or refresh) this task's heartbeat.
    fn publish(&self) {
        let Some(dir) = &self.registry else { return };
        session_registry::write_to(dir, &self.record());
    }

    /// Take the commands the agent queued during the turn that just ended.
    fn take_pending_commands(&self) -> Vec<String> {
        std::mem::take(&mut self.lock().pending_commands)
    }

    fn set_context_window(&self, window: Option<u32>) {
        self.lock().context_window = window;
    }

    /// Emit a [`Frame::Context`] for `tokens` against the active model's window.
    fn push_context(&self, tokens: u64) {
        let mut state = self.lock();
        let window = state.context_window;
        state.context_tokens = Some(tokens);
        push_locked(&mut state, Frame::Context { tokens, window });
    }

    fn has_subscriber(&self) -> bool {
        self.lock().subscriber.is_some()
    }

    fn set_cancel(&self, cancel: CancelHandle) {
        self.lock().cancel = Some(cancel);
    }

    /// `cancel` client frame: interrupt the running turn cooperatively.
    pub fn cancel_turn(&self) {
        let cancel = {
            let mut state = self.lock();
            state.turn_cancel_requested = true;
            state.cancel.clone()
        };
        if let Some(cancel) = cancel {
            cancel.cancel();
        }
    }

    /// Claim the task's single turn slot. False when a turn is already
    /// queued or running.
    fn try_begin_turn(&self) -> bool {
        let mut state = self.lock();
        if state.turn_active {
            return false;
        }
        state.turn_active = true;
        true
    }

    /// Release a claimed turn slot without running it (the worker is gone).
    fn abandon_turn(&self) {
        self.lock().turn_active = false;
    }

    /// Emit one frame: append to the current turn's buffer and forward to
    /// the attached socket, if any.
    fn push(&self, frame: Frame) {
        push_locked(&mut self.lock(), frame);
    }

    /// Turn start: reset the replay buffer and per-turn counters, go
    /// working. Subagent state is *not* per-turn: a background run outlives
    /// the turn that spawned it, and keeps streaming into the panel.
    fn begin_turn(&self) {
        {
            let mut state = self.lock();
            state.buffer.clear();
            state.open_calls.clear();
            state.turn_seq += 1;
            state.retries = 0;
            state.turn_error_seen = false;
            state.turn_cancel_requested = false;
            state.turn_failed = false;
            state.task_state = TaskState::Working;
            state.activity = "working".to_string();
            let frame = Frame::State {
                state: state.task_state.as_str(),
            };
            push_locked(&mut state, frame);
        }
        self.publish();
    }

    /// Turn end: release the turn slot and go idle — or failed, when the
    /// agent could not be built or the turn's `done` reason was `error`.
    fn finish_turn(&self, failed: bool) {
        {
            let mut state = self.lock();
            state.turn_active = false;
            state.task_state = if failed || state.turn_failed {
                TaskState::Failed
            } else {
                TaskState::Idle
            };
            state.activity = "idle".to_string();
            // run_turn resolves its own gates before returning; drop any
            // leftovers defensively so a stale sender can never pin needs_input.
            state.pending_plan = None;
            state.pending_interview = None;
            let frame = Frame::State {
                state: state.task_state.as_str(),
            };
            push_locked(&mut state, frame);
        }
        self.publish();
    }

    /// Attach a socket: replay the current turn's buffered frames (when one
    /// is in flight), report the current state, and become the subscriber.
    /// A replay that already opens with the turn's own `state` frame carries
    /// every later transition too, so the snapshot would only duplicate it.
    /// Returns a generation token for [`TaskShared::detach`].
    ///
    /// Subagent runs still going are announced last, unless the replay just
    /// sent already carries their `started` frame (see [`LiveRun`]).
    pub fn attach(&self, tx: mpsc::UnboundedSender<String>) -> u64 {
        let mut state = self.lock();
        let mut replayed_state = false;
        let mut replayed_turn = None;
        if state.turn_active {
            replayed_state = state
                .buffer
                .front()
                .is_some_and(|frame| frame.starts_with(r#"{"type":"state""#));
            for frame in &state.buffer {
                let _ = tx.send(frame.clone());
            }
            replayed_turn = Some(state.turn_seq);
        }
        if !replayed_state {
            let current = serialize(&Frame::State {
                state: state.task_state.as_str(),
            });
            let _ = tx.send(current);
        }
        // The meter is a property of the conversation, not of the turn that
        // last moved it, so a client attaching between turns is told the
        // reading rather than showing nothing until the next one lands.
        if !state.buffer_carries_context()
            && let Some(tokens) = state.context_tokens
        {
            let window = state.context_window;
            let _ = tx.send(serialize(&Frame::Context { tokens, window }));
        }
        for live in &state.live_runs {
            if replayed_turn != Some(live.turn) {
                let _ = tx.send(live.frame.clone());
            }
        }
        state.subscriber = Some(tx);
        state.subscriber_gen += 1;
        state.subscriber_gen
    }

    /// Detach the socket identified by `generation` (a newer attach wins). A held
    /// plan/interview gate resolves the gateway way: approve the plan, skip
    /// the interview — a dropped reviewer must never hang the turn.
    pub fn detach(&self, generation: u64) {
        let resumed = {
            let mut state = self.lock();
            if state.subscriber_gen != generation {
                return;
            }
            state.subscriber = None;
            let mut resumed = false;
            if let Some(respond) = state.pending_plan.take() {
                let _ = respond.send(PlanVerdict::approve());
                resume_after_gate(&mut state, "plan auto-approved (client disconnected)");
                resumed = true;
            }
            if let Some(respond) = state.pending_interview.take() {
                let _ = respond.send(None);
                resume_after_gate(&mut state, "interview skipped (client disconnected)");
                resumed = true;
            }
            resumed
        };
        if resumed {
            self.publish();
        }
    }

    /// `plan_verdict` client frame. False when no plan is awaiting one.
    pub fn resolve_plan(&self, verdict: PlanVerdict) -> bool {
        {
            let mut state = self.lock();
            let Some(respond) = state.pending_plan.take() else {
                return false;
            };
            let _ = respond.send(verdict);
            resume_after_gate(&mut state, "");
        }
        self.publish();
        true
    }

    /// `interview_answers` client frame (`None` = declined). False when no
    /// interview is pending.
    pub fn resolve_interview(&self, answers: Option<Vec<String>>) -> bool {
        {
            let mut state = self.lock();
            let Some(respond) = state.pending_interview.take() else {
                return false;
            };
            let _ = respond.send(answers);
            resume_after_gate(&mut state, "");
        }
        self.publish();
        true
    }

    /// Map one [`AgentEvent`] to its protocol frame(s).
    fn handle_event(&self, event: AgentEvent) {
        match event {
            AgentEvent::TextDelta(text) => self.push(Frame::TextDelta { text }),
            AgentEvent::ThinkingDelta(text) => self.push(Frame::ThinkingDelta { text }),
            AgentEvent::ToolStarted { name, args } => {
                let mut state = self.lock();
                let call_id = state.next_call_id;
                state.next_call_id += 1;
                state
                    .open_calls
                    .entry(name.clone())
                    .or_default()
                    .push_back((call_id, args.clone()));
                // The newest in-flight tool call is what the dashboard row shows
                // while the task works, exactly as in the TUI.
                state.activity = name.clone();
                push_locked(
                    &mut state,
                    Frame::ToolStarted {
                        call_id,
                        name,
                        args,
                    },
                );
            }
            AgentEvent::ToolFinished { name, output } => {
                let mut state = self.lock();
                let open = state
                    .open_calls
                    .get_mut(&name)
                    .and_then(VecDeque::pop_front);
                let (call_id, args) = match open {
                    Some(pair) => pair,
                    None => {
                        let call_id = state.next_call_id;
                        state.next_call_id += 1;
                        (call_id, Value::Null)
                    }
                };
                let summary = summarize_tool(&name, &args, &output.content);
                push_locked(
                    &mut state,
                    Frame::ToolFinished {
                        call_id,
                        name,
                        ok: !output.is_error,
                        summary,
                    },
                );
            }
            AgentEvent::Images { source, images } => self.push(Frame::Images {
                source: source.as_str(),
                tool: source.tool().map(str::to_string),
                images,
            }),
            AgentEvent::SubagentRunImages {
                run,
                source,
                images,
            } => self.push(Frame::SubagentRunImages {
                run,
                source: source.as_str(),
                tool: source.tool().map(str::to_string),
                images,
            }),
            AgentEvent::TodoUpdated(items) => self.push(Frame::Todo {
                items: items.iter().map(TodoRow::from_item).collect(),
            }),
            AgentEvent::Usage {
                prompt_tokens,
                completion_tokens,
            } => {
                self.push(Frame::Usage {
                    prompt_tokens,
                    completion_tokens,
                });
                // The prompt this call actually ran on *is* the context that
                // will load into the next one — it is the number
                // `Agent::context_tokens` reports right after this event, and
                // the one the TUI's meter shows. A provider that reported only
                // completion tokens sends 0 here, which is not a context size.
                if prompt_tokens > 0 {
                    self.push_context(prompt_tokens);
                }
            }
            // What the next model call will load, re-estimated after the
            // history shrank (`/clear`, compaction) and the last reported
            // prompt size went stale.
            AgentEvent::ContextSize { tokens } => self.push_context(tokens),
            AgentEvent::PlanReady { plan, respond } => {
                {
                    let mut state = self.lock();
                    push_locked(&mut state, Frame::PlanReady { plan });
                    state.pending_plan = Some(respond);
                    state.task_state = TaskState::NeedsInput;
                    state.activity = "waiting for plan approval".to_string();
                    let frame = Frame::State {
                        state: state.task_state.as_str(),
                    };
                    push_locked(&mut state, frame);
                }
                self.publish();
            }
            AgentEvent::Interview { questions, respond } => {
                {
                    let mut state = self.lock();
                    push_locked(
                        &mut state,
                        Frame::Interview {
                            questions: questions.into_iter().map(|q| q.question).collect(),
                        },
                    );
                    state.pending_interview = Some(respond);
                    state.task_state = TaskState::NeedsInput;
                    state.activity = "waiting for interview answers".to_string();
                    let frame = Frame::State {
                        state: state.task_state.as_str(),
                    };
                    push_locked(&mut state, frame);
                }
                self.publish();
            }
            AgentEvent::OmakaseProceeding { plan } => self.push(Frame::Notice {
                text: format!("omakase — executing the agent's own plan:\n\n{plan}"),
            }),
            AgentEvent::TaskStarted { id, command } => self.push(Frame::TaskEvent {
                phase: "started",
                label: format!("#{id} {command}"),
            }),
            AgentEvent::TaskFinished {
                id,
                command,
                status,
            } => self.push(Frame::TaskEvent {
                phase: "finished",
                label: format!("#{id} {command} ({})", status.describe()),
            }),
            AgentEvent::SubagentStarted { name, task, .. } => self.push(Frame::Subagent {
                phase: "started",
                name,
                task,
            }),
            AgentEvent::SubagentFinished { name, task, .. } => self.push(Frame::Subagent {
                phase: "finished",
                name,
                task,
            }),
            AgentEvent::SubagentRunStarted {
                run,
                bg,
                name,
                task,
            } => {
                let mut state = self.lock();
                let text = serialize(&Frame::SubagentRunStarted {
                    run,
                    bg,
                    name,
                    task,
                });
                let turn = state.turn_seq;
                state.live_runs.push(LiveRun {
                    run,
                    turn,
                    frame: text.clone(),
                });
                push_text_locked(&mut state, text);
            }
            AgentEvent::SubagentRunText { run, text } => {
                self.push(Frame::SubagentRunText { run, text })
            }
            AgentEvent::SubagentRunToolStarted { run, name, args } => {
                let mut state = self.lock();
                let call_id = state.next_call_id;
                state.next_call_id += 1;
                state
                    .open_subagent_calls
                    .entry((run, name.clone()))
                    .or_default()
                    .push_back((call_id, args.clone()));
                push_locked(
                    &mut state,
                    Frame::SubagentRunToolStarted {
                        run,
                        call_id,
                        name,
                        args,
                    },
                );
            }
            AgentEvent::SubagentRunToolFinished { run, name, output } => {
                let mut state = self.lock();
                let open = state
                    .open_subagent_calls
                    .get_mut(&(run, name.clone()))
                    .and_then(VecDeque::pop_front);
                let (call_id, args) = match open {
                    Some(pair) => pair,
                    None => {
                        let call_id = state.next_call_id;
                        state.next_call_id += 1;
                        (call_id, Value::Null)
                    }
                };
                // The parent's own tool cards are summarized the same way, so
                // a subagent's read the same as the chat's.
                let summary = summarize_tool(&name, &args, &output.content);
                push_locked(
                    &mut state,
                    Frame::SubagentRunToolFinished {
                        run,
                        call_id,
                        name,
                        ok: !output.is_error,
                        summary,
                    },
                );
            }
            AgentEvent::SubagentRunStep { run, step } => {
                self.push(Frame::SubagentRunStep { run, step })
            }
            AgentEvent::SubagentRunDone {
                run,
                completed,
                output,
                steps_used,
                error,
            } => {
                let mut state = self.lock();
                state.live_runs.retain(|live| live.run != run);
                state.open_subagent_calls.retain(|(id, _), _| *id != run);
                push_locked(
                    &mut state,
                    Frame::SubagentRunDone {
                        run,
                        completed,
                        output,
                        steps_used,
                        error,
                    },
                );
            }
            AgentEvent::StreamRetrying => {
                let mut state = self.lock();
                state.retries += 1;
                let frame = Frame::Retrying {
                    attempt: state.retries + 1,
                };
                push_locked(&mut state, frame);
            }
            AgentEvent::HookFired {
                event,
                command,
                outcome,
            } => self.push(Frame::Notice {
                text: format!("hook {event}: {outcome} ({command})"),
            }),
            // The agent asked for one of its own slash commands. It cannot run
            // now — `run_turn` holds `&mut Agent`, and a request already in
            // flight cannot be reconfigured — so it queues, and the worker
            // applies it the moment the turn's borrow ends. The tool already
            // refused anything this surface does not run ([`AGENT_COMMANDS`]),
            // so what lands here is a command the executor has.
            AgentEvent::CommandRequested(line) => {
                let mut state = self.lock();
                push_locked(
                    &mut state,
                    Frame::Notice {
                        text: format!("agent requested {line} (runs after this turn)"),
                    },
                );
                state.pending_commands.push(line);
            }
            AgentEvent::Error(message) => {
                let mut state = self.lock();
                state.turn_error_seen = true;
                push_locked(&mut state, Frame::Error { message });
            }
            AgentEvent::Notice(text) => self.push(Frame::Notice { text }),
            AgentEvent::StepCompleted { .. } => {}
            AgentEvent::Done { reason } => {
                let mut state = self.lock();
                let reason =
                    done_reason(reason, state.turn_error_seen, state.turn_cancel_requested);
                state.turn_failed = reason == "error";
                push_locked(&mut state, Frame::Done { reason });
            }
        }
    }
}

fn serialize(frame: &Frame) -> String {
    serde_json::to_string(frame).expect("frames serialize")
}

fn push_locked(state: &mut SharedState, frame: Frame) {
    push_text_locked(state, serialize(&frame));
}

/// [`push_locked`] for a frame already serialized (one the task keeps a copy
/// of, so it is not serialized twice).
fn push_text_locked(state: &mut SharedState, text: String) {
    if state.buffer.len() >= MAX_BUFFERED_FRAMES {
        state.buffer.pop_front();
    }
    state.buffer.push_back(text.clone());
    if let Some(tx) = &state.subscriber
        && tx.send(text).is_err()
    {
        state.subscriber = None;
    }
}

/// A resolved gate: back to working, with an optional notice first.
fn resume_after_gate(state: &mut SharedState, notice: &str) {
    if !notice.is_empty() {
        push_locked(
            state,
            Frame::Notice {
                text: notice.to_string(),
            },
        );
    }
    state.task_state = TaskState::Working;
    state.activity = "working".to_string();
    let frame = Frame::State {
        state: state.task_state.as_str(),
    };
    push_locked(state, frame);
}

/// The registry state a task state heartbeats as.
///
/// A failed turn leaves the task live and waiting for the user, so it publishes
/// as idle — not as the registry's `Failed`, which marks a *finished* background
/// run and is retained for a day after its process is gone. Working, needs-input
/// and idle are the three states the TUI publishes, and a GUI chat is the same
/// kind of thing.
fn registry_state(state: TaskState) -> SessionState {
    match state {
        TaskState::Working => SessionState::Working,
        TaskState::NeedsInput => SessionState::NeedsInput,
        TaskState::Idle | TaskState::Failed => SessionState::Idle,
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The in-process registry of managed tasks, keyed by session id.
pub struct TaskManager {
    /// Read at agent-build time rather than cloned at startup, so a provider
    /// added on the Settings page is live for the very next turn.
    config: Arc<ConfigStore>,
    /// The GUI's MCP servers, connected once by [`crate::gui::run`] and handed
    /// to every task's agent build. One process-wide manager, as the TUI keeps:
    /// connecting per build would leave a GUI with four warm tasks running four
    /// copies of every configured server, each a real OS process.
    mcp: Arc<McpManager>,
    /// `~/.wizard/running/`, where every task this manager owns heartbeats.
    /// `None` only when the wizard directory cannot be resolved at all, in which
    /// case no session on the machine is registered anyway.
    registry: Option<PathBuf>,
    tasks: Mutex<HashMap<String, ManagedTask>>,
}

struct ManagedTask {
    shared: Arc<TaskShared>,
    turn_tx: mpsc::UnboundedSender<WorkerRequest>,
    last_used: Instant,
}

impl TaskManager {
    pub fn new(config: Arc<ConfigStore>, mcp: Arc<McpManager>) -> Self {
        Self::with_registry(config, mcp, session_registry::running_dir())
    }

    /// [`TaskManager::new`] heartbeating into an explicit directory (tests use a
    /// temp dir, so a test run never advertises itself as a live session).
    pub(crate) fn with_registry(
        config: Arc<ConfigStore>,
        mcp: Arc<McpManager>,
        registry: Option<PathBuf>,
    ) -> Self {
        Self {
            config,
            mcp,
            registry,
            tasks: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, ManagedTask>> {
        self.tasks.lock().expect("gui task map lock poisoned")
    }

    /// `POST /api/tasks`: create the session for `cwd`, spawn its worker,
    /// and queue the first turn when the request carries a prompt. Without
    /// one the chat opens empty and the first `user_message` starts it.
    /// Returns the new task id.
    pub fn create_task(
        &self,
        cwd: &Path,
        prompt: Option<String>,
        model: Option<String>,
    ) -> Result<String> {
        let sessions_dir = Config::sessions_dir()?;
        let session = Session::create_in(&sessions_dir, cwd)?;
        let id = session.id.clone();
        self.spawn(id.clone(), cwd.to_path_buf(), session);
        if let Some(text) = prompt {
            self.submit_turn(
                &id,
                TurnRequest {
                    text,
                    model,
                    ..TurnRequest::default()
                },
            )
            .map_err(|message| anyhow::anyhow!(message))?;
        }
        Ok(id)
    }

    /// The managed task for `id`, spawning a worker over the on-disk
    /// session when it is not live yet (WS attach on an old task).
    pub fn ensure(&self, id: &str) -> Result<Arc<TaskShared>> {
        if let Some(shared) = self.get(id) {
            return Ok(shared);
        }
        let sessions_dir = Config::sessions_dir()?;
        let session = Session::open_by_id(&sessions_dir, id)?
            .with_context(|| format!("no session '{id}'"))?;
        let cwd = session
            .cwd()
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("/"));
        Ok(self.spawn(id.to_string(), cwd, session))
    }

    /// The managed task for `id`, when live.
    pub fn get(&self, id: &str) -> Option<Arc<TaskShared>> {
        let mut tasks = self.lock();
        let task = tasks.get_mut(id)?;
        task.last_used = Instant::now();
        Some(task.shared.clone())
    }

    /// Queue one turn on task `id`. One in-flight turn per task: a second
    /// `user_message` while one runs is refused with the protocol's error
    /// text.
    pub fn submit_turn(&self, id: &str, request: TurnRequest) -> Result<(), String> {
        self.submit(id, WorkerRequest::Turn(request))
    }

    /// Queue one server-side slash command on task `id`. It takes the same
    /// slot a turn does — it mutates the same conversation — so a command
    /// arriving mid-turn is refused rather than queued behind it: the client
    /// asked for something to happen now, and "in four minutes" is not that.
    pub fn submit_command(&self, id: &str, request: CommandRequest) -> Result<(), String> {
        self.submit(id, WorkerRequest::Command(request))
    }

    fn submit(&self, id: &str, request: WorkerRequest) -> Result<(), String> {
        let mut tasks = self.lock();
        let Some(task) = tasks.get_mut(id) else {
            return Err(format!("task '{id}' is not live"));
        };
        if !task.shared.try_begin_turn() {
            return Err("turn in progress".to_string());
        }
        task.last_used = Instant::now();
        if task.turn_tx.send(request).is_err() {
            task.shared.abandon_turn();
            return Err("task worker exited".to_string());
        }
        Ok(())
    }

    /// Live task states, for merging into `/api/tasks`.
    pub fn states(&self) -> HashMap<String, TaskState> {
        self.lock()
            .iter()
            .map(|(id, task)| (id.clone(), task.shared.state()))
            .collect()
    }

    /// The model a live task runs on, if managed.
    pub fn model_of(&self, id: &str) -> Option<String> {
        self.lock().get(id).map(|task| task.shared.model())
    }

    /// Drop every task's heartbeat, so a stopped server leaves no session behind
    /// claiming to be running. Called on the server's graceful shutdown; a hard
    /// kill leaves the records to age out ([`session_registry::STALE_SECS`]).
    pub fn shutdown(&self) {
        let Some(dir) = &self.registry else { return };
        for id in self.lock().keys() {
            session_registry::remove_from(dir, id);
        }
    }

    fn spawn(&self, id: String, cwd: PathBuf, session: Session) -> Arc<TaskShared> {
        let mut tasks = self.lock();
        if let Some(existing) = tasks.get(&id) {
            // Raced with another request; keep the first worker.
            return existing.shared.clone();
        }
        evict_lru(&mut tasks, self.registry.as_deref());
        let config = self.config.current();
        let shared = TaskShared::new(
            id.clone(),
            cwd,
            config.active().model,
            config.mode.to_string(),
            self.registry.clone(),
        );
        // Live from here, not from the first turn: a chat opened and left empty
        // is a session somebody may come back to, and every other Wizard on the
        // machine should be able to see it.
        shared.publish();
        spawn_heartbeat(&shared);
        let (turn_tx, turn_rx) = mpsc::unbounded_channel();
        tokio::spawn(run_worker(
            Arc::clone(&self.config),
            Arc::clone(&self.mcp),
            Arc::clone(&shared),
            session,
            turn_rx,
        ));
        tasks.insert(
            id,
            ManagedTask {
                shared: Arc::clone(&shared),
                turn_tx,
                last_used: Instant::now(),
            },
        );
        shared
    }
}

/// Refresh a task's registry heartbeat for as long as it is live.
///
/// Its own task, rather than a tick on the worker's loop: the worker sits inside
/// `run_turn` for however long a turn takes, and a working session that stops
/// heartbeating is pruned as crashed — precisely when the dashboard most wants
/// to see it. Holds a `Weak`, so the beat stops when the task is evicted.
fn spawn_heartbeat(shared: &Arc<TaskShared>) {
    let shared = Arc::downgrade(shared);
    tokio::spawn(async move {
        let mut beat = tokio::time::interval(HEARTBEAT);
        beat.tick().await; // the first tick is immediate; `spawn` just published
        loop {
            beat.tick().await;
            let Some(shared) = shared.upgrade() else {
                return;
            };
            shared.publish();
        }
    });
}

/// Retire least-recently-used tasks that are safe to drop (no turn queued
/// or running, no socket attached) until the map is under the keep-warm
/// cap. Dropping the turn sender ends the worker, which fires the
/// session-end hooks and releases the agent.
///
/// The heartbeat goes here, under the map lock, and not in the worker's exit
/// path: a task re-spawned for the same session id publishes again immediately,
/// and the outgoing worker — which is still finishing its session-end hooks —
/// must not then delete the newcomer's record.
fn evict_lru(tasks: &mut HashMap<String, ManagedTask>, registry: Option<&Path>) {
    while tasks.len() >= MAX_WARM_TASKS {
        let candidate = tasks
            .iter()
            .filter(|(_, task)| {
                task.shared.state() != TaskState::Working
                    && task.shared.state() != TaskState::NeedsInput
                    && !task.shared.has_subscriber()
            })
            .min_by_key(|(_, task)| task.last_used)
            .map(|(id, _)| id.clone());
        match candidate {
            Some(id) => {
                tasks.remove(&id);
                if let Some(dir) = registry {
                    session_registry::remove_from(dir, &id);
                }
            }
            // Everything is busy or watched: let the map grow.
            None => break,
        }
    }
}

/// Per-task agent config: the user's own config, unchanged — same mode, same
/// step budget as the TUI, because the GUI is that agent on another surface and
/// not a reduced one. The only per-task edit is the model override: a configured
/// provider name switches the active provider, anything else is a model tag on
/// the active provider.
fn agent_config(base: &Config, model: Option<&str>) -> Config {
    let mut config = base.clone();
    if let Some(want) = model {
        if config.providers.iter().any(|p| p.name == want) {
            config.active_provider = Some(want.to_string());
        } else {
            let active = config.active().name;
            match config.providers.iter_mut().find(|p| p.name == active) {
                Some(provider) => provider.model = want.to_string(),
                // No configured providers: the synthesized local provider
                // reads the legacy `model` field.
                None => config.model = want.to_string(),
            }
        }
    }
    config
}

/// The dedicated worker for one task: owns the agent (built on the first
/// turn so server startup never needs a reachable provider) and runs queued
/// turns and commands one at a time, draining each turn's events into `shared`.
/// Ends when the manager drops the turn sender (eviction or shutdown).
async fn run_worker(
    store: Arc<ConfigStore>,
    mcp: Arc<McpManager>,
    shared: Arc<TaskShared>,
    session: Session,
    mut requests: mpsc::UnboundedReceiver<WorkerRequest>,
) {
    let mut agent: Option<Agent> = None;
    let mut task_config: Option<Config> = None;

    while let Some(request) = requests.recv().await {
        if let WorkerRequest::Turn(turn) = &request {
            shared.name_after_first_message(&turn.text);
        }
        shared.begin_turn();
        // Read the config per turn: a build that failed for want of a provider
        // must succeed on the next turn once Settings has configured one.
        let base_config = store.current();
        let model_override = match &request {
            WorkerRequest::Turn(turn) => turn.model.clone(),
            WorkerRequest::Command(_) => None,
        };

        // The agent is taken out for the turn and put back when it ends: an
        // agent that has not been built yet is built here (the session is
        // retained, so a failed build retries on the next turn), and a model
        // override on a *later* turn switches the live agent in place — the
        // first turn's override is already baked into its config.
        let mut agent_for_turn = match agent.take() {
            Some(mut live) => {
                if let Some(model) = model_override.as_deref()
                    && model != shared.model()
                {
                    let config = task_config.as_ref().unwrap_or(&base_config);
                    switch_model(&mut live, config, model, &shared).await;
                }
                live
            }
            None => {
                let config = agent_config(&base_config, model_override.as_deref());
                match build_headless_agent_for_session(
                    &config,
                    &shared.cwd,
                    session.clone(),
                    Some(mcp.as_ref()),
                )
                .await
                {
                    Ok(mut built) => {
                        if config.plan_first {
                            built.set_plan_mode(true);
                        }
                        if config.omakase {
                            built.set_omakase(true);
                        }
                        // The GUI drains the commands the agent queues — but only
                        // the ones its executor implements, so `run_command`
                        // refuses the rest to the model instead of accepting work
                        // that would never run.
                        built.set_command_dispatch(CommandDispatch::Only(AGENT_COMMANDS));
                        shared.set_cancel(built.cancel_handle());
                        shared.set_model(&config.active().model);
                        shared.set_mode(config.mode);
                        read_context_window(&config, &config.active().model, &shared).await;
                        fire_start_hooks(&mut built, &shared).await;
                        task_config = Some(config);
                        built
                    }
                    Err(err) => {
                        shared.push(Frame::Error {
                            message: format!("could not start the agent: {err:#}"),
                        });
                        shared.push(Frame::Done { reason: "error" });
                        shared.finish_turn(true);
                        continue;
                    }
                }
            }
        };

        match request {
            WorkerRequest::Turn(turn) => {
                run_turn(&mut agent_for_turn, turn, &shared).await;
                let config = task_config.as_ref().unwrap_or(&base_config);
                // The turn is over and its borrow of the agent with it, so the
                // commands it queued can finally be applied — through the one
                // executor a client-sent `command` frame goes through, emitting
                // the same frames.
                for line in shared.take_pending_commands() {
                    apply_command(
                        &mut agent_for_turn,
                        parse_command_line(&line),
                        config,
                        &shared,
                    )
                    .await;
                }
                shared.finish_turn(false);
            }
            WorkerRequest::Command(command) => {
                let config = task_config.as_ref().unwrap_or(&base_config);
                apply_command(&mut agent_for_turn, command, config, &shared).await;
                shared.finish_turn(false);
            }
        }
        agent = Some(agent_for_turn);
    }

    if let Some(agent) = &agent {
        agent.fire_session_end(None).await;
    }
}

/// Split a slash line the agent queued (`/model claude-sonnet-5`) into the name
/// and arguments the executor takes. The tool has already validated the line, so
/// this only has to cut it.
fn parse_command_line(line: &str) -> CommandRequest {
    let line = line.trim().trim_start_matches('/');
    let (name, args) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    CommandRequest {
        name: name.to_string(),
        args: args.trim().to_string(),
    }
}

/// Run one user turn on `agent`, streaming its events out as frames.
///
/// The text goes through [`crate::commands::preprocess`] first — the one
/// pipeline every surface shares — so a GUI message gets the same `@file`
/// references and the same custom `.wizard/commands/*.md` commands a TUI
/// message does. Non-image attachments join it as `@`-tokens, which is how
/// their contents reach the model: no second file-reading path.
async fn run_turn(agent: &mut Agent, request: TurnRequest, shared: &TaskShared) {
    let prompt = turn_prompt(request, &shared.cwd);

    let (events_tx, mut events_rx) = mpsc::channel::<AgentEvent>(256);
    // Drain events concurrently with the turn: the turn owns the sender
    // (dropped on completion, ending the collector), the collector owns
    // the receiver — disjoint borrows, same pattern as the gateway.
    let collector = async {
        while let Some(event) = events_rx.recv().await {
            shared.handle_event(event);
        }
    };
    let (result, ()) = tokio::join!(
        agent.run_turn_with_images(&prompt.text, prompt.images, events_tx),
        collector
    );
    if let Err(err) = result {
        // The turn already emitted `error` + `done` frames; the task itself
        // stays usable.
        tracing::warn!("gui task {}: turn failed: {err:#}", shared.id);
    }
    // Not every provider reports token counts, and `Usage` is the only thing
    // that otherwise moves the meter — so a turn against one that stays quiet
    // would leave it blank. `context_tokens` falls back to an estimate of the
    // history, which is what the TUI status bar shows in the same situation.
    shared.push_context(agent.context_tokens());
}

/// What the agent is actually asked: the message with its attached files as
/// `@/abs/path` tokens, run through the shared preprocessing pipeline (custom
/// `/command` expansion, then `@file` references), and the images to attach.
///
/// A path with whitespace in it would not survive `@`-tokenization, which is why
/// the upload route sanitizes names before writing.
fn turn_prompt(request: TurnRequest, cwd: &Path) -> crate::commands::Preprocessed {
    let mut input = request.text;
    for file in &request.files {
        input.push_str(&format!(" @{}", file.display()));
    }
    let custom = crate::commands::load(cwd);
    let mut prompt = crate::commands::preprocess(&input, &custom, cwd);
    // Uploaded images first: they are what the user attached to *this* message,
    // where an `@`-referenced one is context they pointed at.
    let mut images = request.images;
    images.append(&mut prompt.images);
    prompt.images = images;
    prompt
}

/// Apply one server-side slash command to the live agent (see
/// [`crate::gui::server::COMMANDS`]). Everything it has to say comes back as
/// the frames the protocol already has — `notice`, `context`, `error` — so a
/// command needs no reply frame of its own.
///
/// The one executor: a `command` frame from the client and a `/…` the agent
/// asked for through `run_command` both land here, and are answered the same.
async fn apply_command(
    agent: &mut Agent,
    request: CommandRequest,
    config: &Config,
    shared: &TaskShared,
) {
    let args = request.args.trim();
    match request.name.as_str() {
        "compact" => {
            let outcome = agent.compact_now().await;
            shared.push(Frame::Notice {
                text: outcome.describe(),
            });
            shared.push_context(agent.context_tokens());
        }
        "cost" => shared.push(Frame::Notice {
            text: cost_report(agent, config),
        }),
        "model" => {
            if args.is_empty() {
                shared.push(Frame::Error {
                    message: "usage: /model <name>".to_string(),
                });
                return;
            }
            switch_model(agent, config, args, shared).await;
        }
        "mode" => match args {
            "genie" => set_mode(agent, Mode::Genie, shared),
            "sovereign" => set_mode(agent, Mode::Sovereign, shared),
            // Plan is not a mode but a posture on top of one: the agent
            // investigates read-only and presents a plan through the same
            // `plan_ready` gate the socket already answers.
            "plan" => {
                agent.set_plan_mode(true);
                shared.push(Frame::Notice {
                    text: "plan mode on — the next turn plans before it acts".to_string(),
                });
            }
            other => shared.push(Frame::Error {
                message: format!("unknown mode '{other}' (sovereign|genie|plan)"),
            }),
        },
        "help" => shared.push(Frame::Notice {
            text: crate::gui::server::help_text(),
        }),
        other => shared.push(Frame::Error {
            message: format!("unknown command '/{other}'"),
        }),
    }
}

/// `/mode <sovereign|genie>`: switch the posture and leave plan mode, which is
/// a stance the old mode was holding, not a property of the new one.
fn set_mode(agent: &mut Agent, mode: Mode, shared: &TaskShared) {
    agent.set_mode(mode);
    agent.set_plan_mode(false);
    shared.set_mode(mode);
    shared.push(Frame::Notice {
        text: format!("mode: {mode}"),
    });
}

/// `/cost`: session token totals, with an estimate when the active provider
/// carries rates. The same report the TUI's `/cost` prints.
fn cost_report(agent: &Agent, config: &Config) -> String {
    let (prompt, completion) = agent.usage().session_totals();
    let mut text = format!("session usage: {prompt} prompt + {completion} completion tokens");
    let provider = config.active();
    match crate::usage::cost_usd(
        prompt,
        completion,
        provider.usd_per_mtok_in,
        provider.usd_per_mtok_out,
    ) {
        Some(cost) => text.push_str(&format!(" · est. ${cost:.4}")),
        None => text.push_str(&format!(
            "\nset usd_per_mtok_in / usd_per_mtok_out on provider '{}' in \
             ~/.wizard/config.toml for cost estimates",
            provider.name
        )),
    }
    text
}

/// Read the active model's context window and hold it for the task's `context`
/// frames. Once per model, not per event: for a local backend this is an HTTP
/// round trip (llama.cpp's `/props`), and a provider that cannot say degrades
/// to `None` — the meter then shows a count without a ceiling rather than a
/// ceiling somebody invented.
async fn read_context_window(config: &Config, model: &str, shared: &TaskShared) {
    let window = match config.active().build() {
        Ok(client) => client.context_window(model).await,
        Err(err) => {
            tracing::warn!("building a client to read the context window: {err:#}");
            None
        }
    };
    shared.set_context_window(window);
}

/// Fire the `session_start` hooks once per built agent, surfacing their
/// activity as notice frames (mirrors the gateway's console lines).
async fn fire_start_hooks(agent: &mut Agent, shared: &TaskShared) {
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);
    agent.fire_session_start(&tx).await;
    drop(tx);
    while let Some(event) = rx.recv().await {
        shared.handle_event(event);
    }
}

/// `/model`-style switch on a live agent: probe the new tag's tool-calling
/// support on a fresh client of the active provider (switching providers
/// mid-session is not supported) and swap the model in place, context
/// preserved.
async fn switch_model(agent: &mut Agent, config: &Config, model: &str, shared: &TaskShared) {
    let native = match config.active().build() {
        Ok(client) => crate::llm::provider::probe_native_tools(client.as_ref(), model).await,
        Err(err) => {
            tracing::warn!(
                "building a probe client: {err:#}; assuming \
                 native_tools={NATIVE_TOOLS_ON_PROBE_FAILURE}"
            );
            NATIVE_TOOLS_ON_PROBE_FAILURE
        }
    };
    agent.set_model(model.to_string(), native);
    shared.set_model(model);
    // The window is a property of the model, so it is re-read here and nowhere
    // else — a meter still scaled to the old model's window would misreport.
    read_context_window(config, model, shared).await;
    shared.push(Frame::Notice {
        text: format!("switched to model {model}"),
    });
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::config::StepBudget;
    use crate::tools::ToolOutput;

    /// An unmanaged task: it heartbeats nowhere, so a test run never advertises
    /// itself in the real registry.
    fn shared() -> Arc<TaskShared> {
        TaskShared::new(
            "2026-07-11T00-00-00".to_string(),
            PathBuf::from("/tmp/project"),
            "test-model".to_string(),
            "genie".to_string(),
            None,
        )
    }

    fn frames(rx: &mut mpsc::UnboundedReceiver<String>) -> Vec<Value> {
        let mut out = Vec::new();
        while let Ok(text) = rx.try_recv() {
            out.push(serde_json::from_str(&text).expect("valid frame JSON"));
        }
        out
    }

    #[test]
    fn tool_frames_pair_by_call_id_and_summarize_with_args() {
        let shared = shared();
        assert!(shared.try_begin_turn());
        shared.begin_turn();
        shared.handle_event(AgentEvent::ToolStarted {
            name: "read_file".to_string(),
            args: json!({ "path": "src/app.rs" }),
        });
        shared.handle_event(AgentEvent::ToolStarted {
            name: "read_file".to_string(),
            args: json!({ "path": "src/lib.rs" }),
        });
        shared.handle_event(AgentEvent::ToolFinished {
            name: "read_file".to_string(),
            output: ToolOutput::ok("line\n"),
        });

        let (tx, mut rx) = mpsc::unbounded_channel();
        shared.attach(tx);
        let frames = frames(&mut rx);
        let started: Vec<&Value> = frames
            .iter()
            .filter(|f| f["type"] == "tool_started")
            .collect();
        let finished: Vec<&Value> = frames
            .iter()
            .filter(|f| f["type"] == "tool_finished")
            .collect();
        assert_eq!(started.len(), 2);
        assert_eq!(finished.len(), 1);
        // FIFO per name: the finish pairs with the first started call.
        assert_eq!(finished[0]["call_id"], started[0]["call_id"]);
        assert_eq!(finished[0]["ok"], true);
        assert_eq!(finished[0]["summary"], "src/app.rs (1 line)");
    }

    #[test]
    fn subagent_run_frames_demux_by_run_and_pair_their_own_tool_calls() {
        let shared = shared();
        assert!(shared.try_begin_turn());
        shared.begin_turn();
        for (run, name) in [(1_u64, "researcher"), (2, "reviewer")] {
            shared.handle_event(AgentEvent::SubagentRunStarted {
                run,
                bg: None,
                name: name.to_string(),
                task: format!("{name}'s task"),
            });
        }
        // Two runs read a file at once: each finish must pair with its own
        // run's call, not the other's.
        shared.handle_event(AgentEvent::SubagentRunToolStarted {
            run: 1,
            name: "read_file".to_string(),
            args: json!({ "path": "src/app.rs" }),
        });
        shared.handle_event(AgentEvent::SubagentRunToolStarted {
            run: 2,
            name: "read_file".to_string(),
            args: json!({ "path": "src/lib.rs" }),
        });
        shared.handle_event(AgentEvent::SubagentRunToolFinished {
            run: 2,
            name: "read_file".to_string(),
            output: ToolOutput::ok("line\n"),
        });
        shared.handle_event(AgentEvent::SubagentRunStep { run: 1, step: 2 });

        let (tx, mut rx) = mpsc::unbounded_channel();
        shared.attach(tx);
        let frames = frames(&mut rx);
        let of_type =
            |kind: &str| -> Vec<&Value> { frames.iter().filter(|f| f["type"] == kind).collect() };
        let started = of_type("subagent_run_started");
        assert_eq!(started.len(), 2, "one row per run");
        assert_eq!(started[0]["run"], 1);
        assert_eq!(started[0]["name"], "researcher");
        assert_eq!(started[1]["task"], "reviewer's task");

        let tools = of_type("subagent_run_tool_started");
        let finished = of_type("subagent_run_tool_finished");
        assert_eq!(finished.len(), 1);
        assert_eq!(finished[0]["run"], 2);
        assert_eq!(
            finished[0]["call_id"], tools[1]["call_id"],
            "the finish pairs with run 2's call, not run 1's"
        );
        assert_eq!(
            finished[0]["summary"], "src/lib.rs (1 line)",
            "summarized with its own call's arguments"
        );
        assert_eq!(finished[0]["ok"], true);
        assert_eq!(of_type("subagent_run_step")[0]["step"], 2);
    }

    #[test]
    fn subagent_done_tells_a_failure_from_a_spent_step_budget() {
        let shared = shared();
        assert!(shared.try_begin_turn());
        shared.begin_turn();
        let (tx, mut rx) = mpsc::unbounded_channel();
        shared.attach(tx);

        shared.handle_event(AgentEvent::SubagentRunDone {
            run: 1,
            completed: false,
            output: "as far as I got".to_string(),
            steps_used: 12,
            error: None,
        });
        shared.handle_event(AgentEvent::SubagentRunDone {
            run: 2,
            completed: false,
            output: "subagent failed: boom".to_string(),
            steps_used: 3,
            error: Some("boom".to_string()),
        });

        let done: Vec<Value> = frames(&mut rx)
            .into_iter()
            .filter(|f| f["type"] == "subagent_run_done")
            .collect();
        assert_eq!(done[0]["completed"], false);
        assert_eq!(done[0]["steps_used"], 12);
        assert_eq!(done[0]["error"], Value::Null, "it ran out of steps");
        assert_eq!(done[0]["output"], "as far as I got");
        assert_eq!(done[1]["error"], "boom", "it died");
    }

    #[test]
    fn a_background_run_outlives_the_turn_that_spawned_it() {
        let shared = shared();
        assert!(shared.try_begin_turn());
        shared.begin_turn();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let generation = shared.attach(tx);
        shared.handle_event(AgentEvent::SubagentRunStarted {
            run: 7,
            bg: Some(1),
            name: "researcher".to_string(),
            task: "read the docs".to_string(),
        });
        shared.handle_event(AgentEvent::Done {
            reason: DoneReason::Completed,
        });
        shared.finish_turn(false);
        let _ = frames(&mut rx); // the turn's own frames

        // The parent turn is over; the detached run keeps streaming.
        shared.handle_event(AgentEvent::SubagentRunText {
            run: 7,
            text: "still going".to_string(),
        });
        let live = frames(&mut rx);
        assert_eq!(live.len(), 1, "the panel keeps updating: {live:?}");
        assert_eq!(live[0]["type"], "subagent_run_text");

        // A client that reconnects now — the run's `started` frame is in no
        // buffer it will ever see — still gets a row to stream it into.
        shared.detach(generation);
        let (tx, mut rx) = mpsc::unbounded_channel();
        shared.attach(tx);
        let replayed = frames(&mut rx);
        assert_eq!(replayed[0]["type"], "state", "idle: no turn to replay");
        assert_eq!(replayed[1]["type"], "subagent_run_started");
        assert_eq!(replayed[1]["run"], 7);
        assert_eq!(replayed[1]["bg"], 1);

        // Once it ends, it is announced no more.
        shared.handle_event(AgentEvent::SubagentRunDone {
            run: 7,
            completed: true,
            output: "done".to_string(),
            steps_used: 2,
            error: None,
        });
        let (tx, mut rx) = mpsc::unbounded_channel();
        shared.attach(tx);
        let replayed = frames(&mut rx);
        assert_eq!(replayed.len(), 1, "just the state snapshot: {replayed:?}");
    }

    #[test]
    fn attach_replays_the_current_turn_and_reports_state() {
        let shared = shared();
        assert!(shared.try_begin_turn());
        shared.begin_turn();
        shared.handle_event(AgentEvent::TextDelta("hello".to_string()));

        let (tx, mut rx) = mpsc::unbounded_channel();
        shared.attach(tx);
        let replayed = frames(&mut rx);
        assert_eq!(
            replayed.len(),
            2,
            "the buffer opens with the turn's state frame, so no snapshot is appended"
        );
        assert_eq!(replayed[0]["type"], "state");
        assert_eq!(replayed[0]["state"], "working");
        assert_eq!(replayed[1]["type"], "text_delta");
        assert_eq!(replayed[1]["text"], "hello");

        // Live frames flow after the replay.
        shared.handle_event(AgentEvent::Done {
            reason: DoneReason::Completed,
        });
        let live = frames(&mut rx);
        assert_eq!(live.last().unwrap()["type"], "done");
        assert_eq!(live.last().unwrap()["reason"], "completed");
    }

    #[test]
    fn attach_when_idle_sends_only_the_state_frame() {
        let shared = shared();
        assert!(shared.try_begin_turn());
        shared.begin_turn();
        shared.handle_event(AgentEvent::TextDelta("old turn".to_string()));
        shared.finish_turn(false);

        let (tx, mut rx) = mpsc::unbounded_channel();
        shared.attach(tx);
        let replayed = frames(&mut rx);
        assert_eq!(replayed.len(), 1, "finished turns are not replayed");
        assert_eq!(replayed[0]["type"], "state");
        assert_eq!(replayed[0]["state"], "idle");
    }

    /// The meter describes the conversation, not the turn that last moved it:
    /// a page reloaded between turns must not come back with a blank one.
    #[test]
    fn attaching_between_turns_is_told_the_context_reading() {
        let shared = shared();
        shared.set_context_window(Some(200_000));
        assert!(shared.try_begin_turn());
        shared.begin_turn();
        shared.push_context(1_234);
        shared.finish_turn(false);

        let (tx, mut rx) = mpsc::unbounded_channel();
        shared.attach(tx);
        let replayed = frames(&mut rx);
        let context = replayed
            .iter()
            .find(|frame| frame["type"] == "context")
            .expect("the reading survives the turn that produced it");
        assert_eq!(context["tokens"], 1_234);
        assert_eq!(context["window"], 200_000);
    }

    /// Mid-turn the replay already carries the reading, so the snapshot would
    /// only say it twice.
    #[test]
    fn attaching_mid_turn_is_not_told_the_reading_twice() {
        let shared = shared();
        assert!(shared.try_begin_turn());
        shared.begin_turn();
        shared.push_context(99);

        let (tx, mut rx) = mpsc::unbounded_channel();
        shared.attach(tx);
        let replayed = frames(&mut rx);
        assert_eq!(
            replayed
                .iter()
                .filter(|frame| frame["type"] == "context")
                .count(),
            1,
            "one reading, from the replay: {replayed:?}"
        );
    }

    #[test]
    fn one_turn_slot_per_task() {
        let shared = shared();
        assert!(shared.try_begin_turn());
        assert!(!shared.try_begin_turn(), "second claim is refused");
        shared.finish_turn(false);
        assert!(shared.try_begin_turn(), "slot frees on turn end");
    }

    #[test]
    fn plan_gate_resolves_via_client_frame() {
        let shared = shared();
        assert!(shared.try_begin_turn());
        shared.begin_turn();
        let (respond, verdict) = oneshot::channel();
        shared.handle_event(AgentEvent::PlanReady {
            plan: "1. do it".to_string(),
            respond,
        });
        assert_eq!(shared.state(), TaskState::NeedsInput);

        assert!(shared.resolve_plan(PlanVerdict::reject("smaller steps")));
        assert_eq!(shared.state(), TaskState::Working);
        let got = verdict.blocking_recv().expect("verdict delivered");
        assert!(!got.approved);
        assert_eq!(got.feedback, "smaller steps");
        assert!(
            !shared.resolve_plan(PlanVerdict::approve()),
            "gate is spent"
        );
    }

    #[test]
    fn socket_drop_auto_approves_plan_and_skips_interview() {
        let shared = shared();
        assert!(shared.try_begin_turn());
        shared.begin_turn();
        let (tx, rx) = mpsc::unbounded_channel();
        let generation = shared.attach(tx);
        drop(rx);

        let (plan_tx, plan_rx) = oneshot::channel();
        shared.handle_event(AgentEvent::PlanReady {
            plan: "plan".to_string(),
            respond: plan_tx,
        });
        let (interview_tx, interview_rx) = oneshot::channel();
        shared.handle_event(AgentEvent::Interview {
            questions: Vec::new(),
            respond: interview_tx,
        });

        shared.detach(generation);
        assert!(plan_rx.blocking_recv().expect("plan resolved").approved);
        assert_eq!(
            interview_rx.blocking_recv().expect("interview resolved"),
            None
        );
        assert_eq!(shared.state(), TaskState::Working);
    }

    #[test]
    fn stale_detach_does_not_clobber_a_newer_socket() {
        let shared = shared();
        let (old_tx, _old_rx) = mpsc::unbounded_channel();
        let old_gen = shared.attach(old_tx);
        let (new_tx, mut new_rx) = mpsc::unbounded_channel();
        shared.attach(new_tx);

        shared.detach(old_gen);
        shared.handle_event(AgentEvent::TextDelta("still streaming".to_string()));
        let got = frames(&mut new_rx);
        assert!(
            got.iter().any(|f| f["type"] == "text_delta"),
            "the newer socket keeps receiving: {got:?}"
        );
    }

    #[test]
    fn done_reasons_map_to_protocol_strings() {
        assert_eq!(
            done_reason(DoneReason::Completed, false, false),
            "completed"
        );
        assert_eq!(done_reason(DoneReason::Stopped, false, false), "cancelled");
        assert_eq!(done_reason(DoneReason::MaxSteps, false, false), "max_steps");
        assert_eq!(
            done_reason(DoneReason::CircuitBreaker, false, false),
            "error"
        );
        assert_eq!(done_reason(DoneReason::TimeLimit, false, false), "error");
        // A stop after an error is a failed turn — unless the client asked
        // for the stop, which stays a cancellation.
        assert_eq!(done_reason(DoneReason::Stopped, true, false), "error");
        assert_eq!(done_reason(DoneReason::Stopped, true, true), "cancelled");
        assert_eq!(done_reason(DoneReason::Stopped, false, true), "cancelled");
        // An error the turn recovered from does not taint its completion.
        assert_eq!(done_reason(DoneReason::Completed, true, false), "completed");
    }

    #[test]
    fn provider_error_ends_the_turn_failed_not_cancelled() {
        let shared = shared();
        assert!(shared.try_begin_turn());
        shared.begin_turn();
        // The agent's provider-failure shape: `Error` then `Done{Stopped}`.
        shared.handle_event(AgentEvent::Error("provider exploded".to_string()));
        shared.handle_event(AgentEvent::Done {
            reason: DoneReason::Stopped,
        });
        shared.finish_turn(false);
        assert_eq!(shared.state(), TaskState::Failed);

        let (tx, mut rx) = mpsc::unbounded_channel();
        shared.attach(tx);
        let replayed = frames(&mut rx);
        assert_eq!(replayed.len(), 1, "finished turns are not replayed");
        assert_eq!(replayed[0]["type"], "state");
        assert_eq!(replayed[0]["state"], "failed");
    }

    #[test]
    fn client_cancel_still_reads_cancelled() {
        let shared = shared();
        assert!(shared.try_begin_turn());
        shared.begin_turn();
        let (tx, mut rx) = mpsc::unbounded_channel();
        shared.attach(tx);

        shared.cancel_turn();
        // Even an error emitted while unwinding stays a cancellation.
        shared.handle_event(AgentEvent::Error("interrupted".to_string()));
        shared.handle_event(AgentEvent::Done {
            reason: DoneReason::Stopped,
        });
        shared.finish_turn(false);
        assert_eq!(shared.state(), TaskState::Idle);

        let got = frames(&mut rx);
        let done = got
            .iter()
            .find(|f| f["type"] == "done")
            .expect("done frame");
        assert_eq!(done["reason"], "cancelled");
        assert_eq!(got.last().unwrap()["type"], "state");
        assert_eq!(got.last().unwrap()["state"], "idle");

        // The flags are per-turn: an errored stop on the next turn is not
        // masked by the previous turn's cancel.
        assert!(shared.try_begin_turn());
        shared.begin_turn();
        shared.handle_event(AgentEvent::Error("provider exploded".to_string()));
        shared.handle_event(AgentEvent::Done {
            reason: DoneReason::Stopped,
        });
        let got = frames(&mut rx);
        let done = got
            .iter()
            .find(|f| f["type"] == "done")
            .expect("done frame");
        assert_eq!(done["reason"], "error");
    }

    #[test]
    fn agent_config_resolves_provider_names_and_model_tags() {
        let base = Config {
            providers: vec![
                crate::config::ProviderConfig {
                    name: "local".to_string(),
                    kind: crate::config::ProviderKind::LlamaCpp,
                    base_url: "http://127.0.0.1:11435".to_string(),
                    model: "qwen3.6:27b".to_string(),
                    api_key_env: None,
                    gguf_path: None,
                    usd_per_mtok_in: None,
                    usd_per_mtok_out: None,
                },
                crate::config::ProviderConfig {
                    name: "claude".to_string(),
                    kind: crate::config::ProviderKind::Anthropic,
                    base_url: "https://api.anthropic.com".to_string(),
                    model: "claude-fable-5".to_string(),
                    api_key_env: None,
                    gguf_path: None,
                    usd_per_mtok_in: None,
                    usd_per_mtok_out: None,
                },
            ],
            ..Default::default()
        };

        // No override: the config is the user's own, untouched.
        let config = agent_config(&base, None);
        assert_eq!(config.mode, base.mode);
        assert_eq!(config.active().name, "local");

        // A provider name switches the active provider.
        let config = agent_config(&base, Some("claude"));
        assert_eq!(config.active().name, "claude");
        assert_eq!(config.active().model, "claude-fable-5");

        // Anything else is a model tag on the active provider.
        let config = agent_config(&base, Some("qwen3.6:32b"));
        assert_eq!(config.active().name, "local");
        assert_eq!(config.active().model, "qwen3.6:32b");
    }

    #[test]
    fn gui_turns_run_on_the_users_own_mode_and_step_budget() {
        // The GUI is the same agent on another surface: it runs the config the
        // user configured, not a posture or a budget of its own.
        let base = Config {
            mode: Mode::Genie,
            max_steps: StepBudget::new(25),
            ..Config::default()
        };
        let config = agent_config(&base, None);
        assert_eq!(config.mode, Mode::Genie);
        assert_eq!(config.max_steps, StepBudget::new(25));

        let base = Config {
            mode: Mode::Sovereign,
            max_steps: StepBudget::UNLIMITED,
            ..Config::default()
        };
        let config = agent_config(&base, None);
        assert_eq!(config.mode, Mode::Sovereign);
        assert_eq!(
            config.max_steps,
            StepBudget::UNLIMITED,
            "the v1.2 default — no ceiling — reaches the GUI too"
        );
    }

    // --- the preprocessing seam ---

    #[test]
    fn a_gui_message_gets_the_file_refs_and_custom_commands_every_surface_has() {
        let cwd = tempfile::tempdir().unwrap();
        std::fs::write(cwd.path().join("notes.md"), "the note\n").unwrap();
        std::fs::create_dir_all(cwd.path().join(".wizard/commands")).unwrap();
        std::fs::write(
            cwd.path().join(".wizard/commands/review.md"),
            "Review $ARGUMENTS against @notes.md",
        )
        .unwrap();

        // An `@file` reference is read into the prompt.
        let prompt = turn_prompt(
            TurnRequest {
                text: "explain @notes.md".to_string(),
                ..TurnRequest::default()
            },
            cwd.path(),
        );
        assert!(prompt.text.contains("the note"), "got: {}", prompt.text);

        // A project custom command expands to its template — arguments and the
        // `@file` refs inside it included.
        let prompt = turn_prompt(
            TurnRequest {
                text: "/review src/app.rs".to_string(),
                ..TurnRequest::default()
            },
            cwd.path(),
        );
        assert!(
            prompt.text.starts_with("Review src/app.rs against"),
            "got: {}",
            prompt.text
        );
        assert!(prompt.text.contains("the note"), "got: {}", prompt.text);
    }

    #[test]
    fn attachments_reach_the_turn_as_images_and_file_refs() {
        let cwd = tempfile::tempdir().unwrap();
        let spec = cwd.path().join("spec.txt");
        std::fs::write(&spec, "the spec\n").unwrap();
        let shot = cwd.path().join("shot.png");
        std::fs::write(&shot, [0x89, b'P', b'N', b'G']).unwrap();

        let prompt = turn_prompt(
            TurnRequest {
                text: "what is wrong here?".to_string(),
                images: vec![shot.clone()],
                files: vec![spec],
                ..TurnRequest::default()
            },
            cwd.path(),
        );
        // The file is pulled in by the `@file` expansion, not by a second
        // file-reading path.
        assert!(prompt.text.starts_with("what is wrong here?"));
        assert!(prompt.text.contains("the spec"), "got: {}", prompt.text);
        // The image rides along as an attachment for the vision path.
        assert_eq!(prompt.images, vec![shot]);
    }

    // --- the context meter ---

    #[test]
    fn the_context_frame_reports_the_next_turn_not_the_lifetime_total() {
        let shared = shared();
        shared.set_context_window(Some(200_000));
        assert!(shared.try_begin_turn());
        shared.begin_turn();
        let (tx, mut rx) = mpsc::unbounded_channel();
        shared.attach(tx);

        // Two model calls in one turn. `usage` accumulates in the client's
        // lifetime total; `context` is what the *next* call will load, so the
        // second call's prompt replaces the first's rather than adding to it.
        shared.handle_event(AgentEvent::Usage {
            prompt_tokens: 1_000,
            completion_tokens: 40,
        });
        shared.handle_event(AgentEvent::Usage {
            prompt_tokens: 1_600,
            completion_tokens: 30,
        });
        let got = frames(&mut rx);
        let usage: Vec<&Value> = got.iter().filter(|f| f["type"] == "usage").collect();
        let context: Vec<&Value> = got.iter().filter(|f| f["type"] == "context").collect();
        assert_eq!(usage.len(), 2, "the lifetime frame is untouched");
        assert_eq!(usage[1]["prompt_tokens"], 1_600);
        assert_eq!(context.len(), 2);
        assert_eq!(context[1]["tokens"], 1_600, "not 2_600");
        assert_eq!(context[1]["window"], 200_000);

        // Compaction shrinks the history: the meter falls, and says so.
        shared.handle_event(AgentEvent::ContextSize { tokens: 300 });
        let got = frames(&mut rx);
        assert_eq!(got.len(), 1, "no usage frame — nothing was spent: {got:?}");
        assert_eq!(got[0]["type"], "context");
        assert_eq!(got[0]["tokens"], 300);
    }

    #[test]
    fn a_provider_with_no_known_window_omits_it() {
        let shared = shared();
        assert!(shared.try_begin_turn());
        shared.begin_turn();
        let (tx, mut rx) = mpsc::unbounded_channel();
        shared.attach(tx);

        shared.handle_event(AgentEvent::ContextSize { tokens: 512 });
        // A completion-only usage report is not a context size.
        shared.handle_event(AgentEvent::Usage {
            prompt_tokens: 0,
            completion_tokens: 12,
        });

        let context: Vec<Value> = frames(&mut rx)
            .into_iter()
            .filter(|f| f["type"] == "context")
            .collect();
        assert_eq!(context.len(), 1);
        assert_eq!(context[0]["tokens"], 512);
        assert_eq!(
            context[0].get("window"),
            None,
            "no window is no field, not a made-up default"
        );
    }

    // --- server-side slash commands ---

    /// A provider that answers nothing: the command tests drive the agent's own
    /// state (history, session, usage), which needs no model call.
    #[derive(Debug)]
    struct SilentProvider;

    #[async_trait::async_trait]
    impl crate::llm::provider::LlmProvider for SilentProvider {
        async fn health(&self) -> Result<()> {
            Ok(())
        }
        async fn supports_native_tools(&self, _model: &str) -> Result<bool> {
            Ok(true)
        }
        async fn list_models(&self) -> Result<Vec<String>> {
            Ok(Vec::new())
        }
        async fn chat_stream(
            &self,
            _request: crate::llm::ChatRequest,
        ) -> Result<crate::llm::ChatStream> {
            anyhow::bail!("no model behind this test")
        }
        fn label(&self) -> String {
            "silent".to_string()
        }
    }

    fn test_agent(cwd: &Path) -> Agent {
        let sessions = Config::sessions_dir().expect("sessions dir");
        let session = Session::create_in(&sessions, cwd).expect("session");
        let hooks = Arc::new(crate::hooks::HookEngine::new(
            Vec::new(),
            cwd.to_path_buf(),
            session.id.clone(),
        ));
        Agent::new(
            Arc::new(SilentProvider),
            crate::tools::registry::ToolRegistry::new(),
            Config::default(),
            Vec::new(),
            cwd.to_path_buf(),
            session,
            true,
            hooks,
        )
        .expect("agent")
    }

    async fn command(agent: &mut Agent, shared: &TaskShared, name: &str, args: &str) {
        apply_command(
            agent,
            CommandRequest {
                name: name.to_string(),
                args: args.to_string(),
            },
            &Config::default(),
            shared,
        )
        .await;
    }

    /// `Agent::clear` rotates the session file, and a GUI task is keyed by its
    /// session id: a server-side clear would leave `GET /api/tasks/{id}`
    /// replaying the session the agent had just stopped writing to, so the chat
    /// would come back from a reload missing every turn taken after the clear.
    /// The GUI therefore clears by starting a new chat, on the client.
    #[tokio::test]
    async fn clear_is_not_a_server_command_because_it_would_strand_the_session() {
        let cwd = tempfile::tempdir().unwrap();
        let mut agent = test_agent(cwd.path());
        let shared = shared();
        let (tx, mut rx) = mpsc::unbounded_channel();
        shared.attach(tx);
        let before = agent.session().id.clone();

        command(&mut agent, &shared, "clear", "").await;

        // Refused like any other unknown server command, and the session the
        // task is keyed by is left alone.
        assert_eq!(agent.session().id, before);
        let got = frames(&mut rx);
        assert!(
            got.iter().any(|f| f["type"] == "error"),
            "clear is not dispatched server-side: {got:?}"
        );

        assert_eq!(
            crate::gui::server::COMMANDS
                .iter()
                .find(|(name, ..)| *name == "clear")
                .map(|(_, _, executed_by, _)| *executed_by),
            Some("client"),
            "and the palette routes it to the client"
        );
    }

    #[tokio::test]
    async fn compact_summarizes_and_reports_the_new_context_size() {
        let cwd = tempfile::tempdir().unwrap();
        let mut agent = test_agent(cwd.path());
        let shared = shared();
        let (tx, mut rx) = mpsc::unbounded_channel();
        shared.attach(tx);

        // A history with nothing between the system prompt and the recent tail
        // has nothing to compact — and says so, rather than failing silently.
        command(&mut agent, &shared, "compact", "").await;
        let got = frames(&mut rx);
        let notice = got
            .iter()
            .find(|f| f["type"] == "notice")
            .expect("notice frame");
        assert!(
            notice["text"].as_str().unwrap().contains("compact"),
            "got: {notice}"
        );
        assert!(
            got.iter().any(|f| f["type"] == "context"),
            "the meter is refreshed either way: {got:?}"
        );
    }

    #[tokio::test]
    async fn cost_reports_the_lifetime_totals_and_unknown_commands_error() {
        let cwd = tempfile::tempdir().unwrap();
        let mut agent = test_agent(cwd.path());
        let shared = shared();
        let (tx, mut rx) = mpsc::unbounded_channel();
        shared.attach(tx);

        let _ = frames(&mut rx); // the attach snapshot

        agent.usage().record(Some(1_200), Some(300));
        command(&mut agent, &shared, "cost", "").await;
        let got = frames(&mut rx);
        assert_eq!(got[0]["type"], "notice");
        let text = got[0]["text"].as_str().unwrap();
        assert!(
            text.contains("1200 prompt + 300 completion"),
            "the session total, not the last call: {text}"
        );

        // `/mode` switches the posture the turn runs in.
        command(&mut agent, &shared, "mode", "sovereign").await;
        assert_eq!(agent.mode(), Mode::Sovereign);
        assert_eq!(frames(&mut rx)[0]["type"], "notice");

        command(&mut agent, &shared, "mode", "yolo").await;
        let got = frames(&mut rx);
        assert_eq!(got[0]["type"], "error");

        // A command the server does not have is an error, not a silent no-op.
        command(&mut agent, &shared, "publish", "").await;
        let got = frames(&mut rx);
        assert_eq!(got[0]["type"], "error");
        assert!(got[0]["message"].as_str().unwrap().contains("publish"));

        // `/model` without the model it needs.
        command(&mut agent, &shared, "model", "").await;
        let got = frames(&mut rx);
        assert_eq!(got[0]["type"], "error");
    }

    // --- the agent's own slash commands ---

    /// The set the tool gates on is the set the executor implements. Let them
    /// drift and the agent is either refused a command the GUI runs, or told
    /// "queued" for one it will answer with an error after the turn.
    #[test]
    fn the_commands_the_agent_may_queue_are_the_ones_the_server_executes() {
        let mut executed: Vec<&str> = crate::gui::server::COMMANDS
            .iter()
            .filter(|(_, _, executed_by, _)| *executed_by == "server")
            .map(|(name, ..)| *name)
            .collect();
        executed.sort_unstable();
        let mut offered = AGENT_COMMANDS.to_vec();
        offered.sort_unstable();
        assert_eq!(offered, executed);
    }

    #[test]
    fn a_queued_line_splits_into_the_name_and_arguments_the_executor_takes() {
        let request = parse_command_line("/model claude-sonnet-5");
        assert_eq!(request.name, "model");
        assert_eq!(request.args, "claude-sonnet-5");

        let request = parse_command_line("/compact");
        assert_eq!(request.name, "compact");
        assert_eq!(request.args, "");
    }

    /// The turn holds `&mut Agent`, so a command the agent calls for is queued
    /// and applied the moment the borrow ends — the TUI's `pending_agent_commands`
    /// for the same reason. What the model is told is *not* deferred: the tool
    /// refuses anything this surface cannot run before it ever gets here.
    #[tokio::test]
    async fn a_command_the_agent_calls_for_runs_when_the_turn_releases_the_agent() {
        let cwd = tempfile::tempdir().unwrap();
        let mut agent = test_agent(cwd.path());
        let shared = shared();
        assert!(shared.try_begin_turn());
        shared.begin_turn();
        let (tx, mut rx) = mpsc::unbounded_channel();
        shared.attach(tx);

        // Mid-turn: the tool's event lands, and says so — but nothing is applied
        // while the turn owns the agent.
        shared.handle_event(AgentEvent::CommandRequested("/mode sovereign".to_string()));
        shared.handle_event(AgentEvent::Done {
            reason: DoneReason::Completed,
        });
        assert_ne!(agent.mode(), Mode::Sovereign, "not while the turn runs");
        let queued = frames(&mut rx);
        assert!(
            queued.iter().any(|f| f["type"] == "notice"
                && f["text"]
                    .as_str()
                    .is_some_and(|text| text.contains("/mode sovereign"))),
            "the request is announced: {queued:?}"
        );

        // The worker drains the queue the moment the turn returns.
        for line in shared.take_pending_commands() {
            apply_command(
                &mut agent,
                parse_command_line(&line),
                &Config::default(),
                &shared,
            )
            .await;
        }
        shared.finish_turn(false);

        assert_eq!(agent.mode(), Mode::Sovereign, "it took effect");
        let applied = frames(&mut rx);
        assert!(
            applied
                .iter()
                .any(|f| f["type"] == "notice" && f["text"].as_str() == Some("mode: sovereign")),
            "and its effect is reported, exactly as a client-sent command's is: {applied:?}"
        );
        assert!(
            shared.take_pending_commands().is_empty(),
            "the queue is drained, not replayed on the next turn"
        );
    }

    // --- MCP servers ---

    /// One connected manager for the process, shared by every task. Connecting
    /// inside each agent build would give a GUI with four warm chats four copies
    /// of every configured MCP server — four filesystem servers, four browser
    /// servers, each a real OS process. The workers hold the manager the server
    /// connected, which is what the strong count says.
    #[tokio::test]
    async fn every_task_shares_the_one_connected_mcp_manager() {
        let cwd = tempfile::tempdir().unwrap();
        let sessions = cwd.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let mcp = Arc::new(McpManager::empty());
        let tasks = TaskManager::with_registry(
            Arc::new(ConfigStore::new(Config::default())),
            Arc::clone(&mcp),
            None,
        );
        assert_eq!(Arc::strong_count(&mcp), 2, "ours and the manager's");

        for _ in 0..2 {
            let session = Session::create_in(&sessions, cwd.path()).expect("session");
            tasks.spawn(session.id.clone(), cwd.path().to_path_buf(), session);
        }
        assert_eq!(
            Arc::strong_count(&mcp),
            4,
            "each worker holds the manager the server connected, not one it \
             connected for itself"
        );
    }

    // --- the session registry ---

    #[test]
    fn a_failed_turn_still_heartbeats_as_a_live_idle_session() {
        // The registry's `Failed` marks a *finished* background run and is kept
        // for a day; a GUI task whose turn errored is live, and the next message
        // retries it. The TUI publishes the same three states.
        assert_eq!(registry_state(TaskState::Working), SessionState::Working);
        assert_eq!(
            registry_state(TaskState::NeedsInput),
            SessionState::NeedsInput
        );
        assert_eq!(registry_state(TaskState::Idle), SessionState::Idle);
        assert_eq!(registry_state(TaskState::Failed), SessionState::Idle);
    }

    /// A GUI chat is a running Wizard session: `/dashboard` — in any other
    /// instance on the machine — must see it while it is alive, and must not see
    /// it once it is gone.
    #[tokio::test]
    async fn a_live_gui_task_is_in_the_session_registry_until_it_ends() {
        let cwd = tempfile::tempdir().unwrap();
        let sessions = cwd.path().join("sessions");
        let running = cwd.path().join("running");
        std::fs::create_dir_all(&sessions).unwrap();
        let tasks = TaskManager::with_registry(
            Arc::new(ConfigStore::new(Config::default())),
            Arc::new(McpManager::empty()),
            Some(running.clone()),
        );

        let session = Session::create_in(&sessions, cwd.path()).expect("session");
        let id = session.id.clone();
        let shared = tasks.spawn(id.clone(), cwd.path().to_path_buf(), session);

        // Live from the moment the task exists, idle and named for its workspace.
        let listed = session_registry::list_from(&running);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, id);
        assert_eq!(listed[0].state, SessionState::Idle);
        assert_eq!(listed[0].cwd, cwd.path().display().to_string());
        assert_eq!(listed[0].pid, std::process::id());

        // The turn's transitions are published as they happen, so a dashboard
        // does not wait a heartbeat to see the task go to work.
        shared.name_after_first_message("fix the parser\nand the tests");
        assert!(shared.try_begin_turn());
        shared.begin_turn();
        shared.handle_event(AgentEvent::ToolStarted {
            name: "read_file".to_string(),
            args: json!({ "path": "src/app.rs" }),
        });
        shared.publish();
        let listed = session_registry::list_from(&running);
        assert_eq!(listed[0].state, SessionState::Working);
        assert_eq!(listed[0].name, "fix the parser");
        assert_eq!(listed[0].activity, "read_file");

        // A gate is a session that needs its user — the state `/dashboard` sorts
        // to the top — and resolving it goes straight back to working.
        let (respond, _verdict) = oneshot::channel();
        shared.handle_event(AgentEvent::PlanReady {
            plan: "1. do it".to_string(),
            respond,
        });
        let listed = session_registry::list_from(&running);
        assert_eq!(listed[0].state, SessionState::NeedsInput);
        assert_eq!(listed[0].activity, "waiting for plan approval");
        assert!(shared.resolve_plan(PlanVerdict::approve()));
        assert_eq!(
            session_registry::list_from(&running)[0].state,
            SessionState::Working
        );

        shared.finish_turn(false);
        assert_eq!(
            session_registry::list_from(&running)[0].state,
            SessionState::Idle
        );

        // And it leaves the registry when the server stops, rather than sitting
        // there claiming to be running until it ages out.
        tasks.shutdown();
        assert!(session_registry::list_from(&running).is_empty());
    }
}
