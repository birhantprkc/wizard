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
use std::time::Instant;

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
use crate::tools::todo::{TodoItem, TodoStatus};

/// Keep at most this many agents warm; beyond it the least-recently-used
/// idle task is retired (its session persists, so it rebuilds on demand).
const MAX_WARM_TASKS: usize = 4;

/// Cap on buffered frames per turn, so a runaway turn with no socket
/// attached cannot grow without bound (the oldest frames are dropped).
const MAX_BUFFERED_FRAMES: usize = 10_000;

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
    Usage {
        prompt_tokens: u64,
        completion_tokens: u64,
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

/// One queued turn: the user text plus an optional model override.
#[derive(Debug)]
pub struct TurnRequest {
    pub text: String,
    pub model: Option<String>,
}

/// State shared between a task's worker, the WebSocket handler, and the
/// HTTP handlers. All mutation goes through the inner mutex; the async
/// sides never hold it across an await.
pub struct TaskShared {
    pub id: String,
    pub cwd: PathBuf,
    state: Mutex<SharedState>,
}

struct SharedState {
    task_state: TaskState,
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

impl TaskShared {
    fn new(id: String, cwd: PathBuf, model: String) -> Arc<Self> {
        Arc::new(Self {
            id,
            cwd,
            state: Mutex::new(SharedState {
                task_state: TaskState::Idle,
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
        let mut state = self.lock();
        state.buffer.clear();
        state.open_calls.clear();
        state.turn_seq += 1;
        state.retries = 0;
        state.turn_error_seen = false;
        state.turn_cancel_requested = false;
        state.turn_failed = false;
        state.task_state = TaskState::Working;
        let frame = Frame::State {
            state: state.task_state.as_str(),
        };
        push_locked(&mut state, frame);
    }

    /// Turn end: release the turn slot and go idle — or failed, when the
    /// agent could not be built or the turn's `done` reason was `error`.
    fn finish_turn(&self, failed: bool) {
        let mut state = self.lock();
        state.turn_active = false;
        state.task_state = if failed || state.turn_failed {
            TaskState::Failed
        } else {
            TaskState::Idle
        };
        // run_turn resolves its own gates before returning; drop any
        // leftovers defensively so a stale sender can never pin needs_input.
        state.pending_plan = None;
        state.pending_interview = None;
        let frame = Frame::State {
            state: state.task_state.as_str(),
        };
        push_locked(&mut state, frame);
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
        let mut state = self.lock();
        if state.subscriber_gen != generation {
            return;
        }
        state.subscriber = None;
        if let Some(respond) = state.pending_plan.take() {
            let _ = respond.send(PlanVerdict::approve());
            resume_after_gate(&mut state, "plan auto-approved (client disconnected)");
        }
        if let Some(respond) = state.pending_interview.take() {
            let _ = respond.send(None);
            resume_after_gate(&mut state, "interview skipped (client disconnected)");
        }
    }

    /// `plan_verdict` client frame. False when no plan is awaiting one.
    pub fn resolve_plan(&self, verdict: PlanVerdict) -> bool {
        let mut state = self.lock();
        let Some(respond) = state.pending_plan.take() else {
            return false;
        };
        let _ = respond.send(verdict);
        resume_after_gate(&mut state, "");
        true
    }

    /// `interview_answers` client frame (`None` = declined). False when no
    /// interview is pending.
    pub fn resolve_interview(&self, answers: Option<Vec<String>>) -> bool {
        let mut state = self.lock();
        let Some(respond) = state.pending_interview.take() else {
            return false;
        };
        let _ = respond.send(answers);
        resume_after_gate(&mut state, "");
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
            } => self.push(Frame::Usage {
                prompt_tokens,
                completion_tokens,
            }),
            // The TUI's status-bar estimate of what the next turn will cost.
            // The GUI reports real usage instead (`Frame::Usage`), so there is
            // nothing on the protocol for this and nothing to say.
            AgentEvent::ContextSize { .. } => {}
            AgentEvent::PlanReady { plan, respond } => {
                let mut state = self.lock();
                push_locked(&mut state, Frame::PlanReady { plan });
                state.pending_plan = Some(respond);
                state.task_state = TaskState::NeedsInput;
                let frame = Frame::State {
                    state: state.task_state.as_str(),
                };
                push_locked(&mut state, frame);
            }
            AgentEvent::Interview { questions, respond } => {
                let mut state = self.lock();
                push_locked(
                    &mut state,
                    Frame::Interview {
                        questions: questions.into_iter().map(|q| q.question).collect(),
                    },
                );
                state.pending_interview = Some(respond);
                state.task_state = TaskState::NeedsInput;
                let frame = Frame::State {
                    state: state.task_state.as_str(),
                };
                push_locked(&mut state, frame);
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
            AgentEvent::CommandRequested(command) => self.push(Frame::Notice {
                text: format!("agent requested command: {command}"),
            }),
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
    let frame = Frame::State {
        state: state.task_state.as_str(),
    };
    push_locked(state, frame);
}

/// The in-process registry of managed tasks, keyed by session id.
pub struct TaskManager {
    /// Read at agent-build time rather than cloned at startup, so a provider
    /// added on the Settings page is live for the very next turn.
    config: Arc<ConfigStore>,
    tasks: Mutex<HashMap<String, ManagedTask>>,
}

struct ManagedTask {
    shared: Arc<TaskShared>,
    turn_tx: mpsc::UnboundedSender<TurnRequest>,
    last_used: Instant,
}

impl TaskManager {
    pub fn new(config: Arc<ConfigStore>) -> Self {
        Self {
            config,
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
            self.submit_turn(&id, TurnRequest { text, model })
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

    fn spawn(&self, id: String, cwd: PathBuf, session: Session) -> Arc<TaskShared> {
        let mut tasks = self.lock();
        if let Some(existing) = tasks.get(&id) {
            // Raced with another request; keep the first worker.
            return existing.shared.clone();
        }
        evict_lru(&mut tasks);
        let shared = TaskShared::new(id.clone(), cwd, self.config.current().active().model);
        let (turn_tx, turn_rx) = mpsc::unbounded_channel();
        tokio::spawn(run_worker(
            Arc::clone(&self.config),
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

/// Retire least-recently-used tasks that are safe to drop (no turn queued
/// or running, no socket attached) until the map is under the keep-warm
/// cap. Dropping the turn sender ends the worker, which fires the
/// session-end hooks and releases the agent.
fn evict_lru(tasks: &mut HashMap<String, ManagedTask>) {
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
            }
            // Everything is busy or watched: let the map grow.
            None => break,
        }
    }
}

/// Per-task agent config: sovereign posture (there is no terminal behind the
/// GUI to prompt at) on the GUI's own step budget (`[gui] max_steps`, which
/// the Settings page edits), plus the optional model override — a configured
/// provider name switches the active provider, anything else is a model tag on
/// the active provider.
fn agent_config(base: &Config, model: Option<&str>) -> Config {
    let mut config = base.clone();
    config.mode = Mode::Sovereign;
    config.max_steps = config.gui.step_budget();
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
/// turns one at a time, draining each turn's events into `shared`. Ends
/// when the manager drops the turn sender (eviction or shutdown).
async fn run_worker(
    store: Arc<ConfigStore>,
    shared: Arc<TaskShared>,
    session: Session,
    mut requests: mpsc::UnboundedReceiver<TurnRequest>,
) {
    let mut agent: Option<Agent> = None;
    let mut task_config: Option<Config> = None;

    while let Some(request) = requests.recv().await {
        shared.begin_turn();
        // Read the config per turn: a build that failed for want of a provider
        // must succeed on the next turn once Settings has configured one.
        let base_config = store.current();

        // The agent is taken out for the turn and put back when it ends: an
        // agent that has not been built yet is built here (the session is
        // retained, so a failed build retries on the next turn), and a model
        // override on a *later* turn switches the live agent in place — the
        // first turn's override is already baked into its config.
        let mut agent_for_turn = match agent.take() {
            Some(mut live) => {
                if let Some(model) = request.model.as_deref()
                    && model != shared.model()
                {
                    let config = task_config.as_ref().unwrap_or(&base_config);
                    switch_model(&mut live, config, model, &shared).await;
                }
                live
            }
            None => {
                let config = agent_config(&base_config, request.model.as_deref());
                match build_headless_agent_for_session(&config, &shared.cwd, session.clone()).await
                {
                    Ok(mut built) => {
                        if config.plan_first {
                            built.set_plan_mode(true);
                        }
                        if config.omakase {
                            built.set_omakase(true);
                        }
                        shared.set_cancel(built.cancel_handle());
                        shared.set_model(&config.active().model);
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

        let (events_tx, mut events_rx) = mpsc::channel::<AgentEvent>(256);
        // Drain events concurrently with the turn: the turn owns the sender
        // (dropped on completion, ending the collector), the collector owns
        // the receiver — disjoint borrows, same pattern as the gateway.
        let collector = async {
            while let Some(event) = events_rx.recv().await {
                shared.handle_event(event);
            }
        };
        let (result, ()) =
            tokio::join!(agent_for_turn.run_turn(&request.text, events_tx), collector);
        if let Err(err) = result {
            // The turn already emitted `error` + `done` frames; the task
            // itself stays usable.
            tracing::warn!("gui task {}: turn failed: {err:#}", shared.id);
        }
        agent = Some(agent_for_turn);
        shared.finish_turn(false);
    }

    if let Some(agent) = &agent {
        agent.fire_session_end(None).await;
    }
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
        Ok(client) => match client.supports_native_tools(model).await {
            Ok(supported) => supported,
            Err(err) => {
                tracing::warn!("probing tool support for '{model}': {err:#}; assuming native");
                true
            }
        },
        Err(err) => {
            tracing::warn!("building a probe client: {err:#}; assuming native tools");
            true
        }
    };
    agent.set_model(model.to_string(), native);
    shared.set_model(model);
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

    fn shared() -> Arc<TaskShared> {
        TaskShared::new(
            "2026-07-11T00-00-00".to_string(),
            PathBuf::from("/tmp/project"),
            "test-model".to_string(),
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

        // Sovereign posture always applies.
        let config = agent_config(&base, None);
        assert_eq!(config.mode, Mode::Sovereign);

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
    fn gui_turns_run_on_the_step_limit_settings_shows() {
        // The Settings page edits `[gui] max_steps`; if a turn ran on anything
        // else, that control would be decoration.
        let mut base = Config::default();
        base.gui.max_steps = 7;
        // The TUI's budget, and none of the GUI's business — not even the
        // unlimited default a TUI turn now runs on.
        base.max_steps = StepBudget::new(25);
        assert_eq!(agent_config(&base, None).max_steps, StepBudget::new(7));

        base.gui.max_steps = 0;
        assert_eq!(
            agent_config(&base, None).max_steps,
            StepBudget::UNLIMITED,
            "0 lifts the GUI's ceiling, as it does the TUI's"
        );
    }
}
