//! Unified tool registry: native + scripted + MCP tools behind one lookup,
//! so the agent loop and the model treat all three identically.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use async_trait::async_trait;

use crate::llm::ToolSpec;
use crate::mcp::McpManager;
use crate::tools::{Tool, ToolAccess, ToolContext, ToolError, ToolKind, ToolOutput};

use super::command::RunCommandTool;
use super::compact::CompactTool;
use super::file::{EditFileTool, ListFilesTool, ReadFileTool, SearchFilesTool, WriteFileTool};
use super::git::{GitDiffTool, GitStatusTool};
use super::image::GenerateImageTool;
use super::memory::MemoryTool;
use super::shell::ExecuteTool;
use super::subagent_tasks::{SubagentKillTool, SubagentStatusTool};
use super::tasks::{TaskKillTool, TaskOutputTool};
use super::todo::TodoTool;
use super::web::{WebFetchTool, WebSearchTool};

/// Registry of every callable tool, keyed by advertised name.
/// Registration order is preserved for stable spec ordering in prompts.
#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
    order: Vec<String>,
}

impl Clone for ToolRegistry {
    /// Shallow snapshot: each tool is an `Arc`, so this is a cheap handle clone
    /// (used by `/fork` so a mid-turn side-quest can keep the parent's tool set
    /// without borrowing the live dispatcher).
    fn clone(&self) -> Self {
        Self {
            tools: self.tools.clone(),
            order: self.order.clone(),
        }
    }
}

impl ToolRegistry {
    /// Empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Cheap handle clone of every registered tool (see [`Clone`]).
    pub fn snapshot(&self) -> Self {
        self.clone()
    }

    /// Registry pre-populated with all native tools
    /// (`read_file`, `write_file`, `edit_file`, `list_files`,
    /// `search_files`, `execute`, `git_status`, `git_diff`, `memory`,
    /// `todo`, `web_fetch`, `web_search`, `generate_image`, `task_output`,
    /// `task_kill`, `subagent_status`, `subagent_kill`, `run_command`,
    /// `compact`).
    pub fn with_native_tools() -> Self {
        let mut registry = Self::new();
        registry.register(Arc::new(ReadFileTool));
        registry.register(Arc::new(WriteFileTool));
        registry.register(Arc::new(EditFileTool));
        registry.register(Arc::new(ListFilesTool));
        registry.register(Arc::new(SearchFilesTool));
        registry.register(Arc::new(ExecuteTool));
        registry.register(Arc::new(GitStatusTool));
        registry.register(Arc::new(GitDiffTool));
        registry.register(Arc::new(MemoryTool));
        registry.register(Arc::new(TodoTool));
        registry.register(Arc::new(WebFetchTool));
        registry.register(Arc::new(WebSearchTool));
        registry.register(Arc::new(GenerateImageTool));
        registry.register(Arc::new(TaskOutputTool));
        registry.register(Arc::new(TaskKillTool));
        registry.register(Arc::new(SubagentStatusTool));
        registry.register(Arc::new(SubagentKillTool));
        registry.register(Arc::new(RunCommandTool));
        registry.register(Arc::new(CompactTool));
        registry
    }

    /// Register (or replace) a tool under its advertised name.
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.name().to_string();
        if !self.tools.contains_key(&name) {
            self.order.push(name.clone());
        }
        self.tools.insert(name, tool);
    }

    /// Look up a tool by name.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    /// Number of registered tools.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether the registry has no tools.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Wire-format specs for the request `tools` array, in registration
    /// order.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.order
            .iter()
            .filter_map(|name| self.tools.get(name))
            .map(|tool| tool.spec())
            .collect()
    }

    /// Load every scripted tool manifest from `dir` (normally
    /// `~/.wizard/tools/`) and register them. Returns how many were added.
    pub fn load_scripted(&mut self, dir: &Path) -> Result<usize> {
        let scripted = super::scripted::load_dir(dir)?;
        let count = scripted.len();
        for tool in scripted {
            self.register(Arc::new(tool));
        }
        Ok(count)
    }

    /// Merge the tools of every connected MCP server. Returns how many were
    /// added.
    pub async fn attach_mcp(&mut self, manager: &McpManager) -> Result<usize> {
        let tools = manager.tools().await?;
        let count = tools.len();
        for tool in tools {
            self.register(tool);
        }
        Ok(count)
    }

    /// Dispatch a tool call by name.
    pub async fn execute(
        &self,
        name: &str,
        args: Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| ToolError::UnknownTool(name.to_string()))?;
        tool.execute(args, ctx).await
    }

    /// `/reload`: drop scripted and MCP tools, re-register natives, and
    /// reload both dynamic sources. Harness description overrides are
    /// re-applied last so they keep shadowing across reloads.
    pub async fn reload(&mut self, scripted_dir: &Path, manager: &McpManager) -> Result<()> {
        *self = Self::with_native_tools();
        self.load_scripted(scripted_dir)?;
        self.attach_mcp(manager).await?;
        self.apply_harness_overrides();
        Ok(())
    }

    /// Apply the active harness bundle's tool-description overrides
    /// (`<harness_dir>/tool_descriptions/<tool>.md`), if a bundle is set.
    /// Returns how many descriptions were replaced.
    pub fn apply_harness_overrides(&mut self) -> usize {
        match crate::config::Config::harness_dir() {
            Some(dir) => self.apply_description_overrides(&dir.join("tool_descriptions")),
            None => 0,
        }
    }

    /// Replace the advertised description of every registered tool that has
    /// a matching non-empty `<dir>/<tool_name>.md`. Files that name no
    /// registered tool are skipped with a warning (the evolve loop may edit
    /// a description for a tool this build doesn't ship). Returns how many
    /// descriptions were replaced.
    pub fn apply_description_overrides(&mut self, dir: &Path) -> usize {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(_) => return 0, // no overrides shipped — the common case
        };
        let mut replaced = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Some(name) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            let Some(tool) = self.tools.get(&name) else {
                tracing::warn!(
                    "harness description override {} names no registered tool",
                    path.display()
                );
                continue;
            };
            let Ok(text) = std::fs::read_to_string(&path) else {
                tracing::warn!("unreadable harness description override {}", path.display());
                continue;
            };
            let description = text.trim().to_string();
            if description.is_empty() {
                continue; // empty file ⇒ keep the compiled default
            }
            self.register(Arc::new(DescriptionOverride {
                inner: Arc::clone(tool),
                description,
            }));
            replaced += 1;
        }
        replaced
    }
}

/// A registered tool with its advertised description replaced by a harness
/// bundle override. Everything except the description delegates to the
/// wrapped tool, so behavior, access class, and origin are unchanged.
struct DescriptionOverride {
    inner: Arc<dyn Tool>,
    description: String,
}

#[async_trait]
impl Tool for DescriptionOverride {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        self.inner.parameters()
    }

    fn access(&self) -> ToolAccess {
        self.inner.access()
    }

    fn kind(&self) -> ToolKind {
        self.inner.kind()
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        self.inner.execute(args, ctx).await
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use async_trait::async_trait;
    use serde_json::json;

    use super::*;
    use crate::tools::ToolAccess;

    /// Temp project dir removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("wizard-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }

        fn ctx(&self) -> ToolContext {
            ToolContext::new(&self.0)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Minimal tool that records nothing and echoes a fixed string.
    struct FakeTool {
        name: &'static str,
        reply: &'static str,
    }

    #[async_trait]
    impl Tool for FakeTool {
        fn name(&self) -> &str {
            self.name
        }

        fn description(&self) -> &str {
            "fake tool for registry tests"
        }

        fn parameters(&self) -> Value {
            serde_json::json!({ "type": "object", "properties": {} })
        }

        async fn execute(&self, _args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::ok(self.reply))
        }
    }

    #[test]
    fn native_registry_advertises_documented_tools_in_order() {
        let registry = ToolRegistry::with_native_tools();
        let names: Vec<String> = registry
            .specs()
            .into_iter()
            .map(|spec| spec.function.name)
            .collect();
        assert_eq!(
            names,
            [
                "read_file",
                "write_file",
                "edit_file",
                "list_files",
                "search_files",
                "execute",
                "git_status",
                "git_diff",
                "memory",
                "todo",
                "web_fetch",
                "web_search",
                "generate_image",
                "task_output",
                "task_kill",
                "subagent_status",
                "subagent_kill",
                "run_command",
                "compact",
            ]
        );
        assert_eq!(registry.len(), 19);
        assert!(!registry.is_empty());

        for spec in registry.specs() {
            assert_eq!(spec.kind, "function");
            assert!(!spec.function.description.is_empty());
            assert_eq!(spec.function.parameters["type"], "object");
        }
    }

    #[test]
    fn native_tools_report_their_access_class() {
        let registry = ToolRegistry::with_native_tools();
        let access = |name: &str| registry.get(name).expect(name).access();

        for read_only in [
            "read_file",
            "list_files",
            "search_files",
            "git_status",
            "git_diff",
            // `todo` mutates only agent-local state, so it stays usable in
            // plan mode.
            "todo",
            // The web tools only observe the outside world.
            "web_fetch",
            "web_search",
            // `task_output` only reads buffered task state.
            "task_output",
            // `subagent_status` only reads registry state.
            "subagent_status",
            // `compact` only rewrites conversation history.
            "compact",
        ] {
            assert_eq!(access(read_only), ToolAccess::ReadOnly, "{read_only}");
        }
        for edit in ["write_file", "edit_file"] {
            assert_eq!(access(edit), ToolAccess::Edit, "{edit}");
        }
        // `execute` runs arbitrary commands; `memory` mutates its store;
        // `generate_image` hits the network and writes files; `task_kill` and
        // `subagent_kill` terminate running work.
        for side_effecting in [
            "execute",
            "memory",
            "generate_image",
            "task_kill",
            "subagent_kill",
        ] {
            assert_eq!(
                access(side_effecting),
                ToolAccess::Execute,
                "{side_effecting}"
            );
        }
    }

    #[tokio::test]
    async fn execute_dispatches_by_name() {
        let tmp = TempDir::new();
        std::fs::write(tmp.0.join("hello.txt"), "hello registry\n").unwrap();

        let registry = ToolRegistry::with_native_tools();
        let out = registry
            .execute("read_file", json!({ "path": "hello.txt" }), &tmp.ctx())
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("hello registry"));
    }

    #[tokio::test]
    async fn execute_unknown_tool_is_an_error() {
        let tmp = TempDir::new();
        let registry = ToolRegistry::with_native_tools();
        let err = registry
            .execute("summon_demon", json!({}), &tmp.ctx())
            .await
            .expect_err("unknown tool must fail");
        assert!(matches!(err, ToolError::UnknownTool(name) if name == "summon_demon"));
    }

    #[test]
    fn register_replaces_by_name_without_duplicating_order() {
        let mut registry = ToolRegistry::new();
        assert!(registry.is_empty());

        registry.register(Arc::new(FakeTool {
            name: "probe",
            reply: "v1",
        }));
        registry.register(Arc::new(FakeTool {
            name: "other",
            reply: "x",
        }));
        registry.register(Arc::new(FakeTool {
            name: "probe",
            reply: "v2",
        }));

        assert_eq!(registry.len(), 2);
        let names: Vec<String> = registry
            .specs()
            .into_iter()
            .map(|spec| spec.function.name)
            .collect();
        assert_eq!(
            names,
            ["probe", "other"],
            "re-registration keeps original order"
        );
    }

    #[tokio::test]
    async fn replaced_tool_dispatches_to_the_new_implementation() {
        let tmp = TempDir::new();
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(FakeTool {
            name: "probe",
            reply: "v1",
        }));
        registry.register(Arc::new(FakeTool {
            name: "probe",
            reply: "v2",
        }));

        let out = registry
            .execute("probe", json!({}), &tmp.ctx())
            .await
            .unwrap();
        assert_eq!(out.content, "v2");
    }

    #[tokio::test]
    async fn description_overrides_shadow_matching_tools_only() {
        let tmp = TempDir::new();
        let dir = tmp.0.join("tool_descriptions");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("read_file.md"), "EVOLVED read_file description\n").unwrap();
        std::fs::write(dir.join("no_such_tool.md"), "orphan override\n").unwrap();
        std::fs::write(dir.join("todo.md"), "  \n").unwrap(); // empty ⇒ keep default
        std::fs::write(dir.join("execute.txt"), "wrong extension\n").unwrap();

        let mut registry = ToolRegistry::with_native_tools();
        let default_todo = registry.get("todo").unwrap().description().to_string();
        let order_before: Vec<String> = registry
            .specs()
            .into_iter()
            .map(|spec| spec.function.name)
            .collect();

        assert_eq!(registry.apply_description_overrides(&dir), 1);

        let read_file = registry.get("read_file").unwrap();
        assert_eq!(read_file.description(), "EVOLVED read_file description");
        assert_eq!(
            read_file.access(),
            ToolAccess::ReadOnly,
            "override keeps the wrapped tool's access class"
        );
        assert_eq!(registry.get("todo").unwrap().description(), default_todo);
        assert!(registry.get("no_such_tool").is_none());
        let order_after: Vec<String> = registry
            .specs()
            .into_iter()
            .map(|spec| spec.function.name)
            .collect();
        assert_eq!(order_before, order_after, "overrides keep spec order");

        // The wrapped tool still executes: behavior is untouched.
        std::fs::write(tmp.0.join("hello.txt"), "hello override\n").unwrap();
        let out = registry
            .execute("read_file", json!({ "path": "hello.txt" }), &tmp.ctx())
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("hello override"));
    }

    #[test]
    fn description_overrides_missing_dir_is_a_noop() {
        let tmp = TempDir::new();
        let mut registry = ToolRegistry::with_native_tools();
        assert_eq!(
            registry.apply_description_overrides(&tmp.0.join("absent")),
            0
        );
        assert_eq!(registry.len(), 19);
    }

    #[test]
    fn load_scripted_registers_manifest_tools() {
        let tmp = TempDir::new();
        std::fs::write(tmp.0.join("greet.sh"), "#!/bin/sh\necho hi\n").unwrap();
        std::fs::write(
            tmp.0.join("greet.toml"),
            "name = \"greet\"\ndescription = \"say hi\"\nscript = \"greet.sh\"\ninterpreter = \"sh\"\n",
        )
        .unwrap();

        let mut registry = ToolRegistry::with_native_tools();
        let added = registry.load_scripted(&tmp.0).unwrap();
        assert_eq!(added, 1);
        let tool = registry.get("greet").expect("scripted tool registered");
        assert_eq!(tool.kind(), crate::tools::ToolKind::Scripted);
        assert_eq!(
            tool.access(),
            ToolAccess::Execute,
            "scripted tools keep the Execute default"
        );
    }
}
