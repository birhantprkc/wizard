//! Tool-call dispatch pipeline.
//!
//! Every tool call in every mode (TUI, headless, gateway) funnels through
//! [`Dispatcher::dispatch`]. The pipeline runs in stages, in order:
//! plan-mode gate (blocks non-read-only tools while planning) → pre-tool
//! hooks (may rewrite arguments or block) → checkpoint snapshot of
//! `Edit`-class targets (best-effort, never blocks the call) → execute →
//! post-tool hooks (may append context) → failure bookkeeping.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::Value;
use tokio::sync::mpsc;

use crate::agent::subagent::SPAWN_SUBAGENT_TOOL_NAME;
use crate::agent::{AgentEvent, DoneReason, emit, normalize_args};
use crate::config::Mode;
use crate::hooks::{HookEngine, PreToolUse};
use crate::llm::ToolCall;
use crate::tools::plan::EXIT_PLAN_TOOL_NAME;
use crate::tools::{ToolAccess, ToolContext, ToolOutput, registry::ToolRegistry};

/// Consecutive identical failures that trip the sovereign circuit breaker.
const CIRCUIT_BREAKER_LIMIT: u32 = 3;

/// Consecutive failures of one tool (any args) before the model is nudged
/// to change approach.
const TOOL_FAILURE_NUDGE: u32 = 5;
/// Consecutive failures of one tool (any args) before the turn ends with
/// [`DoneReason::CircuitBreaker`].
const TOOL_FAILURE_TRIP: u32 = 8;

/// Runs the tool-call pipeline and owns its per-session state: the tool
/// registry and the failure counters feeding the circuit breakers.
pub struct Dispatcher {
    registry: ToolRegistry,
    /// Lifecycle hooks, shared with the agent and the subagent spawner.
    hooks: Arc<HookEngine>,
    /// Sovereign runs add the identical-failure circuit breaker.
    mode: Mode,
    /// Plan-mode flag, shared with the agent (`/plan`, `--plan`) and the
    /// `exit_plan` tool (cleared on approval). While set, only read-only
    /// tools and `exit_plan` may run.
    plan_mode: Arc<AtomicBool>,
    /// Signature of the last failing tool call and how many consecutive
    /// times it has failed identically (sovereign only).
    failure_streak: Option<(String, u32)>,
    /// Per-tool consecutive-failure counts (args ignored).
    tool_failures: ToolFailureCounter,
}

/// What [`Dispatcher::dispatch`] tells the agent loop to do after one call.
#[derive(Debug)]
pub struct DispatchOutcome {
    /// Tool result to feed back to the model. `None` when the turn ended
    /// before a result could be reported (event receiver gone).
    pub output: Option<ToolOutput>,
    /// System-message nudge to inject after the feedback (repeated failures
    /// of one tool).
    pub nudge: Option<String>,
    /// Set when the turn must end early (UI gone, circuit breaker).
    pub done: Option<DoneReason>,
}

impl DispatchOutcome {
    /// The event receiver is gone: stop the turn without feedback.
    fn stopped() -> Self {
        Self {
            output: None,
            nudge: None,
            done: Some(DoneReason::Stopped),
        }
    }
}

impl Dispatcher {
    pub fn new(
        registry: ToolRegistry,
        mode: Mode,
        hooks: Arc<HookEngine>,
        plan_mode: Arc<AtomicBool>,
    ) -> Self {
        Self {
            registry,
            hooks,
            mode,
            plan_mode,
            failure_streak: None,
            tool_failures: ToolFailureCounter::default(),
        }
    }

    /// The registered tools (for specs and lookups).
    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    /// Swap the tool registry (after `/reload` or `/evolve`).
    pub fn set_registry(&mut self, registry: ToolRegistry) {
        self.registry = registry;
    }

    /// Track a mode switch (the identical-failure breaker is sovereign-only).
    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    /// Forget all failure state (`/clear`).
    pub fn reset_failures(&mut self) {
        self.failure_streak = None;
        self.tool_failures.reset();
    }

    /// Run one tool call through the pipeline.
    pub async fn dispatch(
        &mut self,
        call: &ToolCall,
        ctx: &ToolContext,
        events: &mpsc::Sender<AgentEvent>,
    ) -> DispatchOutcome {
        let name = call.function.name.clone();
        let mut args = normalize_args(&call.function.arguments);

        // Plan-mode gate, first stage: while planning, only read-only tools
        // and exit_plan may run. The block feeds back to the model as an
        // ordinary tool error but is exempt from the failure breakers — a
        // model probing for write access mid-plan must not end the turn.
        // Unknown tools fall through so the real "unknown tool" error
        // surfaces instead. Delegation stays available but demoted: the
        // spawn call is tagged so the subagent runs with a read-only scope.
        if self.plan_mode.load(Ordering::SeqCst) && name != EXIT_PLAN_TOOL_NAME {
            if name == SPAWN_SUBAGENT_TOOL_NAME {
                if let Some(object) = args.as_object_mut() {
                    object.insert("plan_mode".to_string(), Value::Bool(true));
                }
            } else if self
                .registry
                .get(&name)
                .is_some_and(|tool| tool.access() != ToolAccess::ReadOnly)
            {
                let output = ToolOutput::error(
                    "blocked by plan mode: only read-only tools are allowed; finish your plan \
                     and call exit_plan",
                );
                if !emit(
                    events,
                    AgentEvent::ToolFinished {
                        name,
                        output: output.clone(),
                    },
                )
                .await
                {
                    return DispatchOutcome::stopped();
                }
                return DispatchOutcome {
                    output: Some(output),
                    nudge: None,
                    done: None,
                };
            }
        }

        // Pre-tool hooks: may rewrite the arguments or veto the call. A veto
        // feeds back to the model as an ordinary tool error (not fatal), so
        // the failure breakers cover repeated blocked calls too.
        match self
            .hooks
            .pre_tool_use(&name, &args, self.mode, Some(events))
            .await
        {
            PreToolUse::Continue(updated) => {
                if let Some(updated) = updated {
                    args = updated;
                }
            }
            PreToolUse::Block(reason) => {
                let output = ToolOutput::error(format!("blocked by pre_tool_use hook: {reason}"));
                return self.bookkeep(&name, &args, output, events).await;
            }
        }

        // Checkpoint stage: snapshot the target of an Edit-class tool so the
        // turn can be rewound. Runs after the pre-hooks (which may have
        // rewritten the path) and never fails the call.
        crate::checkpoint::snapshot_edit_target(&self.registry, &name, &args, ctx);

        let Some(mut output) = self.execute(&name, args.clone(), ctx, events).await else {
            return DispatchOutcome::stopped();
        };

        // Post-tool hooks: stdout becomes extra context on the tool result.
        if let Some(extra) = self
            .hooks
            .post_tool_use_with_output(
                &name,
                &args,
                &output.content,
                output.is_error,
                self.mode,
                Some(events),
            )
            .await
        {
            crate::hooks::append_context(&mut output.content, &extra);
        }

        self.bookkeep(&name, &args, output, events).await
    }

    /// Execute stage: announce the call and run the tool. `None` when the
    /// event receiver is gone and the turn must stop.
    async fn execute(
        &self,
        name: &str,
        args: Value,
        ctx: &ToolContext,
        events: &mpsc::Sender<AgentEvent>,
    ) -> Option<ToolOutput> {
        if !emit(
            events,
            AgentEvent::ToolStarted {
                name: name.to_string(),
                args: args.clone(),
            },
        )
        .await
        {
            return None;
        }
        // Tools run with the turn's event channel in their context, so a
        // tool that converses with the surface (exit_plan's approval
        // round-trip) can reach it.
        let ctx = ctx.with_events(events.clone());
        Some(match self.registry.execute(name, args, &ctx).await {
            Ok(output) => output,
            Err(err) => ToolOutput::error(err.to_string()),
        })
    }

    /// Failure-bookkeeping stage: report the result and update both circuit
    /// breakers.
    async fn bookkeep(
        &mut self,
        name: &str,
        args: &Value,
        output: ToolOutput,
        events: &mpsc::Sender<AgentEvent>,
    ) -> DispatchOutcome {
        if !emit(
            events,
            AgentEvent::ToolFinished {
                name: name.to_string(),
                output: output.clone(),
            },
        )
        .await
        {
            return DispatchOutcome::stopped();
        }

        let breaker_tripped = self.track_failure(name, args, &output);
        let failure_action = self.tool_failures.record(name, output.is_error);

        if breaker_tripped {
            let _ = emit(
                events,
                AgentEvent::Error(format!(
                    "circuit breaker: '{name}' failed identically {CIRCUIT_BREAKER_LIMIT} times in a row"
                )),
            )
            .await;
            return DispatchOutcome {
                output: Some(output),
                nudge: None,
                done: Some(DoneReason::CircuitBreaker),
            };
        }
        match failure_action {
            FailureAction::Continue => DispatchOutcome {
                output: Some(output),
                nudge: None,
                done: None,
            },
            FailureAction::Nudge => DispatchOutcome {
                output: Some(output),
                nudge: Some(format!(
                    "Repeated failures with tool '{name}' ({TOOL_FAILURE_NUDGE} in a row) — \
                     stop retrying it and change approach."
                )),
                done: None,
            },
            FailureAction::Trip => {
                let _ = emit(
                    events,
                    AgentEvent::Error(format!(
                        "circuit breaker: '{name}' failed {TOOL_FAILURE_TRIP} times in a row"
                    )),
                )
                .await;
                DispatchOutcome {
                    output: Some(output),
                    nudge: None,
                    done: Some(DoneReason::CircuitBreaker),
                }
            }
        }
    }

    /// Update identical-failure circuit-breaker state (sovereign only).
    /// Returns true when the breaker trips.
    fn track_failure(&mut self, name: &str, args: &Value, output: &ToolOutput) -> bool {
        if self.mode != Mode::Sovereign {
            return false;
        }
        if !output.is_error {
            self.failure_streak = None;
            return false;
        }
        let signature = format!("{name}\u{1}{args}\u{1}{}", output.content);
        let count = match &self.failure_streak {
            Some((last, count)) if *last == signature => count + 1,
            _ => 1,
        };
        self.failure_streak = Some((signature, count));
        count >= CIRCUIT_BREAKER_LIMIT
    }
}

/// What [`ToolFailureCounter::record`] says to do after a tool result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureAction {
    Continue,
    /// Inject a system nudge telling the model to stop retrying the tool.
    Nudge,
    /// End the turn via the circuit breaker.
    Trip,
}

/// Per-tool-name consecutive-failure counter, independent of arguments
/// (catches models that jitter args to dodge the identical-failure
/// breaker). A success of a tool resets that tool's count.
#[derive(Debug, Default)]
struct ToolFailureCounter {
    counts: std::collections::HashMap<String, u32>,
}

impl ToolFailureCounter {
    /// Record one tool result and return the action it warrants.
    fn record(&mut self, name: &str, failed: bool) -> FailureAction {
        if !failed {
            self.counts.remove(name);
            return FailureAction::Continue;
        }
        let count = self.counts.entry(name.to_string()).or_insert(0);
        *count += 1;
        match *count {
            TOOL_FAILURE_NUDGE => FailureAction::Nudge,
            count if count >= TOOL_FAILURE_TRIP => FailureAction::Trip,
            _ => FailureAction::Continue,
        }
    }

    fn reset(&mut self) {
        self.counts.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_failures_nudge_then_trip() {
        let mut counter = ToolFailureCounter::default();
        for i in 1..TOOL_FAILURE_NUDGE {
            assert_eq!(
                counter.record("execute", true),
                FailureAction::Continue,
                "failure {i}"
            );
        }
        assert_eq!(counter.record("execute", true), FailureAction::Nudge);
        for i in TOOL_FAILURE_NUDGE + 1..TOOL_FAILURE_TRIP {
            assert_eq!(
                counter.record("execute", true),
                FailureAction::Continue,
                "failure {i}"
            );
        }
        assert_eq!(counter.record("execute", true), FailureAction::Trip);
    }

    #[test]
    fn tool_failures_reset_on_success_of_that_tool() {
        let mut counter = ToolFailureCounter::default();
        for _ in 0..TOOL_FAILURE_NUDGE - 1 {
            counter.record("execute", true);
        }
        assert_eq!(counter.record("execute", false), FailureAction::Continue);
        // The streak starts over after the success.
        for i in 1..TOOL_FAILURE_NUDGE {
            assert_eq!(
                counter.record("execute", true),
                FailureAction::Continue,
                "failure {i}"
            );
        }
        assert_eq!(counter.record("execute", true), FailureAction::Nudge);
    }

    #[test]
    fn tool_failures_count_per_tool_name() {
        let mut counter = ToolFailureCounter::default();
        for _ in 0..TOOL_FAILURE_NUDGE - 1 {
            counter.record("execute", true);
            counter.record("write_file", true);
        }
        // Each tool reaches the nudge threshold independently; a success of
        // one tool does not reset the other.
        counter.record("write_file", false);
        assert_eq!(counter.record("execute", true), FailureAction::Nudge);
        assert_eq!(counter.record("write_file", true), FailureAction::Continue);
    }

    #[test]
    fn tool_failures_reset_clears_all_counts() {
        let mut counter = ToolFailureCounter::default();
        for _ in 0..TOOL_FAILURE_TRIP {
            counter.record("execute", true);
        }
        counter.reset();
        assert_eq!(counter.record("execute", true), FailureAction::Continue);
    }
}
