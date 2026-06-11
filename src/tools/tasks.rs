//! Background task registry, shared across tool calls via
//! [`ToolContext`](super::ToolContext).
//!
//! Bookkeeping for shell commands running in the background: ids, the
//! command line, and lifecycle state. Process handles and output buffers
//! attach to [`Task`] when the background-execution tools land.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

/// Lifecycle state of one background task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Running,
    /// Exited with the given code.
    Done(i32),
    /// Terminated on request.
    Killed,
}

/// One background command and its state.
#[derive(Debug, Clone)]
pub struct Task {
    pub id: u32,
    pub command: String,
    pub status: TaskStatus,
}

/// Session-wide registry of background tasks.
#[derive(Debug, Default)]
pub struct TaskRegistry {
    tasks: Mutex<HashMap<u32, Task>>,
    next_id: AtomicU32,
}

impl TaskRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new running task and return its id (1-based).
    pub fn add(&self, command: impl Into<String>) -> u32 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let task = Task {
            id,
            command: command.into(),
            status: TaskStatus::Running,
        };
        self.tasks
            .lock()
            .expect("task registry lock poisoned")
            .insert(id, task);
        id
    }

    /// Snapshot of all tasks, ordered by id.
    pub fn list(&self) -> Vec<Task> {
        let mut tasks: Vec<Task> = self
            .tasks
            .lock()
            .expect("task registry lock poisoned")
            .values()
            .cloned()
            .collect();
        tasks.sort_unstable_by_key(|task| task.id);
        tasks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_assigns_sequential_ids_and_list_is_ordered() {
        let registry = TaskRegistry::new();
        assert!(registry.list().is_empty());

        let first = registry.add("cargo build");
        let second = registry.add("cargo test");
        assert_eq!(first, 1);
        assert_eq!(second, 2);

        let tasks = registry.list();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].id, 1);
        assert_eq!(tasks[0].command, "cargo build");
        assert_eq!(tasks[0].status, TaskStatus::Running);
        assert_eq!(tasks[1].id, 2);
    }
}
