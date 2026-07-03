//! `publish` tool: fork Wizard to the user's GitHub and emit a one-line
//! installer for their variant.
//!
//! Mirrors [`crate::tools::evolve::EvolveTool`] in structure. Requires `gh`
//! authenticated (`gh auth login`).

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

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: PublishArgs = parse_args(self.name(), args)?;

        let req = PublishRequest {
            branch: args.branch,
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn args_default_branch_to_none() {
        let args: PublishArgs = parse_args("publish", json!({})).unwrap();
        assert!(args.branch.is_none());

        // Zero-parameter calls may pass null instead of an empty object.
        let args: PublishArgs = parse_args("publish", Value::Null).unwrap();
        assert!(args.branch.is_none());
    }

    #[test]
    fn args_accept_a_branch() {
        let args: PublishArgs = parse_args("publish", json!({ "branch": "dev" })).unwrap();
        assert_eq!(args.branch.as_deref(), Some("dev"));
    }

    #[test]
    fn args_reject_a_non_string_branch() {
        let err = parse_args::<PublishArgs>("publish", json!({ "branch": 5 }))
            .expect_err("branch must be a string");
        assert!(matches!(err, ToolError::InvalidArgs { tool, .. } if tool == "publish"));
    }

    #[test]
    fn tool_name_and_schema_shape() {
        let tool = PublishTool::new(Config::default());
        assert_eq!(tool.name(), "publish");
        let params = tool.parameters();
        assert_eq!(params["type"], "object");
        assert_eq!(params["properties"]["branch"]["type"], "string");
    }
}
