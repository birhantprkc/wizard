//! `evolve` tool: lets the agent extend or rebuild ITSELF at runtime.
//! Wraps the tiered self-extension pipeline (`crate::evolve`). A successful
//! deep rebuild drops a re-exec marker so the continuous loop relaunches
//! into the new binary.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Tool, ToolContext, ToolError, ToolOutput, parse_args};
use crate::config::Config;
use crate::evolve::{EvolveOutcome, EvolveRequest, EvolveTier, Evolver};

/// `evolve` — add a new capability to Wizard itself, or deep-rebuild its
/// binary.
pub struct EvolveTool {
    config: Config,
}

impl EvolveTool {
    pub fn new(config: Config) -> Self {
        Self { config }
    }
}

/// Arguments for [`EvolveTool`].
#[derive(Debug, Deserialize)]
pub struct EvolveArgs {
    /// Precise natural-language spec of the capability to add.
    pub description: String,
    /// When true, change Wizard's own Rust source and rebuild the binary
    /// (Tier 2). Defaults to a fast runtime extension (Tier 1).
    #[serde(default)]
    pub deep: bool,
}

#[async_trait]
impl Tool for EvolveTool {
    fn name(&self) -> &str {
        "evolve"
    }

    fn description(&self) -> &str {
        "Add a NEW capability to yourself when the current task needs one you \
         lack. By default (deep=false) this performs a fast runtime extension — \
         it adds a skill, MCP server, scripted tool (LuaJIT by default), or \
         subagent under ~/.wizard/ with no recompile. Scripted tools run through \
         the embedded LuaJIT just-in-time compiler unless an external interpreter \
         is required. Set deep=true ONLY when the capability genuinely requires \
         changing Wizard's own Rust source: this rebuilds and replaces the \
         running binary, is much slower, and is gated by a build plus smoke test \
         (falling back to a runtime extension if no toolchain or source is \
         available). The `description` argument is a precise natural-language \
         specification of the capability you want."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "description": {
                    "type": "string",
                    "description": "Precise natural-language spec of the capability to add to yourself."
                },
                "deep": {
                    "type": "boolean",
                    "default": false,
                    "description": "Change Wizard's own Rust source and rebuild the binary (slow). Default false uses a fast runtime extension."
                }
            },
            "required": ["description"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: EvolveArgs = parse_args(self.name(), args)?;

        let request = EvolveRequest {
            description: args.description,
            tier: if args.deep {
                EvolveTier::Deep
            } else {
                EvolveTier::Runtime
            },
        };

        let outcome = match Evolver::new(self.config.clone()).run(request).await {
            Ok(outcome) => outcome,
            Err(err) => return Ok(ToolOutput::error(format!("evolve failed: {err:#}"))),
        };

        let summary = match outcome {
            EvolveOutcome::DeepRebuilt { binary } => {
                let marker_note = write_marker(ctx, "evolve-reexec");
                format!(
                    "Deep evolve rebuilt Wizard's binary at {}. {marker_note}",
                    binary.display()
                )
            }
            EvolveOutcome::SkillAdded { name, path } => {
                let marker_note = write_marker(ctx, "evolve-reload");
                format!("Added skill '{name}' at {}. {marker_note}", path.display())
            }
            EvolveOutcome::McpServerRegistered { name } => {
                let marker_note = write_marker(ctx, "evolve-reload");
                format!("Registered MCP server '{name}'. {marker_note}")
            }
            EvolveOutcome::ScriptedToolAdded { name, path } => {
                let marker_note = write_marker(ctx, "evolve-reload");
                format!(
                    "Added scripted tool '{name}' at {}. {marker_note}",
                    path.display()
                )
            }
            EvolveOutcome::SubagentAdded { name } => {
                let marker_note = write_marker(ctx, "evolve-reload");
                format!("Added subagent '{name}'. {marker_note}")
            }
            EvolveOutcome::FellBackToRuntime { reason, outcome } => {
                let marker_note = write_marker(ctx, "evolve-reload");
                format!(
                    "Deep evolve fell back to a runtime extension ({reason}): {}. {marker_note}",
                    describe_outcome(&outcome)
                )
            }
            EvolveOutcome::Denied => {
                return Ok(ToolOutput::error("evolve was denied"));
            }
        };

        Ok(ToolOutput::ok(summary))
    }
}

/// Drop an empty marker file under `<cwd>/.wizard/` so the supervising loop
/// knows to react (relaunch on `evolve-reexec`, hot-reload the registry on
/// `evolve-reload`). Returns a note describing success or failure rather than
/// propagating the error.
fn write_marker(ctx: &ToolContext, name: &str) -> String {
    let dir = ctx.cwd.join(".wizard");
    if let Err(err) = std::fs::create_dir_all(&dir) {
        return format!("(could not create {} marker: {err})", dir.display());
    }
    let marker = dir.join(name);
    match std::fs::write(&marker, b"") {
        Ok(()) => format!("Wrote {} marker for the loop.", marker.display()),
        Err(err) => format!("(could not write {} marker: {err})", marker.display()),
    }
}

/// One-line description of a nested Tier-1 outcome (used for fallbacks).
fn describe_outcome(outcome: &EvolveOutcome) -> String {
    match outcome {
        EvolveOutcome::SkillAdded { name, .. } => format!("added skill '{name}'"),
        EvolveOutcome::McpServerRegistered { name } => format!("registered MCP server '{name}'"),
        EvolveOutcome::ScriptedToolAdded { name, .. } => format!("added scripted tool '{name}'"),
        EvolveOutcome::SubagentAdded { name } => format!("added subagent '{name}'"),
        EvolveOutcome::DeepRebuilt { binary } => format!("rebuilt binary at {}", binary.display()),
        EvolveOutcome::FellBackToRuntime { reason, outcome } => {
            format!("fell back ({reason}): {}", describe_outcome(outcome))
        }
        EvolveOutcome::Denied => "denied".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

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

    #[test]
    fn args_require_a_description() {
        let err =
            parse_args::<EvolveArgs>("evolve", json!({})).expect_err("description is required");
        assert!(matches!(err, ToolError::InvalidArgs { tool, .. } if tool == "evolve"));
    }

    #[test]
    fn args_default_deep_to_false() {
        let args: EvolveArgs = parse_args("evolve", json!({ "description": "add x" })).unwrap();
        assert_eq!(args.description, "add x");
        assert!(!args.deep);

        let args: EvolveArgs =
            parse_args("evolve", json!({ "description": "add x", "deep": true })).unwrap();
        assert!(args.deep);
    }

    #[test]
    fn write_marker_creates_the_file_and_reports_it() {
        let tmp = TempDir::new();
        let note = write_marker(&tmp.ctx(), "evolve-reload");
        assert!(tmp.0.join(".wizard").join("evolve-reload").exists());
        assert!(note.starts_with("Wrote "), "{note}");
        assert!(note.contains("evolve-reload"), "{note}");
    }

    #[test]
    fn write_marker_reports_failure_instead_of_erroring() {
        let tmp = TempDir::new();
        // A file where the .wizard directory should go makes create_dir_all fail.
        std::fs::write(tmp.0.join(".wizard"), b"not a dir").unwrap();
        let note = write_marker(&tmp.ctx(), "evolve-reexec");
        assert!(note.starts_with("(could not create"), "{note}");
    }

    #[test]
    fn describe_outcome_covers_every_variant() {
        assert_eq!(
            describe_outcome(&EvolveOutcome::SkillAdded {
                name: "s".to_string(),
                path: PathBuf::from("/p"),
            }),
            "added skill 's'"
        );
        assert_eq!(
            describe_outcome(&EvolveOutcome::McpServerRegistered {
                name: "m".to_string(),
            }),
            "registered MCP server 'm'"
        );
        assert_eq!(
            describe_outcome(&EvolveOutcome::ScriptedToolAdded {
                name: "t".to_string(),
                path: PathBuf::from("/p"),
            }),
            "added scripted tool 't'"
        );
        assert_eq!(
            describe_outcome(&EvolveOutcome::SubagentAdded {
                name: "a".to_string(),
            }),
            "added subagent 'a'"
        );
        assert_eq!(
            describe_outcome(&EvolveOutcome::DeepRebuilt {
                binary: PathBuf::from("/bin/wizard"),
            }),
            "rebuilt binary at /bin/wizard"
        );
        assert_eq!(describe_outcome(&EvolveOutcome::Denied), "denied");
        assert_eq!(
            describe_outcome(&EvolveOutcome::FellBackToRuntime {
                reason: "no toolchain".to_string(),
                outcome: Box::new(EvolveOutcome::SubagentAdded {
                    name: "a".to_string(),
                }),
            }),
            "fell back (no toolchain): added subagent 'a'"
        );
    }
}
