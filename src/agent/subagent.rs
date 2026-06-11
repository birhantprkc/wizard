//! Subagents: isolated sub-contexts for parallel or decomposed work.
//!
//! Each subagent gets its own message history, step budget, and tool scope.
//! The result returns to the parent as a single tool result, so a multi-step
//! sub-task costs the parent one turn of context.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::{Config, Mode};
use crate::hooks::{HookEngine, PreToolUse};
use crate::llm::provider::LlmProvider;
use crate::llm::{ChatMessage, ChatOptions, ChatRequest, Role};
use crate::tools::{Tool, ToolContext, ToolError, ToolOutput, registry::ToolRegistry};

use super::prompts;
use super::{normalize_args, parse_json_tool_call};

/// A named, reusable subagent definition. Built-in defaults exist
/// (a general-purpose worker); `/evolve` can add more as TOML files under
/// `~/.wizard/subagents/`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentConfig {
    /// Unique name the parent refers to (e.g. `"reviewer"`).
    pub name: String,
    /// What this subagent is for (shown to the parent model).
    pub description: String,
    /// System prompt for the isolated context.
    pub system_prompt: String,
    /// Tool names this subagent may call. `None` = the parent's full set.
    #[serde(default)]
    pub tool_scope: Option<Vec<String>>,
    /// Step budget for the sub-loop.
    #[serde(default = "SubagentConfig::default_max_steps")]
    pub max_steps: u32,
}

impl SubagentConfig {
    fn default_max_steps() -> u32 {
        15
    }
}

/// Outcome of a subagent run, summarized for the parent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentResult {
    pub name: String,
    /// The subagent's final answer text.
    pub output: String,
    pub steps_used: u32,
    /// False when the sub-loop hit its step budget or errored out.
    pub completed: bool,
}

/// Built-in subagent definitions available on every install.
pub fn builtin_configs() -> Vec<SubagentConfig> {
    vec![SubagentConfig {
        name: "worker".to_string(),
        description: "General-purpose worker for self-contained sub-tasks: \
                      investigate, edit, run commands, and report back."
            .to_string(),
        system_prompt: "You are a focused subagent of Wizard, a local agent. Complete \
                        the given sub-task end-to-end using the provided tools, then reply \
                        with a concise final report of what you found or changed. Do not ask \
                        questions; make reasonable decisions and note them in your report."
            .to_string(),
        tool_scope: None,
        max_steps: SubagentConfig::default_max_steps(),
    }]
}

/// Load `/evolve`-authored subagent definitions (`*.toml`) from `dir`.
/// Missing directory yields an empty vec.
pub fn load_dir(dir: &Path) -> Result<Vec<SubagentConfig>> {
    let mut configs = Vec::new();
    if !dir.is_dir() {
        return Ok(configs);
    }
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    paths.sort();
    for path in paths {
        let parsed = std::fs::read_to_string(&path)
            .map_err(anyhow::Error::from)
            .and_then(|raw| toml::from_str::<SubagentConfig>(&raw).map_err(anyhow::Error::from));
        match parsed {
            Ok(config) => configs.push(config),
            Err(err) => {
                tracing::warn!("skipping subagent manifest {}: {err}", path.display());
            }
        }
    }
    Ok(configs)
}

/// Built-in subagents plus any user-defined ones from `dir`; user
/// definitions shadow built-ins by name.
pub fn available_configs(dir: &Path) -> Vec<SubagentConfig> {
    let mut configs = builtin_configs();
    let user = load_dir(dir).unwrap_or_else(|err| {
        tracing::warn!("loading subagents from {} failed: {err}", dir.display());
        Vec::new()
    });
    for config in user {
        configs.retain(|existing| existing.name != config.name);
        configs.push(config);
    }
    configs
}

/// Build a registry containing the tools of `parent` named in `scope`
/// (`None` = all of them). Unknown names are skipped with a warning.
pub fn scoped_registry(parent: &ToolRegistry, scope: Option<&[String]>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    match scope {
        None => {
            for spec in parent.specs() {
                if let Some(tool) = parent.get(&spec.function.name) {
                    registry.register(Arc::clone(tool));
                }
            }
        }
        Some(names) => {
            for name in names {
                match parent.get(name) {
                    Some(tool) => registry.register(Arc::clone(tool)),
                    None => tracing::warn!("subagent tool scope names unknown tool '{name}'"),
                }
            }
        }
    }
    registry
}

/// Run `task` in an isolated context defined by `config`: fresh history,
/// scoped registry, own step budget. The parent's lifecycle `hooks` apply to
/// the subagent's tool calls too (their activity is not surfaced as events —
/// the subagent reports back as one tool result).
pub async fn spawn(
    config: &SubagentConfig,
    task: &str,
    client: &Arc<dyn LlmProvider>,
    registry: &ToolRegistry,
    hooks: &HookEngine,
    ctx: &ToolContext,
) -> Result<SubagentResult> {
    let model = Config::load()
        .map(|c| c.active().model)
        .unwrap_or_else(|_| Config::default().active().model);
    let scoped = scoped_registry(registry, config.tool_scope.as_deref());
    let native_tools = match client.supports_native_tools(&model).await {
        Ok(supported) => supported,
        Err(err) => {
            tracing::warn!("tool-support probe failed for '{model}': {err}; assuming native");
            true
        }
    };

    let mut system_prompt = config.system_prompt.clone();
    if !native_tools {
        system_prompt.push_str("\n\n");
        system_prompt.push_str(&prompts::render_tool_protocol(&scoped.specs()));
    }

    let mut history = vec![
        ChatMessage::system(system_prompt),
        ChatMessage::user(task.to_string()),
    ];

    // Subagents share the parent's cwd and task registry but get their own
    // todo list (their working notes must not clobber the parent's) and no
    // event channel — their activity surfaces as one tool result.
    let ctx = ToolContext {
        todos: Arc::new(std::sync::Mutex::new(crate::tools::todo::TodoList::new())),
        events: None,
        ..ctx.clone()
    };

    let mut steps_used = 0;
    let mut completed = false;
    let mut last_text = String::new();
    let max_steps = config.max_steps.max(1);

    for step in 1..=max_steps {
        steps_used = step;
        let request = ChatRequest {
            model: model.clone(),
            messages: history.clone(),
            tools: if native_tools {
                scoped.specs()
            } else {
                Vec::new()
            },
            stream: true,
            options: Some(ChatOptions {
                temperature: Some(Mode::Sovereign.temperature()),
                num_ctx: None,
            }),
        };

        let mut stream = client
            .chat_stream(request)
            .await
            .with_context(|| format!("subagent '{}' chat request failed", config.name))?;

        let mut content = String::new();
        let mut tool_calls = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.with_context(|| format!("subagent '{}' stream failed", config.name))?;
            if let Some(message) = chunk.message {
                content.push_str(&message.content);
                tool_calls.extend(message.tool_calls);
            }
            if chunk.done {
                break;
            }
        }

        history.push(ChatMessage {
            role: Role::Assistant,
            content: content.clone(),
            tool_calls: tool_calls.clone(),
            tool_name: None,
        });

        if !native_tools
            && tool_calls.is_empty()
            && let Some(call) = parse_json_tool_call(&content)
        {
            tool_calls.push(call);
        }

        if tool_calls.is_empty() {
            last_text = content;
            completed = true;
            break;
        }
        if !content.trim().is_empty() {
            last_text = content;
        }

        for call in tool_calls {
            let name = call.function.name.clone();
            let mut args = normalize_args(&call.function.arguments);
            // Same hook pipeline as the parent's dispatcher: pre-hooks may
            // rewrite the arguments or veto, post-hooks may append context.
            let output = match hooks
                .pre_tool_use(&name, &args, Mode::Sovereign, None)
                .await
            {
                PreToolUse::Block(reason) => {
                    ToolOutput::error(format!("blocked by pre_tool_use hook: {reason}"))
                }
                PreToolUse::Continue(updated) => {
                    if let Some(updated) = updated {
                        args = updated;
                    }
                    // Same checkpoint seam as the parent's dispatcher: the
                    // subagent's edits are snapshotted under the parent's
                    // current turn (the context carries the parent's store).
                    crate::checkpoint::snapshot_edit_target(&scoped, &name, &args, &ctx);
                    let mut output = match scoped.execute(&name, args.clone(), &ctx).await {
                        Ok(output) => output,
                        Err(err) => ToolOutput::error(err.to_string()),
                    };
                    if let Some(extra) = hooks
                        .post_tool_use(&name, &args, Mode::Sovereign, None)
                        .await
                    {
                        crate::hooks::append_context(&mut output.content, &extra);
                    }
                    output
                }
            };
            let body = if output.is_error {
                format!("Error: {}", output.content)
            } else {
                output.content
            };
            history.push(if native_tools {
                ChatMessage::tool_result(name, body)
            } else {
                ChatMessage::user(format!("Tool result for `{name}`:\n{body}"))
            });
        }
    }

    Ok(SubagentResult {
        name: config.name.clone(),
        output: if last_text.trim().is_empty() {
            "(subagent produced no final text)".to_string()
        } else {
            last_text
        },
        steps_used,
        completed,
    })
}

/// `spawn_subagent` — the tool the parent model calls to fan out work.
pub struct SpawnSubagentTool {
    /// Available subagent definitions, by name.
    pub configs: Vec<SubagentConfig>,
    /// Model client shared with the parent loop.
    client: Arc<dyn LlmProvider>,
    /// Parent tool set subagents scope down from. Built without the spawn
    /// tool itself, so subagents cannot recurse.
    registry: Arc<ToolRegistry>,
    /// The parent's lifecycle hooks, applied to subagent tool calls too.
    hooks: Arc<HookEngine>,
    /// Tool description, including the roster of available subagents.
    description: String,
}

impl SpawnSubagentTool {
    pub fn new(
        configs: Vec<SubagentConfig>,
        client: Arc<dyn LlmProvider>,
        registry: Arc<ToolRegistry>,
        hooks: Arc<HookEngine>,
    ) -> Self {
        let roster = configs
            .iter()
            .map(|c| format!("`{}` ({})", c.name, c.description))
            .collect::<Vec<_>>()
            .join("; ");
        let description = format!(
            "Delegate a self-contained sub-task to an isolated subagent with its own context \
             and step budget. Returns the subagent's final answer. Available subagents: {roster}"
        );
        Self {
            configs,
            client,
            registry,
            hooks,
            description,
        }
    }
}

#[async_trait]
impl Tool for SpawnSubagentTool {
    fn name(&self) -> &str {
        "spawn_subagent"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "subagent": { "type": "string", "description": "Name of the subagent to use" },
                "task": { "type": "string", "description": "Self-contained task description with all needed context" }
            },
            "required": ["subagent", "task"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        #[derive(Deserialize)]
        struct Args {
            subagent: String,
            task: String,
        }
        let args: Args = serde_json::from_value(args).map_err(|err| ToolError::InvalidArgs {
            tool: "spawn_subagent".to_string(),
            message: err.to_string(),
        })?;

        let config = self
            .configs
            .iter()
            .find(|c| c.name == args.subagent)
            .ok_or_else(|| ToolError::InvalidArgs {
                tool: "spawn_subagent".to_string(),
                message: format!(
                    "unknown subagent '{}'; available: {}",
                    args.subagent,
                    self.configs
                        .iter()
                        .map(|c| c.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            })?;

        let result = spawn(
            config,
            &args.task,
            &self.client,
            &self.registry,
            &self.hooks,
            ctx,
        )
        .await
        .map_err(|err| ToolError::Execution {
            tool: "spawn_subagent".to_string(),
            source: err,
        })?;

        let summary = format!(
            "Subagent '{}' {} after {} step(s).\n\n{}",
            result.name,
            if result.completed {
                "completed"
            } else {
                "hit its step budget"
            },
            result.steps_used,
            result.output
        );
        Ok(if result.completed {
            ToolOutput::ok(summary)
        } else {
            ToolOutput::error(summary)
        })
    }
}
