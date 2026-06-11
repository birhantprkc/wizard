//! Shared todo-list state, exposed to tools via
//! [`ToolContext`](super::ToolContext).

/// Progress state of one todo item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

/// One entry in the agent's working todo list.
#[derive(Debug, Clone)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
}

/// The agent's working todo list.
pub type TodoList = Vec<TodoItem>;
