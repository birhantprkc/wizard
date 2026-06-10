//! `publish` tool: fork Wizard to the user's GitHub and emit a one-line
//! installer for their variant.
//!
//! Mirrors [`crate::tools::evolve::EvolveTool`] in structure. Requires `gh`
//! authenticated; opts into `requires_approval` so a y/n prompt fires when
//! `auto_approve = false`; both modes auto-approve by default.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Tool, ToolContext, ToolError, ToolOutput, parse_args};
use crate::config::Config;
use crate::evolve::{PublishRequest, publish};

/// `publish` — fork Wizard to the user's GitHub and emit a one-line
/// installer for their variant.
pub struct PublishTool {
    config: Config,
}

impl PublishTool {
    pub fn new(config: Config) -> Self {
        Self { config }
    }
}

/// Arguments for [`PublishTool`].
#[derive(Debug, Deserialize)]
pub struct PublishArgs {
    /// Branch to push to on the fork. Defaults to `"main"` when omitted.
    #[serde(default)]
    pub branch: Option<String>,
}

#[async_trait]
impl Tool for PublishTool {
    fn name(&self) -> &str {
        "publish"
    }

    fn description(&self) -> &str {
        "Fork Wizard to your own GitHub account and get a one-line installer \
         for your personalised variant. Use this after a deep evolve (or any \
         time you want to distribute the version of Wizard running on this \
         machine). The fork is created under your authenticated GitHub account \
         via `gh`; the source checkout at ~/.wizard/src is pushed to the fork \
         and a `curl | bash` one-liner is returned that anyone can run to \
         install your variant (building from source). Requires `gh auth login`."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "branch": {
                    "type": "string",
                    "description": "Branch to push to on the fork. Defaults to \"main\"."
                }
            },
            "required": []
        })
    }

    fn requires_approval(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: PublishArgs = parse_args(self.name(), args)?;

        let req = PublishRequest {
            branch: args.branch,
            // The tool-layer approval gate already governs whether
            // execute() is reached, so the pipeline itself need not re-prompt.
            auto_approve: true,
        };

        match publish(&self.config, req, false).await {
            Ok(outcome) => {
                let summary = format!(
                    "Published to {}  (branch: {}){}\n\nInstall one-liner:\n{}",
                    outcome.fork_url,
                    outcome.branch,
                    outcome
                        .commit
                        .as_deref()
                        .map(|sha| format!("  commit: {sha}"))
                        .unwrap_or_default(),
                    outcome.install_one_liner,
                );
                Ok(ToolOutput::ok(summary))
            }
            Err(err) => Ok(ToolOutput::error(format!("publish failed: {err:#}"))),
        }
    }
}
