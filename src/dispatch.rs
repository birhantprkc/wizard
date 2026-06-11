//! Tool-call dispatch pipeline.
//!
//! Every tool call in every mode (TUI, headless, gateway) funnels through
//! [`Dispatcher::dispatch`]. The pipeline runs in stages so upcoming
//! features slot in between them, in order: plan-mode gate → pre-tool hooks
//! → checkpoint snapshot → execute → post-tool hooks → failure bookkeeping.

use serde_json::Value;
use tokio::sync::mpsc;

use crate::agent::{AgentEvent, DoneReason, emit, normalize_args};
use crate::config::Mode;
use crate::llm::ToolCall;
use crate::tools::{ToolContext, ToolOutput, registry::ToolRegistry};

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
    /// Sovereign runs add the identical-failure circuit breaker.
    mode: Mode,
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
    pub fn new(registry: ToolRegistry, mode: Mode) -> Self {
        Self {
            registry,
            mode,
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
        let args = normalize_args(&call.function.arguments);

        let Some(output) = self.execute(&name, args.clone(), ctx, events).await else {
            return DispatchOutcome::stopped();
        };
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
        Some(match self.registry.execute(name, args, ctx).await {
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
