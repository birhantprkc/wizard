//! Headless output sinks: how a sovereign (`-p`) run reports its events.
//!
//! The agent loop emits [`AgentEvent`]s; `run_headless` forwards them to one
//! [`EventSink`] selected by `--output-format`:
//!
//! - `text` (default) — the human-readable stream: deltas as they arrive,
//!   tool one-liners, a spinner on terminals.
//! - `json` — silent until the run ends, then one JSON object summarizing
//!   the result, steps, usage, and tool calls.
//! - `stream-json` — one JSON object per line as events arrive, terminated
//!   by a `{"type":"done", ...}` line.
//!
//! The run outcome also becomes the process exit code (see [`exit_code`]),
//! so scripts can branch on *why* a run ended without parsing output.

use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;

use serde_json::json;

use crate::agent::{AgentEvent, DoneReason, ImageSource, PlanVerdict};
use crate::images::ImageRef;
use crate::progress::TurnSpinner;

/// `--output-format` values for headless (`-p`) runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum OutputFormat {
    /// Human-readable streaming output (the default).
    #[default]
    Text,
    /// One final JSON object summarizing the whole run.
    Json,
    /// One JSON object per line as events arrive (JSONL).
    StreamJson,
}

/// One line per image announced by [`AgentEvent::Images`]: what produced it and
/// where it was written. A headless run has no canvas, so the path is the
/// deliverable — it is printed, not the pixels.
fn image_lines(source: &ImageSource, images: &[ImageRef]) -> Vec<String> {
    images
        .iter()
        .map(|image| {
            let from = match source.tool() {
                Some(tool) => format!("image from `{tool}`"),
                None => "image".to_string(),
            };
            format!(
                "⏺ {from}: {} ({}, {} KB)",
                image.path.display(),
                image.mime,
                image.bytes.div_ceil(1024)
            )
        })
        .collect()
}

/// The `stream-json` frame for announced images. `run` is set when they came
/// from inside a subagent run.
fn image_json(run: Option<u64>, source: &ImageSource, images: &[ImageRef]) -> serde_json::Value {
    json!({
        "type": "images",
        "run": run,
        "source": source.as_str(),
        "tool": source.tool(),
        "images": images,
    })
}

/// Process exit code for a finished headless run. Hard errors exit 1 from
/// `main`; a user-requested stop is a success.
pub fn exit_code(reason: DoneReason) -> i32 {
    match reason {
        DoneReason::Completed | DoneReason::Stopped => 0,
        DoneReason::MaxSteps => 2,
        DoneReason::CircuitBreaker => 3,
        DoneReason::TimeLimit => 4,
    }
}

/// Stable snake_case name for a [`DoneReason`] in JSON output.
pub fn reason_str(reason: DoneReason) -> &'static str {
    match reason {
        DoneReason::Completed => "completed",
        DoneReason::MaxSteps => "max_steps",
        DoneReason::TimeLimit => "time_limit",
        DoneReason::Stopped => "stopped",
        DoneReason::CircuitBreaker => "circuit_breaker",
    }
}

/// Consumes the agent-event stream of one headless run.
///
/// `event` is called for every [`AgentEvent`] in order; `finish` exactly once
/// after the run loop ends (and only for runs that did not hard-error), with
/// the final outcome. Implementations own their output destination and must
/// leave stdout flushed when `finish` returns.
pub trait EventSink: Send {
    fn event(&mut self, event: AgentEvent);
    fn finish(&mut self, reason: DoneReason);
}

// ---------------------------------------------------------------------------
// text
// ---------------------------------------------------------------------------

/// The default human-readable printer (previously inlined in
/// `run_headless`): streams deltas, prints tool one-liners, auto-approves
/// plans, and shares the run's busy spinner so lines never tear it.
pub struct TextSink {
    spinner: Arc<TurnSpinner>,
    prompt_tokens: u64,
    completion_tokens: u64,
    /// Run id -> subagent name, so a subagent's tool calls can still be
    /// printed as `<name> ▸ <tool>`. The TUI routes these into the run's pane
    /// instead; headless has no rail, so it keeps the inline log.
    subagents: HashMap<u64, String>,
}

impl TextSink {
    pub fn new(spinner: Arc<TurnSpinner>) -> Self {
        Self {
            spinner,
            prompt_tokens: 0,
            completion_tokens: 0,
            subagents: HashMap::new(),
        }
    }

    /// Name of the subagent driving `run`. Falls back to `"subagent"` if the
    /// start event was missed (it never should be — it is emitted before the
    /// run can produce anything).
    fn subagent_name(&self, run: u64) -> &str {
        self.subagents
            .get(&run)
            .map(String::as_str)
            .unwrap_or("subagent")
    }
}

impl EventSink for TextSink {
    fn event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::TextDelta(delta) => {
                self.spinner.hide();
                print!("{delta}");
                let _ = std::io::stdout().flush();
            }
            AgentEvent::ThinkingDelta(delta) => {
                // ANSI faint so reasoning reads as background noise.
                self.spinner.hide();
                print!("\x1b[2m{delta}\x1b[0m");
                let _ = std::io::stdout().flush();
            }
            AgentEvent::ToolStarted { name, args } => {
                self.spinner.println(&format!("\n→ {name} {args}"));
                // The tool may run for a while: keep the verb spinning.
                self.spinner.show();
            }
            AgentEvent::ToolFinished { name, output } => {
                let status = if output.is_error { "error" } else { "ok" };
                self.spinner.println(&format!("← {name} [{status}]"));
                // Back to the model: it is thinking about the result.
                self.spinner.show();
            }
            AgentEvent::Images { source, images } => {
                for line in image_lines(&source, &images) {
                    self.spinner.println(&line);
                }
            }
            AgentEvent::SubagentRunStarted { run, name, .. } => {
                self.subagents.insert(run, name);
            }
            AgentEvent::SubagentRunImages {
                run,
                source,
                images,
            } => {
                let who = self.subagent_name(run).to_string();
                for line in image_lines(&source, &images) {
                    self.spinner.println(&format!("{who} ▸ {line}"));
                }
            }
            AgentEvent::SubagentRunToolStarted { run, name, args } => {
                let who = self.subagent_name(run);
                self.spinner.println(&format!("\n→ {who} ▸ {name} {args}"));
                self.spinner.show();
            }
            AgentEvent::SubagentRunToolFinished { run, name, output } => {
                let who = self.subagent_name(run);
                let status = if output.is_error { "error" } else { "ok" };
                self.spinner
                    .println(&format!("← {who} ▸ {name} [{status}]"));
                self.spinner.show();
            }
            AgentEvent::SubagentRunDone { run, .. } => {
                self.subagents.remove(&run);
            }
            // The subagent's intermediate messages and step ticks stay quiet:
            // its report is printed when it lands in the parent's context.
            AgentEvent::SubagentRunText { .. } | AgentEvent::SubagentRunStep { .. } => {}
            AgentEvent::StepCompleted { step } => {
                tracing::debug!("step {step} completed");
            }
            AgentEvent::StreamRetrying => {
                // Text already printed can't be unprinted; mark the cut so
                // the re-generated response below isn't read as a duplicate.
                self.spinner.hide();
                println!("\x1b[2m\n… stream interrupted — the response restarts below …\x1b[0m");
            }
            AgentEvent::Error(message) => {
                self.spinner.hide();
                eprintln!("\nwizard error: {message}");
            }
            AgentEvent::Notice(message) => {
                self.spinner.println(&format!("~ {message}"));
            }
            AgentEvent::HookFired {
                event,
                command,
                outcome,
            } => {
                self.spinner
                    .println(&format!("~ hook {event}: {outcome} ({command})"));
            }
            AgentEvent::PlanReady { plan, respond } => {
                // Headless: print the plan and approve it, so the turn moves
                // from planning to execution on its own.
                self.spinner
                    .println(&format!("\n=== plan ===\n{plan}\n=== plan approved ==="));
                let _ = respond.send(PlanVerdict::approve());
                self.spinner.show();
            }
            AgentEvent::Interview { respond, .. } => {
                // Headless: no interactive user — decline so the model
                // proceeds with its best judgment.
                let _ = respond.send(None);
            }
            AgentEvent::OmakaseProceeding { plan } => {
                self.spinner.println(&format!(
                    "\n=== plan (omakase — chef's choice) ===\n{plan}\n=== proceeding ==="
                ));
                self.spinner.show();
            }
            AgentEvent::Usage {
                prompt_tokens,
                completion_tokens,
            } => {
                self.prompt_tokens += prompt_tokens;
                self.completion_tokens += completion_tokens;
            }
            AgentEvent::TodoUpdated(items) => {
                self.spinner
                    .println(&crate::tools::todo::summary_line(&items));
            }
            AgentEvent::TaskStarted { .. } => {}
            AgentEvent::TaskFinished {
                id,
                command,
                status,
            } => {
                self.spinner.println(&format!(
                    "⏺ background task #{id} finished ({}): {command}",
                    status.describe()
                ));
            }
            AgentEvent::SubagentStarted { .. } => {}
            AgentEvent::SubagentFinished {
                id,
                name,
                task,
                completed,
                ..
            } => {
                self.spinner.println(&format!(
                    "⏺ background subagent #{id} '{name}' {}: {task}",
                    if completed {
                        "finished"
                    } else {
                        "hit its step budget"
                    }
                ));
            }
            AgentEvent::CommandRequested(line) => {
                // No interactive menu to drive in a headless run: report the
                // request but make clear it isn't applied.
                self.spinner.println(&format!(
                    "~ agent requested {line} (slash commands apply only in the interactive TUI)"
                ));
            }
            AgentEvent::Done { reason } => {
                self.spinner.hide();
                println!("\n[turn done: {reason:?}]");
            }
        }
    }

    fn finish(&mut self, reason: DoneReason) {
        if self.prompt_tokens > 0 || self.completion_tokens > 0 {
            println!(
                "[run finished: {reason:?} — {} prompt + {} completion tokens]",
                self.prompt_tokens, self.completion_tokens
            );
        } else {
            println!("[run finished: {reason:?}]");
        }
        let _ = std::io::stdout().flush();
    }
}

// ---------------------------------------------------------------------------
// json
// ---------------------------------------------------------------------------

/// Per-tool aggregation for the final `json` summary.
#[derive(Debug)]
struct ToolCallsEntry {
    name: String,
    calls: u64,
    errors: u64,
}

/// Buffers the whole run and emits one final JSON object:
/// `{result, reason, turns, steps, usage, tool_calls, errors}`.
pub struct JsonSink<W: Write + Send> {
    out: W,
    result: String,
    /// Length of `result` at the last completed step — the truncation point
    /// when a mid-stream retry discards the current attempt's partial text.
    committed: usize,
    turns: u64,
    steps: u64,
    prompt_tokens: u64,
    completion_tokens: u64,
    tool_calls: Vec<ToolCallsEntry>,
    errors: Vec<String>,
    /// Images the run produced, in order — where they were written, so a script
    /// consuming the summary can pick them up.
    images: Vec<ImageRef>,
    /// Run id -> subagent name, so a subagent's tool calls aggregate under
    /// `<name> ▸ <tool>` in the summary, as they did when they were emitted on
    /// the parent's tool events.
    subagents: HashMap<u64, String>,
}

impl JsonSink<std::io::Stdout> {
    pub fn stdout() -> Self {
        Self::new(std::io::stdout())
    }
}

impl<W: Write + Send> JsonSink<W> {
    pub fn new(out: W) -> Self {
        Self {
            out,
            result: String::new(),
            committed: 0,
            turns: 0,
            steps: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
            tool_calls: Vec::new(),
            errors: Vec::new(),
            images: Vec::new(),
            subagents: HashMap::new(),
        }
    }

    /// `<subagent> ▸ <tool>` for a subagent's tool call, matching the label
    /// these calls carried when they rode the parent's tool events.
    fn subagent_label(&self, run: u64, tool: &str) -> String {
        let who = self
            .subagents
            .get(&run)
            .map(String::as_str)
            .unwrap_or("subagent");
        format!("{who} ▸ {tool}")
    }
}

impl<W: Write + Send> EventSink for JsonSink<W> {
    fn event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::TextDelta(delta) => self.result.push_str(&delta),
            AgentEvent::ToolStarted { name, .. } => {
                match self.tool_calls.iter_mut().find(|entry| entry.name == name) {
                    Some(entry) => entry.calls += 1,
                    None => self.tool_calls.push(ToolCallsEntry {
                        name,
                        calls: 1,
                        errors: 0,
                    }),
                }
            }
            AgentEvent::ToolFinished { name, output } => {
                if output.is_error
                    && let Some(entry) = self.tool_calls.iter_mut().find(|entry| entry.name == name)
                {
                    entry.errors += 1;
                }
            }
            AgentEvent::SubagentRunStarted { run, name, .. } => {
                self.subagents.insert(run, name);
            }
            AgentEvent::SubagentRunToolStarted { run, name, .. } => {
                let label = self.subagent_label(run, &name);
                match self.tool_calls.iter_mut().find(|entry| entry.name == label) {
                    Some(entry) => entry.calls += 1,
                    None => self.tool_calls.push(ToolCallsEntry {
                        name: label,
                        calls: 1,
                        errors: 0,
                    }),
                }
            }
            AgentEvent::SubagentRunToolFinished { run, name, output } => {
                if output.is_error {
                    let label = self.subagent_label(run, &name);
                    if let Some(entry) =
                        self.tool_calls.iter_mut().find(|entry| entry.name == label)
                    {
                        entry.errors += 1;
                    }
                }
            }
            AgentEvent::SubagentRunDone { run, .. } => {
                self.subagents.remove(&run);
            }
            AgentEvent::SubagentRunText { .. } | AgentEvent::SubagentRunStep { .. } => {}
            AgentEvent::StepCompleted { .. } => {
                self.steps += 1;
                self.committed = self.result.len();
            }
            AgentEvent::Error(message) => self.errors.push(message),
            AgentEvent::StreamRetrying => {
                // The attempt's partial text never entered history; the retry
                // re-streams it, so keeping it would double the text.
                self.result.truncate(self.committed);
            }
            AgentEvent::PlanReady { respond, .. } => {
                // No human in the loop: approve so the turn executes.
                let _ = respond.send(PlanVerdict::approve());
            }
            AgentEvent::Interview { respond, .. } => {
                // No interactive user: decline so the model uses its judgment.
                let _ = respond.send(None);
            }
            AgentEvent::Usage {
                prompt_tokens,
                completion_tokens,
            } => {
                self.prompt_tokens += prompt_tokens;
                self.completion_tokens += completion_tokens;
            }
            AgentEvent::Done { .. } => self.turns += 1,
            AgentEvent::Images { images, .. } | AgentEvent::SubagentRunImages { images, .. } => {
                self.images.extend(images);
            }
            AgentEvent::ThinkingDelta(_)
            | AgentEvent::Notice(_)
            | AgentEvent::HookFired { .. }
            | AgentEvent::OmakaseProceeding { .. }
            | AgentEvent::TodoUpdated(_)
            | AgentEvent::TaskStarted { .. }
            | AgentEvent::TaskFinished { .. }
            | AgentEvent::SubagentStarted { .. }
            | AgentEvent::SubagentFinished { .. }
            | AgentEvent::CommandRequested(_) => {}
        }
    }

    fn finish(&mut self, reason: DoneReason) {
        let summary = json!({
            "result": self.result,
            "reason": reason_str(reason),
            "turns": self.turns,
            "steps": self.steps,
            "usage": {
                "prompt_tokens": self.prompt_tokens,
                "completion_tokens": self.completion_tokens,
            },
            "tool_calls": self.tool_calls.iter().map(|entry| json!({
                "name": entry.name,
                "calls": entry.calls,
                "errors": entry.errors,
            })).collect::<Vec<_>>(),
            "errors": self.errors,
            "images": self.images,
        });
        let _ = writeln!(self.out, "{summary}");
        let _ = self.out.flush();
    }
}

// ---------------------------------------------------------------------------
// stream-json
// ---------------------------------------------------------------------------

/// Emits one JSON object per line as events arrive, ending with a
/// `{"type":"done"}` line carrying the outcome and usage totals.
pub struct StreamJsonSink<W: Write + Send> {
    out: W,
    prompt_tokens: u64,
    completion_tokens: u64,
    /// Run id -> subagent name; see [`JsonSink::subagents`].
    subagents: HashMap<u64, String>,
}

impl StreamJsonSink<std::io::Stdout> {
    pub fn stdout() -> Self {
        Self::new(std::io::stdout())
    }
}

impl<W: Write + Send> StreamJsonSink<W> {
    pub fn new(out: W) -> Self {
        Self {
            out,
            prompt_tokens: 0,
            completion_tokens: 0,
            subagents: HashMap::new(),
        }
    }

    /// See [`JsonSink::subagent_label`].
    fn subagent_label(&self, run: u64, tool: &str) -> String {
        let who = self
            .subagents
            .get(&run)
            .map(String::as_str)
            .unwrap_or("subagent");
        format!("{who} ▸ {tool}")
    }

    fn emit(&mut self, value: serde_json::Value) {
        let _ = writeln!(self.out, "{value}");
        let _ = self.out.flush();
    }
}

impl<W: Write + Send> EventSink for StreamJsonSink<W> {
    fn event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::TextDelta(text) => {
                self.emit(json!({"type": "text_delta", "text": text}));
            }
            AgentEvent::ThinkingDelta(text) => {
                self.emit(json!({"type": "thinking_delta", "text": text}));
            }
            AgentEvent::ToolStarted { name, args } => {
                self.emit(json!({"type": "tool_call", "name": name, "args": args}));
            }
            AgentEvent::SubagentRunStarted { run, name, .. } => {
                self.subagents.insert(run, name);
            }
            AgentEvent::SubagentRunToolStarted { run, name, args } => {
                let label = self.subagent_label(run, &name);
                self.emit(json!({"type": "tool_call", "name": label, "args": args}));
            }
            AgentEvent::SubagentRunToolFinished { run, name, output } => {
                let label = self.subagent_label(run, &name);
                self.emit(json!({
                    "type": "tool_result",
                    "name": label,
                    "is_error": output.is_error,
                    "output": output.content,
                }));
            }
            AgentEvent::SubagentRunDone { run, .. } => {
                self.subagents.remove(&run);
            }
            AgentEvent::SubagentRunText { .. } | AgentEvent::SubagentRunStep { .. } => {}
            AgentEvent::ToolFinished { name, output } => {
                self.emit(json!({
                    "type": "tool_result",
                    "name": name,
                    "is_error": output.is_error,
                    "output": output.content,
                }));
            }
            AgentEvent::Images { source, images } => {
                self.emit(image_json(None, &source, &images));
            }
            AgentEvent::SubagentRunImages {
                run,
                source,
                images,
            } => {
                self.emit(image_json(Some(run), &source, &images));
            }
            AgentEvent::StepCompleted { step } => {
                self.emit(json!({"type": "step", "step": step}));
            }
            AgentEvent::Error(message) => {
                self.emit(json!({"type": "error", "message": message}));
            }
            AgentEvent::Notice(message) => {
                self.emit(json!({"type": "notice", "message": message}));
            }
            AgentEvent::StreamRetrying => {
                self.emit(json!({"type": "stream_retrying"}));
            }
            AgentEvent::HookFired {
                event,
                command,
                outcome,
            } => {
                self.emit(json!({
                    "type": "hook",
                    "event": event,
                    "command": command,
                    "outcome": outcome.to_string(),
                }));
            }
            AgentEvent::PlanReady { plan, respond } => {
                // No human in the loop: report the plan and approve it.
                self.emit(json!({"type": "plan", "plan": plan, "approved": true}));
                let _ = respond.send(PlanVerdict::approve());
            }
            AgentEvent::Interview { questions, respond } => {
                // No interactive user: report the questions, then decline so
                // the model proceeds with its best judgment.
                let asked: Vec<&str> = questions.iter().map(|q| q.question.as_str()).collect();
                self.emit(json!({"type": "interview", "questions": asked, "answered": false}));
                let _ = respond.send(None);
            }
            AgentEvent::OmakaseProceeding { plan } => {
                self.emit(json!({"type": "plan", "plan": plan, "omakase": true, "approved": true}));
            }
            AgentEvent::Usage {
                prompt_tokens,
                completion_tokens,
            } => {
                self.prompt_tokens += prompt_tokens;
                self.completion_tokens += completion_tokens;
                self.emit(json!({
                    "type": "usage",
                    "prompt_tokens": prompt_tokens,
                    "completion_tokens": completion_tokens,
                }));
            }
            AgentEvent::TodoUpdated(items) => {
                self.emit(json!({
                    "type": "todo",
                    "items": serde_json::to_value(&items).unwrap_or_default(),
                }));
            }
            AgentEvent::TaskStarted { id, command } => {
                self.emit(json!({
                    "type": "task_started",
                    "id": id,
                    "command": command,
                }));
            }
            AgentEvent::TaskFinished {
                id,
                command,
                status,
            } => {
                self.emit(json!({
                    "type": "task_finished",
                    "id": id,
                    "command": command,
                    "status": status.describe(),
                }));
            }
            AgentEvent::SubagentStarted { id, name, task } => {
                self.emit(json!({
                    "type": "subagent_started",
                    "id": id,
                    "name": name,
                    "task": task,
                }));
            }
            AgentEvent::SubagentFinished {
                id,
                name,
                task,
                completed,
                output,
            } => {
                self.emit(json!({
                    "type": "subagent_finished",
                    "id": id,
                    "name": name,
                    "task": task,
                    "completed": completed,
                    "output": output,
                }));
            }
            AgentEvent::CommandRequested(line) => {
                self.emit(json!({"type": "command_requested", "command": line}));
            }
            AgentEvent::Done { reason } => {
                self.emit(json!({"type": "turn_done", "reason": reason_str(reason)}));
            }
        }
    }

    fn finish(&mut self, reason: DoneReason) {
        self.emit(json!({
            "type": "done",
            "reason": reason_str(reason),
            "usage": {
                "prompt_tokens": self.prompt_tokens,
                "completion_tokens": self.completion_tokens,
            },
        }));
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::tools::ToolOutput;
    use crate::tools::todo::{TodoItem, TodoStatus};

    /// A `Write` whose buffer outlives the sink, so tests can assert on what
    /// a moved-in sink wrote. Shared with the agent-loop integration test.
    #[derive(Clone, Default)]
    pub(crate) struct SharedBuf(pub(crate) Arc<Mutex<Vec<u8>>>);

    impl SharedBuf {
        pub(crate) fn contents(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).expect("utf-8 output")
        }
    }

    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn synthetic_events() -> Vec<AgentEvent> {
        vec![
            AgentEvent::ToolStarted {
                name: "execute".to_string(),
                args: json!({"command": "ls"}),
            },
            AgentEvent::ToolFinished {
                name: "execute".to_string(),
                output: ToolOutput::ok("file.txt"),
            },
            AgentEvent::StepCompleted { step: 1 },
            AgentEvent::ToolStarted {
                name: "execute".to_string(),
                args: json!({"command": "false"}),
            },
            AgentEvent::ToolFinished {
                name: "execute".to_string(),
                output: ToolOutput::error("exit 1"),
            },
            AgentEvent::StepCompleted { step: 2 },
            AgentEvent::TextDelta("all ".to_string()),
            AgentEvent::TextDelta("done".to_string()),
            AgentEvent::Usage {
                prompt_tokens: 100,
                completion_tokens: 20,
            },
            AgentEvent::TodoUpdated(vec![TodoItem {
                content: "ship it".to_string(),
                status: TodoStatus::InProgress,
            }]),
            AgentEvent::TaskFinished {
                id: 1,
                command: "sleep 1".to_string(),
                status: crate::tools::tasks::TaskStatus::Done(0),
            },
            AgentEvent::Done {
                reason: DoneReason::Completed,
            },
        ]
    }

    // --- exit codes ---

    #[test]
    fn exit_codes_map_run_outcomes() {
        assert_eq!(exit_code(DoneReason::Completed), 0);
        assert_eq!(exit_code(DoneReason::Stopped), 0);
        assert_eq!(exit_code(DoneReason::MaxSteps), 2);
        assert_eq!(exit_code(DoneReason::CircuitBreaker), 3);
        assert_eq!(exit_code(DoneReason::TimeLimit), 4);
    }

    // --- json ---

    #[test]
    fn json_sink_emits_one_final_summary_object() {
        let buf = SharedBuf::default();
        let mut sink = JsonSink::new(buf.clone());
        for event in synthetic_events() {
            sink.event(event);
        }
        // Nothing until finish.
        assert!(buf.contents().is_empty());
        sink.finish(DoneReason::Completed);

        let out = buf.contents();
        assert_eq!(out.lines().count(), 1, "exactly one line: {out}");
        let value: serde_json::Value = serde_json::from_str(out.trim()).expect("valid JSON");
        assert_eq!(value["result"], "all done");
        assert_eq!(value["reason"], "completed");
        assert_eq!(value["turns"], 1);
        assert_eq!(value["steps"], 2);
        assert_eq!(value["usage"]["prompt_tokens"], 100);
        assert_eq!(value["usage"]["completion_tokens"], 20);
        assert_eq!(value["tool_calls"][0]["name"], "execute");
        assert_eq!(value["tool_calls"][0]["calls"], 2);
        assert_eq!(value["tool_calls"][0]["errors"], 1);
        assert_eq!(value["errors"].as_array().map(Vec::len), Some(0));
    }

    #[test]
    fn json_sink_drops_a_retried_attempts_partial_text() {
        let buf = SharedBuf::default();
        let mut sink = JsonSink::new(buf.clone());
        sink.event(AgentEvent::TextDelta("looked around. ".to_string()));
        sink.event(AgentEvent::StepCompleted { step: 1 });
        // A second completion streams half an answer, dies, and is retried.
        sink.event(AgentEvent::TextDelta("the ans".to_string()));
        sink.event(AgentEvent::StreamRetrying);
        sink.event(AgentEvent::TextDelta("the answer is 42".to_string()));
        sink.finish(DoneReason::Completed);
        let value: serde_json::Value =
            serde_json::from_str(buf.contents().trim()).expect("valid JSON");
        assert_eq!(value["result"], "looked around. the answer is 42");
    }

    #[test]
    fn json_sink_collects_errors() {
        let buf = SharedBuf::default();
        let mut sink = JsonSink::new(buf.clone());
        sink.event(AgentEvent::Error("model unreachable".to_string()));
        sink.finish(DoneReason::CircuitBreaker);
        let value: serde_json::Value =
            serde_json::from_str(buf.contents().trim()).expect("valid JSON");
        assert_eq!(value["errors"][0], "model unreachable");
        assert_eq!(value["reason"], "circuit_breaker");
    }

    // --- stream-json ---

    #[test]
    fn stream_json_sink_emits_one_parseable_object_per_event() {
        let buf = SharedBuf::default();
        let mut sink = StreamJsonSink::new(buf.clone());
        for event in synthetic_events() {
            sink.event(event);
        }
        sink.finish(DoneReason::Completed);

        let out = buf.contents();
        let values: Vec<serde_json::Value> = out
            .lines()
            .map(|line| serde_json::from_str(line).expect("every line parses as JSON"))
            .collect();
        let types: Vec<&str> = values
            .iter()
            .map(|value| value["type"].as_str().expect("typed"))
            .collect();
        assert_eq!(
            types,
            [
                "tool_call",
                "tool_result",
                "step",
                "tool_call",
                "tool_result",
                "step",
                "text_delta",
                "text_delta",
                "usage",
                "todo",
                "task_finished",
                "turn_done",
                "done",
            ]
        );
        assert_eq!(values[0]["name"], "execute");
        assert_eq!(values[1]["is_error"], false);
        assert_eq!(values[4]["is_error"], true);
        assert_eq!(values[6]["text"], "all ");
        assert_eq!(values[9]["items"][0]["content"], "ship it");
        let done = values.last().unwrap();
        assert_eq!(done["reason"], "completed");
        assert_eq!(done["usage"]["prompt_tokens"], 100);
    }

    #[test]
    fn stream_json_plan_is_reported_and_auto_approved() {
        let buf = SharedBuf::default();
        let mut sink = StreamJsonSink::new(buf.clone());
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        sink.event(AgentEvent::PlanReady {
            plan: "1. do it".to_string(),
            respond: tx,
        });
        let verdict = rx.try_recv().expect("verdict sent synchronously");
        assert!(verdict.approved);
        let value: serde_json::Value =
            serde_json::from_str(buf.contents().trim()).expect("valid JSON");
        assert_eq!(value["type"], "plan");
        assert_eq!(value["approved"], true);
    }

    // --- text ---

    #[test]
    fn text_sink_accumulates_usage_and_approves_plans() {
        // The text sink prints to the real stdout (it shares the run's
        // spinner), so this asserts its event handling, not captured bytes.
        let mut sink = TextSink::new(Arc::new(TurnSpinner::new()));
        sink.event(AgentEvent::Usage {
            prompt_tokens: 7,
            completion_tokens: 3,
        });
        sink.event(AgentEvent::Usage {
            prompt_tokens: 5,
            completion_tokens: 1,
        });
        assert_eq!(sink.prompt_tokens, 12);
        assert_eq!(sink.completion_tokens, 4);

        let (tx, mut rx) = tokio::sync::oneshot::channel();
        sink.event(AgentEvent::PlanReady {
            plan: "plan".to_string(),
            respond: tx,
        });
        assert!(rx.try_recv().expect("verdict sent").approved);
        sink.finish(DoneReason::Completed);
    }
}
