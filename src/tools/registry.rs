//! Unified tool registry: native + scripted + MCP tools behind one lookup,
//! so the agent loop and the model treat all three identically.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::llm::ToolSpec;
use crate::mcp::McpManager;
use crate::tools::{Tool, ToolContext, ToolError, ToolOutput};

use super::file::{EditFileTool, ListFilesTool, ReadFileTool, SearchFilesTool, WriteFileTool};
use super::git::{GitDiffTool, GitStatusTool};
use super::memory::MemoryTool;
use super::shell::ExecuteTool;

/// Registry of every callable tool, keyed by advertised name.
/// Registration order is preserved for stable spec ordering in prompts.
#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
    order: Vec<String>,
}

impl ToolRegistry {
    /// Empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registry pre-populated with all native tools
    /// (`read_file`, `write_file`, `edit_file`, `list_files`,
    /// `search_files`, `execute`, `git_status`, `git_diff`, `memory`).
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

    /// Dispatch a tool call by name. Approval gating happens in the agent
    /// loop before this is reached.
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
    /// reload both dynamic sources.
    pub async fn reload(&mut self, scripted_dir: &Path, manager: &McpManager) -> Result<()> {
        *self = Self::with_native_tools();
        self.load_scripted(scripted_dir)?;
        self.attach_mcp(manager).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use async_trait::async_trait;
    use serde_json::json;

    use super::*;

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
            ]
        );
        assert_eq!(registry.len(), 9);
        assert!(!registry.is_empty());

        for spec in registry.specs() {
            assert_eq!(spec.kind, "function");
            assert!(!spec.function.description.is_empty());
            assert_eq!(spec.function.parameters["type"], "object");
        }
    }

    #[test]
    fn risky_tools_require_approval_and_read_only_tools_do_not() {
        let registry = ToolRegistry::with_native_tools();
        let approval = |name: &str| registry.get(name).expect(name).requires_approval();

        for risky in ["write_file", "edit_file", "execute"] {
            assert!(approval(risky), "{risky} must be gated behind approval");
        }
        for safe in [
            "read_file",
            "list_files",
            "search_files",
            "git_status",
            "git_diff",
            "memory",
        ] {
            assert!(!approval(safe), "{safe} must not require approval");
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
        assert!(
            tool.requires_approval(),
            "scripted tools are risky by default"
        );
    }
}
