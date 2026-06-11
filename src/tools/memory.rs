//! Native `memory` tool: persist project facts across sessions.
//!
//! Backed by `crate::memory::MemoryStore` (one markdown file per memory
//! under `~/.wizard/memory/<project-slug>/`). The index of saved memories is
//! injected into the system prompt at session start, so a save made now is
//! recalled automatically next session. Confined to `~/.wizard/memory`.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::memory::MemoryStore;

use super::{
    MAX_OUTPUT_BYTES, Tool, ToolContext, ToolError, ToolOutput, parse_args, truncate_output,
};

/// Arguments for [`MemoryTool`].
#[derive(Debug, Deserialize)]
pub struct MemoryArgs {
    /// One of `save`, `read`, `delete`.
    pub action: String,
    /// Kebab-case memory name (the file is `<name>.md`).
    pub name: String,
    /// One-line summary shown in the index (save only).
    #[serde(default)]
    pub description: Option<String>,
    /// The fact to remember (save only).
    #[serde(default)]
    pub content: Option<String>,
}

/// `memory` — save, read, or delete a persistent per-project memory.
pub struct MemoryTool;

impl MemoryTool {
    /// Require an argument that only some actions take, with a usage hint.
    fn require<'a>(
        &self,
        value: Option<&'a str>,
        field: &str,
        action: &str,
    ) -> Result<&'a str, ToolError> {
        value.ok_or_else(|| ToolError::InvalidArgs {
            tool: self.name().to_string(),
            message: format!("action '{action}' requires '{field}'"),
        })
    }
}

#[async_trait]
impl Tool for MemoryTool {
    fn name(&self) -> &str {
        "memory"
    }

    fn description(&self) -> &str {
        "Persist facts about this project across sessions. Use action 'save' to record a durable fact (user preferences, project conventions, decisions), 'read' to recall a saved memory's full content, and 'delete' to drop a stale one. Saved memories appear in the system prompt's memory index next session. Names are kebab-case; descriptions are one line."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["save", "read", "delete"], "description": "What to do with the memory" },
                "name": { "type": "string", "description": "Kebab-case memory name (lowercase letters, digits, hyphens)" },
                "description": { "type": "string", "description": "One-line summary shown in the memory index (required for save)" },
                "content": { "type": "string", "description": "The fact to remember, markdown allowed (required for save)" }
            },
            "required": ["action", "name"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: MemoryArgs = parse_args(self.name(), args)?;
        let store = MemoryStore::open(&ctx.cwd).map_err(|err| ToolError::Execution {
            tool: self.name().to_string(),
            source: err,
        })?;

        match args.action.as_str() {
            "save" => {
                let description =
                    self.require(args.description.as_deref(), "description", "save")?;
                let content = self.require(args.content.as_deref(), "content", "save")?;
                match store.save(&args.name, description, content) {
                    Ok(()) => Ok(ToolOutput::ok(format!(
                        "Saved memory '{}'. It will appear in the system prompt's memory index next session.",
                        args.name
                    ))),
                    Err(err) => Ok(ToolOutput::error(format!("{err:#}"))),
                }
            }
            "read" => match store.read(&args.name) {
                Ok(contents) => Ok(ToolOutput::ok(truncate_output(contents, MAX_OUTPUT_BYTES))),
                Err(err) => Ok(ToolOutput::error(format!("{err:#}"))),
            },
            "delete" => match store.delete(&args.name) {
                Ok(()) => Ok(ToolOutput::ok(format!("Deleted memory '{}'.", args.name))),
                Err(err) => Ok(ToolOutput::error(format!("{err:#}"))),
            },
            other => Err(ToolError::InvalidArgs {
                tool: self.name().to_string(),
                message: format!("unknown action '{other}' (save|read|delete)"),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// Temp project dir removed on drop, along with the memory store dir
    /// derived from it under `~/.wizard/memory/`.
    struct TempProject(PathBuf);

    impl TempProject {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("wizard-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }

        fn ctx(&self) -> ToolContext {
            ToolContext::new(&self.0)
        }

        fn store(&self) -> MemoryStore {
            MemoryStore::open(&self.0).expect("open store")
        }
    }

    impl Drop for TempProject {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.store().dir());
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn save_read_delete_dispatch() {
        let tmp = TempProject::new();

        let out = MemoryTool
            .execute(
                json!({
                    "action": "save",
                    "name": "test-style",
                    "description": "tests use TempDir",
                    "content": "Unit tests build their own temp dirs."
                }),
                &tmp.ctx(),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("Saved memory 'test-style'"));
        assert_eq!(tmp.store().list().unwrap().len(), 1);

        let out = MemoryTool
            .execute(
                json!({ "action": "read", "name": "test-style" }),
                &tmp.ctx(),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("description: tests use TempDir"));
        assert!(
            out.content
                .contains("Unit tests build their own temp dirs.")
        );

        let out = MemoryTool
            .execute(
                json!({ "action": "delete", "name": "test-style" }),
                &tmp.ctx(),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(tmp.store().list().unwrap().is_empty());
    }

    #[tokio::test]
    async fn read_missing_memory_is_a_tool_output_error() {
        let tmp = TempProject::new();
        let out = MemoryTool
            .execute(json!({ "action": "read", "name": "absent" }), &tmp.ctx())
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("no memory named 'absent'"));
    }

    #[tokio::test]
    async fn save_requires_description_and_content() {
        let tmp = TempProject::new();
        let err = MemoryTool
            .execute(
                json!({ "action": "save", "name": "incomplete" }),
                &tmp.ctx(),
            )
            .await
            .expect_err("save without description must be rejected");
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn unknown_action_is_invalid_args() {
        let tmp = TempProject::new();
        let err = MemoryTool
            .execute(json!({ "action": "forget", "name": "x" }), &tmp.ctx())
            .await
            .expect_err("unknown action must be rejected");
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn traversal_name_is_rejected_without_touching_disk() {
        let tmp = TempProject::new();
        let out = MemoryTool
            .execute(
                json!({
                    "action": "save",
                    "name": "../evil",
                    "description": "d",
                    "content": "c"
                }),
                &tmp.ctx(),
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("kebab-case"));
    }
}
