//! The `compact` tool: summarize older history into a progress note *now*,
//! mid-turn, on every surface (TUI, GUI, headless, gateway).
//!
//! The real work lives on [`Agent::compact_now`](crate::agent::Agent::compact_now)
//! and is invoked from the agent loop's dispatch path — a tool cannot borrow
//! `&mut Agent` through [`ToolContext`]. Direct registry execution (tests,
//! subagents, MCP serve) hits [`CompactTool::execute`] and gets a clear
//! error so nothing fails silently.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Tool, ToolAccess, ToolContext, ToolError, ToolOutput};

/// Advertised name of the tool.
pub const COMPACT_TOOL_NAME: &str = "compact";

/// `compact` — force mid-turn history summarization.
pub struct CompactTool;

#[async_trait]
impl Tool for CompactTool {
    fn name(&self) -> &str {
        COMPACT_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Summarize older conversation history into a progress note right now \
         (keeps the recent tail verbatim). Runs mid-turn on every surface — \
         TUI, GUI, headless, and gateway — so prefer this over \
         `run_command` `/compact` (which only queues until the turn ends, and \
         is unavailable headless). Call after a long investigation, a finished \
         sub-goal, or when a pressure signal is elevated/high. No parameters."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    fn access(&self) -> ToolAccess {
        // History-only: no project files, no shell. Stays available in plan mode
        // so a long investigation can shed weight before exit_plan.
        ToolAccess::ReadOnly
    }

    async fn execute(&self, _args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        // The parent agent loop intercepts this name before execute and runs
        // `Agent::compact_now`. Reaching here means a surface without that
        // intercept (subagent loop, direct registry, MCP serve).
        Ok(ToolOutput::error(
            "compact runs only in the main agent loop; stay lean and finish \
             with a short report instead",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolContext;

    #[tokio::test]
    async fn direct_execute_explains_the_intercept() {
        let tool = CompactTool;
        let ctx = ToolContext::new(std::env::temp_dir());
        let out = tool.execute(json!({}), &ctx).await.unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("main agent loop"), "{}", out.content);
    }

    #[test]
    fn compact_is_read_only_for_plan_mode() {
        assert_eq!(CompactTool.access(), ToolAccess::ReadOnly);
    }
}
