//! Background subagent registry, shared across tool calls via
//! [`ToolContext`](super::ToolContext).
//!
//! `spawn_subagent` with `background: true` detaches the subagent's run as a
//! tokio task and registers it here instead of awaiting it inline. The agent
//! loop calls [`SubagentTaskRegistry::drain_completed`] at the top of every
//! step to notify the model of finished subagents exactly once, mirroring
//! [`super::tasks::TaskRegistry`] for background shell commands.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

/// A finished background subagent run, returned by
/// [`SubagentTaskRegistry::drain_completed`] exactly once.
#[derive(Debug, Clone)]
pub struct SubagentTaskResult {
    pub id: u32,
    pub name: String,
    pub task: String,
    /// False when the sub-loop hit its step budget or errored out.
    pub completed: bool,
    pub output: String,
    pub steps_used: u32,
}

/// Internal per-run state. `result` is `None` while running.
#[derive(Debug)]
struct Entry {
    name: String,
    task: String,
    result: Option<(bool, String, u32)>,
    /// Already returned by [`SubagentTaskRegistry::drain_completed`].
    reported: bool,
}

/// Session-wide registry of background subagent runs.
#[derive(Debug, Default)]
pub struct SubagentTaskRegistry {
    entries: Mutex<HashMap<u32, Entry>>,
    next_id: AtomicU32,
}

impl SubagentTaskRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u32, Entry>> {
        self.entries
            .lock()
            .expect("subagent task registry lock poisoned")
    }

    /// Register `name`/`task` as running and detach `fut` as a tokio task;
    /// returns the new id immediately. `fut` resolves to
    /// `(completed, output, steps_used)` once the subagent finishes.
    pub fn spawn(
        self: &Arc<Self>,
        name: &str,
        task: &str,
        fut: impl Future<Output = (bool, String, u32)> + Send + 'static,
    ) -> u32 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        self.lock().insert(
            id,
            Entry {
                name: name.to_string(),
                task: task.to_string(),
                result: None,
                reported: false,
            },
        );
        let registry = Arc::clone(self);
        tokio::spawn(async move {
            let result = fut.await;
            registry.finish(id, result);
        });
        id
    }

    fn finish(&self, id: u32, result: (bool, String, u32)) {
        if let Some(entry) = self.lock().get_mut(&id) {
            entry.result = Some(result);
        }
    }

    /// Count of runs still in flight (registered, no result yet). Read-only
    /// peek; does not consume anything `drain_completed` would return.
    pub fn pending_count(&self) -> usize {
        self.lock().values().filter(|e| e.result.is_none()).count()
    }

    /// Finished subagent runs not yet reported, each returned exactly once,
    /// ordered by id.
    pub fn drain_completed(&self) -> Vec<SubagentTaskResult> {
        let mut finished = Vec::new();
        for (id, entry) in self.lock().iter_mut() {
            if entry.reported {
                continue;
            }
            if let Some((completed, output, steps_used)) = entry.result.take() {
                entry.reported = true;
                finished.push(SubagentTaskResult {
                    id: *id,
                    name: entry.name.clone(),
                    task: entry.task.clone(),
                    completed,
                    output,
                    steps_used,
                });
            }
        }
        finished.sort_unstable_by_key(|task| task.id);
        finished
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    async fn wait_drained(registry: &Arc<SubagentTaskRegistry>) -> Vec<SubagentTaskResult> {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let drained = registry.drain_completed();
            if !drained.is_empty() {
                return drained;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no subagent task finished in time"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn spawn_returns_immediately_and_drains_exactly_once() {
        let registry = Arc::new(SubagentTaskRegistry::new());
        let id = registry.spawn("worker", "investigate X", async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            (true, "found X".to_string(), 3)
        });
        assert_eq!(id, 1);
        // Not finished yet — `spawn` must not block on the future.
        assert!(registry.drain_completed().is_empty());

        let drained = wait_drained(&registry).await;
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].id, 1);
        assert_eq!(drained[0].name, "worker");
        assert_eq!(drained[0].task, "investigate X");
        assert!(drained[0].completed);
        assert_eq!(drained[0].output, "found X");
        assert_eq!(drained[0].steps_used, 3);

        assert!(
            registry.drain_completed().is_empty(),
            "finished subagent runs are reported exactly once"
        );
    }

    #[tokio::test]
    async fn drain_skips_runs_still_in_flight() {
        let registry = Arc::new(SubagentTaskRegistry::new());
        let quick = registry.spawn("worker", "quick", async { (true, "done".to_string(), 1) });
        let _slow = registry.spawn("worker", "slow", async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            (true, "done".to_string(), 1)
        });

        let drained = wait_drained(&registry).await;
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].id, quick);
    }
}
