//! Subagents: isolated sub-contexts for parallel or decomposed work.
//!
//! Each subagent gets its own message history, step budget, and tool scope.
//! The result returns to the parent as a single tool result, so a multi-step
//! sub-task costs the parent one turn of context.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::{Config, Mode};
use crate::hooks::{HookEngine, PreToolUse};
use crate::llm::provider::LlmProvider;
use crate::llm::{ChatMessage, ChatOptions, ChatRequest, Role, ToolCall};
use crate::tools::subagent_tasks::SubagentRunResult;
use crate::tools::{Tool, ToolAccess, ToolContext, ToolError, ToolOutput, registry::ToolRegistry};

use super::prompts;
use super::{error_is_transient, normalize_args, parse_json_tool_call};

/// Advertised name of the spawn tool, referenced by the dispatcher's
/// plan-mode gate.
pub const SPAWN_SUBAGENT_TOOL_NAME: &str = "spawn_subagent";

/// Retry budget for one subagent model call (mirrors the parent loop's
/// non-continuous budget).
const RETRY_ATTEMPTS: u32 = 6;

/// The parent agent's active model, shared with [`SpawnSubagentTool`] so
/// mid-session `/model` switches reach subagents. `None` falls back to the
/// configured model.
pub type SharedActiveModel = Arc<std::sync::RwLock<Option<String>>>;

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

/// Built-in subagents plus any user-defined ones from `dir`, plus the active
/// harness bundle's `subagents/` (if any); later sources shadow earlier ones
/// by name, so bundle definitions win over user definitions win over
/// built-ins.
pub fn available_configs(dir: &Path) -> Vec<SubagentConfig> {
    let mut configs = builtin_configs();
    let mut merge_from = |dir: &Path| {
        let loaded = load_dir(dir).unwrap_or_else(|err| {
            tracing::warn!("loading subagents from {} failed: {err}", dir.display());
            Vec::new()
        });
        for config in loaded {
            configs.retain(|existing| existing.name != config.name);
            configs.push(config);
        }
    };
    merge_from(dir);
    if let Some(harness) = crate::config::Config::harness_dir() {
        let bundle = harness.join("subagents");
        if bundle.is_dir() {
            merge_from(&bundle);
        }
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

/// Keep only the read-only tools of `parent` (plan-mode delegation: the
/// subagent may explore but not act).
pub fn read_only_registry(parent: &ToolRegistry) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    for spec in parent.specs() {
        if let Some(tool) = parent.get(&spec.function.name)
            && tool.access() == ToolAccess::ReadOnly
        {
            registry.register(Arc::clone(tool));
        }
    }
    registry
}

/// Per-run overrides for [`spawn`].
#[derive(Debug, Clone, Default)]
pub struct SpawnOptions {
    /// Model to run the subagent on; `None` falls back to the configured
    /// active model. The parent passes its live model so `/model` switches
    /// apply.
    pub model: Option<String>,
    /// Restrict the subagent to read-only tools (plan mode).
    pub read_only: bool,
}

/// One model round-trip: stream a completion, skipping reasoning
/// ("thinking") chunks so they never leak into subagent history or reports.
async fn stream_step(
    client: &Arc<dyn LlmProvider>,
    request: ChatRequest,
) -> Result<(String, Vec<ToolCall>)> {
    let mut stream = client.chat_stream(request).await?;
    let mut content = String::new();
    let mut tool_calls = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if let Some(message) = chunk.message {
            if !chunk.thinking {
                content.push_str(&message.content);
            }
            tool_calls.extend(message.tool_calls);
        }
        if chunk.done {
            break;
        }
    }
    Ok((content, tool_calls))
}

/// Run `task` in an isolated context defined by `config`: fresh history,
/// scoped registry, own step budget. The parent's lifecycle `hooks` apply to
/// the subagent's tool calls too (their activity is not surfaced as events —
/// the subagent reports back as one tool result).
pub async fn spawn(
    config: &SubagentConfig,
    task: &str,
    options: &SpawnOptions,
    client: &Arc<dyn LlmProvider>,
    registry: &ToolRegistry,
    hooks: &HookEngine,
    ctx: &ToolContext,
) -> Result<SubagentResult> {
    let loaded = Config::load().unwrap_or_default();
    let model = options
        .model
        .clone()
        .unwrap_or_else(|| loaded.active().model);
    let (retry_base, retry_max) = (loaded.retry_base_secs, loaded.retry_max_secs);
    let mut scoped = scoped_registry(registry, config.tool_scope.as_deref());
    if options.read_only {
        scoped = read_only_registry(&scoped);
    }
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

    // The subagent reports back to the model as one tool result, but its
    // individual tool calls are surfaced to the UI (prefixed with the
    // subagent's name) on the parent's event channel so the user can watch
    // what it's doing. Nested tools run with `events: None` so they don't
    // double-emit (todos, background tasks); we emit our own start/finish pair.
    let progress = ctx.events.clone();
    let ctx = ToolContext {
        todos: Arc::new(std::sync::Mutex::new(crate::tools::todo::TodoList::new())),
        events: None,
        // A subagent has no surface to drive; it must never dispatch the
        // parent's slash commands even if the parent's ctx enabled it.
        dispatches_commands: false,
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
                reasoning_effort: loaded
                    .reasoning_effort
                    .map(|effort| effort.as_str().to_string()),
            }),
        };

        // Same retry policy as the parent loop: transient provider failures
        // (transport drops, 429/5xx) back off and retry instead of killing a
        // deep run; permanent errors (auth, bad request) fail immediately.
        let mut attempt: u32 = 0;
        let (content, mut tool_calls) = loop {
            match stream_step(client, request.clone()).await {
                Ok(completion) => break completion,
                Err(err) => {
                    if !error_is_transient(&err) || attempt >= RETRY_ATTEMPTS {
                        return Err(err)
                            .with_context(|| format!("subagent '{}' chat failed", config.name));
                    }
                    let secs =
                        retry_max.min(retry_base.saturating_mul(2u64.saturating_pow(attempt)));
                    tracing::warn!(
                        "subagent '{}': LLM unavailable ({err:#}); retrying in {secs}s",
                        config.name
                    );
                    tokio::time::sleep(Duration::from_secs(secs)).await;
                    attempt += 1;
                }
            }
        };

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
            // Surface this call to the UI as `<subagent> ▸ <tool>` so the user
            // sees what the subagent is doing while it runs.
            let label = format!("{} ▸ {}", config.name, name);
            if let Some(events) = &progress {
                super::emit(
                    events,
                    crate::agent::AgentEvent::ToolStarted {
                        name: label.clone(),
                        args: args.clone(),
                    },
                )
                .await;
            }
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
                        .post_tool_use_with_output(
                            &name,
                            &args,
                            &output.content,
                            output.is_error,
                            Mode::Sovereign,
                            None,
                        )
                        .await
                    {
                        crate::hooks::append_context(&mut output.content, &extra);
                    }
                    output
                }
            };
            if let Some(events) = &progress {
                super::emit(
                    events,
                    crate::agent::AgentEvent::ToolFinished {
                        name: label.clone(),
                        output: output.clone(),
                    },
                )
                .await;
            }
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
    /// The parent's active model (bound via [`Self::model_handle`] +
    /// `Agent::bind_subagent_model`); `None` reads the configured model.
    model: SharedActiveModel,
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
            .map(|c| {
                let scope = match &c.tool_scope {
                    None => "all tools".to_string(),
                    Some(names) => names.join(", "),
                };
                format!(
                    "\n  - `{}` — {} (tools: {}; up to {} steps)",
                    c.name, c.description, scope, c.max_steps
                )
            })
            .collect::<String>();
        let description = format!(
            "Delegate a self-contained sub-task to an isolated subagent. It runs its own loop \
             with a fresh context and scoped tools, then returns one final report — its \
             intermediate steps never enter your context, so a multi-step sub-task costs you a \
             single turn.\n\n\
             Delegate almost always for anything beyond a quick one-off, and set \
             `background: true` when you do — it returns immediately instead of making the \
             user wait, so they can keep talking to you while the subagent runs. Its progress \
             streams in as it works and its report lands in your context automatically once \
             it's done. Only omit `background` (synchronous) when you need the report to keep \
             working within this same turn.\n\n\
             Delegating also pays off when the work would otherwise flood your context with \
             output you don't need to keep (large greps, reading many files, long logs), or \
             when a specialist fits better than you do. Don't delegate trivial one-tool \
             actions, work that needs the user mid-flight (the subagent can't ask questions), \
             or a task you can't yet describe in full.\n\n\
             `task` is the ONLY context the subagent gets besides its own prompt — make it \
             self-contained: state the goal, the relevant paths/context, any constraints, and \
             exactly what to report back. You can't steer it once it's running, so prefer one \
             well-scoped task over a chain of follow-ups.\n\n\
             Available subagents:{roster}"
        );
        Self {
            configs,
            client,
            registry,
            hooks,
            description,
            model: Arc::new(std::sync::RwLock::new(None)),
        }
    }

    /// Handle the parent agent binds (see `Agent::bind_subagent_model`) so
    /// mid-session `/model` switches reach subagent runs. Unbound, runs fall
    /// back to the configured active model.
    pub fn model_handle(&self) -> SharedActiveModel {
        Arc::clone(&self.model)
    }

    fn active_model(&self) -> Option<String> {
        self.model.read().ok().and_then(|model| model.clone())
    }
}

#[async_trait]
impl Tool for SpawnSubagentTool {
    fn name(&self) -> &str {
        SPAWN_SUBAGENT_TOOL_NAME
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "subagent": { "type": "string", "description": "Name of the subagent to use" },
                "task": { "type": "string", "description": "Self-contained task description with all needed context" },
                "background": {
                    "type": "boolean",
                    "description": "Run detached and return immediately instead of waiting for \
                        the report. Default false. Set true for self-contained, non-blocking \
                        delegation — the common case — so the user isn't stuck waiting on you."
                }
            },
            "required": ["subagent", "task"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        #[derive(Deserialize)]
        struct Args {
            subagent: String,
            task: String,
            #[serde(default)]
            background: bool,
            /// Injected by the dispatcher while plan mode is on (not
            /// advertised in the schema): the subagent runs read-only.
            #[serde(default)]
            plan_mode: bool,
        }
        let args: Args = serde_json::from_value(args).map_err(|err| ToolError::InvalidArgs {
            tool: SPAWN_SUBAGENT_TOOL_NAME.to_string(),
            message: err.to_string(),
        })?;
        let options = SpawnOptions {
            model: self.active_model(),
            read_only: args.plan_mode,
        };

        let config = self
            .configs
            .iter()
            .find(|c| c.name == args.subagent)
            .ok_or_else(|| ToolError::InvalidArgs {
                tool: SPAWN_SUBAGENT_TOOL_NAME.to_string(),
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

        if args.background {
            let name = config.name.clone();
            let config = config.clone();
            let task = args.task.clone();
            let client = Arc::clone(&self.client);
            let registry = Arc::clone(&self.registry);
            let hooks = Arc::clone(&self.hooks);
            let fut_ctx = ctx.clone();
            let fut_options = options.clone();
            let fut = async move {
                match spawn(
                    &config,
                    &task,
                    &fut_options,
                    &client,
                    &registry,
                    &hooks,
                    &fut_ctx,
                )
                .await
                {
                    Ok(result) => SubagentRunResult {
                        completed: result.completed,
                        output: result.output,
                        steps_used: result.steps_used,
                        error: None,
                    },
                    Err(err) => SubagentRunResult {
                        completed: false,
                        output: format!("subagent failed: {err:#}"),
                        steps_used: 0,
                        error: Some(format!("{err:#}")),
                    },
                }
            };
            let id = ctx.subagents.spawn(&name, &args.task, fut);
            if let Some(events) = &ctx.events {
                super::emit(
                    events,
                    crate::agent::AgentEvent::SubagentStarted {
                        id,
                        name: name.clone(),
                        task: args.task.clone(),
                    },
                )
                .await;
            }
            return Ok(ToolOutput::ok(format!(
                "Delegated to subagent '{name}' (#{id}): {}.\nRunning in the background — \
                 you'll see its progress as it works, and the report lands in your context \
                 once it's done.",
                args.task
            )));
        }

        let result = spawn(
            config,
            &args.task,
            &options,
            &self.client,
            &self.registry,
            &self.hooks,
            ctx,
        )
        .await
        .map_err(|err| ToolError::Execution {
            tool: SPAWN_SUBAGENT_TOOL_NAME.to_string(),
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

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::Mutex;

    use futures_util::stream;

    use super::*;
    use crate::llm::{ChatChunk, ChatStream};

    /// Temp project dir removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("wizard-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Provider that replays canned chunk sequences (or a permanent error)
    /// and records the requests it received.
    struct ScriptedProvider {
        responses: Mutex<VecDeque<Vec<ChatChunk>>>,
        requests: Mutex<Vec<ChatRequest>>,
        /// When set, every chat_stream call fails with this HTTP status.
        fail_status: Option<u16>,
    }

    impl ScriptedProvider {
        fn new(responses: Vec<Vec<ChatChunk>>) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(responses.into()),
                requests: Mutex::new(Vec::new()),
                fail_status: None,
            })
        }

        fn failing(status: u16) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(VecDeque::new()),
                requests: Mutex::new(Vec::new()),
                fail_status: Some(status),
            })
        }
    }

    #[async_trait]
    impl LlmProvider for ScriptedProvider {
        async fn health(&self) -> Result<()> {
            Ok(())
        }

        async fn supports_native_tools(&self, _model: &str) -> Result<bool> {
            Ok(true)
        }

        async fn list_models(&self) -> Result<Vec<String>> {
            Ok(Vec::new())
        }

        async fn chat_stream(&self, request: ChatRequest) -> Result<ChatStream> {
            self.requests.lock().unwrap().push(request);
            if let Some(status) = self.fail_status {
                return Err(crate::llm::ProviderError::http(status, "scripted failure").into());
            }
            let chunks = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted response available");
            Ok(futures_util::StreamExt::boxed(stream::iter(
                chunks.into_iter().map(Ok),
            )))
        }

        fn label(&self) -> String {
            "scripted:test".to_string()
        }
    }

    fn chunk(content: &str, thinking: bool, done: bool) -> ChatChunk {
        ChatChunk {
            message: Some(ChatMessage::assistant(content)),
            thinking,
            done,
            done_reason: None,
            eval_count: None,
            prompt_eval_count: None,
        }
    }

    /// Minimal tool with a configurable access class.
    struct FakeTool {
        name: &'static str,
        access: ToolAccess,
    }

    #[async_trait]
    impl Tool for FakeTool {
        fn name(&self) -> &str {
            self.name
        }

        fn description(&self) -> &str {
            "fake tool for subagent tests"
        }

        fn parameters(&self) -> Value {
            json!({ "type": "object", "properties": {} })
        }

        fn access(&self) -> ToolAccess {
            self.access
        }

        async fn execute(&self, _args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::ok("ok"))
        }
    }

    fn worker() -> SubagentConfig {
        builtin_configs()
            .into_iter()
            .next()
            .expect("builtin worker")
    }

    #[test]
    fn read_only_registry_keeps_only_read_only_tools() {
        let mut parent = ToolRegistry::new();
        parent.register(Arc::new(FakeTool {
            name: "probe",
            access: ToolAccess::ReadOnly,
        }));
        parent.register(Arc::new(FakeTool {
            name: "mutate",
            access: ToolAccess::Edit,
        }));
        parent.register(Arc::new(FakeTool {
            name: "run",
            access: ToolAccess::Execute,
        }));

        let filtered = read_only_registry(&parent);
        assert!(filtered.get("probe").is_some());
        assert!(filtered.get("mutate").is_none());
        assert!(filtered.get("run").is_none());
        assert_eq!(filtered.len(), 1);
    }

    #[test]
    fn scoped_registry_selects_named_tools_and_skips_unknown() {
        let mut parent = ToolRegistry::new();
        parent.register(Arc::new(FakeTool {
            name: "probe",
            access: ToolAccess::ReadOnly,
        }));
        parent.register(Arc::new(FakeTool {
            name: "run",
            access: ToolAccess::Execute,
        }));

        let all = scoped_registry(&parent, None);
        assert_eq!(all.len(), 2);
        let scoped = scoped_registry(&parent, Some(&["probe".to_string(), "missing".to_string()]));
        assert!(scoped.get("probe").is_some());
        assert!(scoped.get("missing").is_none());
        assert_eq!(scoped.len(), 1);
    }

    #[test]
    fn user_configs_shadow_builtins_by_name() {
        let tmp = TempDir::new();
        std::fs::write(
            tmp.0.join("worker.toml"),
            "name = \"worker\"\ndescription = \"custom\"\nsystem_prompt = \"be custom\"\n",
        )
        .unwrap();
        let configs = available_configs(&tmp.0);
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].description, "custom");
    }

    #[tokio::test]
    async fn spawn_skips_thinking_chunks_and_uses_the_model_override() {
        let tmp = TempDir::new();
        let provider = ScriptedProvider::new(vec![vec![
            chunk("secret reasoning", true, false),
            chunk("the actual report", false, true),
        ]]);
        let hooks = HookEngine::new(Vec::new(), tmp.0.clone(), "test".to_string());
        let ctx = ToolContext::new(&tmp.0);
        let client: Arc<dyn LlmProvider> = provider.clone();

        let options = SpawnOptions {
            model: Some("parent-active-model".to_string()),
            read_only: false,
        };
        let result = spawn(
            &worker(),
            "report",
            &options,
            &client,
            &ToolRegistry::new(),
            &hooks,
            &ctx,
        )
        .await
        .expect("spawn ok");

        assert!(result.completed);
        assert_eq!(result.output, "the actual report", "thinking never leaks");
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests[0].model, "parent-active-model");
    }

    #[tokio::test]
    async fn spawn_fails_fast_on_permanent_provider_errors() {
        let tmp = TempDir::new();
        let provider = ScriptedProvider::failing(401);
        let hooks = HookEngine::new(Vec::new(), tmp.0.clone(), "test".to_string());
        let ctx = ToolContext::new(&tmp.0);
        let client: Arc<dyn LlmProvider> = provider.clone();

        let err = spawn(
            &worker(),
            "report",
            &SpawnOptions::default(),
            &client,
            &ToolRegistry::new(),
            &hooks,
            &ctx,
        )
        .await
        .expect_err("permanent error fails the run");
        assert!(format!("{err:#}").contains("scripted failure"), "{err:#}");
        assert_eq!(
            provider.requests.lock().unwrap().len(),
            1,
            "a 401 is never retried"
        );
    }
}
