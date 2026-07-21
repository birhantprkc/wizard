//! Subagents: isolated sub-contexts for parallel or decomposed work.
//!
//! Each subagent gets its own message history and tool scope. By default a
//! sub-loop has no step ceiling (same as the parent turn); an optional
//! `max_steps` on the definition can still cap a specialist. The result
//! returns to the parent as a single tool result, so a multi-step sub-task
//! costs the parent one turn of context.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::{Config, Mode, StepBudget};
use crate::hooks::{HookEngine, PreToolUse};
use crate::llm::provider::LlmProvider;
use crate::llm::{ChatMessage, ChatOptions, ChatRequest, Image, Role, ToolCall};
use crate::tools::subagent_tasks::SubagentRunResult;
use crate::tools::{
    CommandDispatch, Tool, ToolAccess, ToolContext, ToolError, ToolOutput, registry::ToolRegistry,
};

use super::prompts;
use super::{error_is_transient, normalize_args, parse_json_tool_call};

/// Advertised name of the spawn tool, referenced by the dispatcher's
/// plan-mode gate.
pub const SPAWN_SUBAGENT_TOOL_NAME: &str = "spawn_subagent";

/// Retry budget for one subagent model call (mirrors the parent loop's
/// non-continuous budget).
const RETRY_ATTEMPTS: u32 = 6;

/// What a subagent reports when its loop ended without any final text — it
/// only ever called tools, or the model returned nothing. Not an error, but
/// not an answer either, which is why a caller that *judges* subagent output
/// ([`crate::agent::ultra`]) has to be able to tell the two apart.
pub const NO_FINAL_TEXT: &str = "(subagent produced no final text)";

/// Session-unique id for one subagent run. Every `AgentEvent::SubagentRun*`
/// event carries it, so a surface can demux concurrent runs — including two
/// runs of the same subagent — into separate panes.
pub fn next_run_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

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
    /// Optional step ceiling for the sub-loop. Defaults to unlimited (`0`) —
    /// the subagent runs until it finishes, the parent kills it, or a hard
    /// error stops it. Set a positive number only when a specialist should
    /// be hard-capped.
    #[serde(default = "SubagentConfig::default_max_steps")]
    pub max_steps: StepBudget,
}

impl SubagentConfig {
    fn default_max_steps() -> StepBudget {
        StepBudget::UNLIMITED
    }
}

/// Outcome of a subagent run, summarized for the parent.
#[derive(Debug, Clone)]
pub struct SubagentResult {
    pub name: String,
    /// The subagent's final answer text.
    pub output: String,
    pub steps_used: u32,
    /// False when the sub-loop hit an optional step budget or errored out.
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
    /// When set, seed the run from the parent conversation instead of a fresh
    /// system prompt + bare task. Used by [`spawn_fork`] (`/fork`): the side
    /// quest inherits history, tools, and prompt, then appends its brief.
    pub inherited_history: Option<Vec<ChatMessage>>,
}

/// Built-in name for a `/fork` side-quest run (shown on the subagent rail and
/// in the background-subagent report injected into the parent).
pub const FORK_NAME: &str = "fork";

/// Tools a fork must never call: nesting another spawn would recurse forever,
/// and interactive / surface-bound tools have no user attached to answer them.
const FORK_TOOL_DENYLIST: &[&str] = &[
    SPAWN_SUBAGENT_TOOL_NAME,
    "run_command",
    "exit_plan",
    "interview",
];

/// System reminder appended as the user message that launches a `/fork`
/// side quest. The parent conversation stays untouched; this brief is only
/// in the fork's own history.
const FORK_BRIEF: &str = "\
This is a forked side quest from the user (\"/fork\"). You inherit the full \
conversation above — history, tools, and system prompt — and run in parallel \
with the main session.\n\
\n\
CRITICAL CONSTRAINTS:\n\
- Complete the side quest end-to-end using your tools, then reply with a \
concise final report of what you found or changed.\n\
- Do not ask the user questions; make reasonable decisions and note them in \
your report.\n\
- Do not try to steer the main conversation or wait on it — you are a \
detached worker. Your report is injected back into the main session when \
you finish.\n\
- Stay focused on the side quest below; ignore unrelated open work unless it \
blocks you.";

/// Parent tool set with the tools a fork must never call stripped (see
/// [`FORK_TOOL_DENYLIST`]).
pub fn fork_registry(parent: &ToolRegistry) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    for spec in parent.specs() {
        let name = spec.function.name.as_str();
        if FORK_TOOL_DENYLIST.contains(&name) {
            continue;
        }
        if let Some(tool) = parent.get(name) {
            registry.register(Arc::clone(tool));
        }
    }
    registry
}

/// Config used by every `/fork` run: general-purpose worker, full remaining
/// tool set, no step ceiling.
pub fn fork_config() -> SubagentConfig {
    SubagentConfig {
        name: FORK_NAME.to_string(),
        description: "User-spawned side quest that inherits the full conversation \
                      context and reports back when finished."
            .to_string(),
        // Unused when `inherited_history` is set — the parent's system prompt
        // already sits at history[0]. Kept as a safe fallback if a caller
        // ever spawns a fork without history.
        system_prompt: "You are a focused fork of Wizard. Complete the given side \
                        quest end-to-end, then reply with a concise final report."
            .to_string(),
        tool_scope: None,
        max_steps: SubagentConfig::default_max_steps(),
    }
}

/// Run a `/fork` side quest: same loop as [`spawn`], but seeded with the
/// parent's conversation and a stripped tool set. Streams progress as
/// `SubagentRun*` events and returns one final report for the parent.
#[allow(clippy::too_many_arguments)]
pub async fn spawn_fork(
    run: u64,
    task: &str,
    history: Vec<ChatMessage>,
    options: &SpawnOptions,
    client: &Arc<dyn LlmProvider>,
    registry: &ToolRegistry,
    hooks: &HookEngine,
    ctx: &ToolContext,
) -> Result<SubagentResult> {
    let config = fork_config();
    let mut options = options.clone();
    options.inherited_history = Some(history);
    // Forks always scope down from a denylisted snapshot so a parent that
    // still has `spawn_subagent` registered cannot recurse through the fork.
    let scoped = fork_registry(registry);
    spawn(run, &config, task, &options, client, &scoped, hooks, ctx).await
}

/// One model round-trip: stream a completion, skipping reasoning
/// ("thinking") chunks so they never leak into subagent history or reports.
/// Any images the model generated come back alongside the text (see
/// [`ChatChunk::images`](crate::llm::ChatChunk::images)).
async fn stream_step(client: &Arc<dyn LlmProvider>, request: ChatRequest) -> Result<Step> {
    let mut stream = client.chat_stream(request).await?;
    let mut step = Step::default();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        step.images.extend(chunk.images);
        if let Some(message) = chunk.message {
            if !chunk.thinking {
                step.content.push_str(&message.content);
            }
            step.images.extend(message.images);
            step.tool_calls.extend(message.tool_calls);
        }
        if chunk.prompt_eval_count.is_some() {
            step.prompt_tokens = chunk.prompt_eval_count;
        }
        if chunk.eval_count.is_some() {
            step.completion_tokens = chunk.eval_count;
        }
        if chunk.done {
            break;
        }
    }
    Ok(step)
}

/// One completed model call inside a subagent's loop: what the model said,
/// what it asked to run, and what it cost (when the backend reported counts).
#[derive(Debug, Default)]
struct Step {
    content: String,
    tool_calls: Vec<ToolCall>,
    /// Images the model generated during the call (see
    /// [`ChatChunk::images`](crate::llm::ChatChunk::images)).
    images: Vec<Image>,
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
}

/// Account for one subagent model call, on both of the surfaces that report
/// spend: the parent's counters (`/cost`, the usage log) through the shared
/// [`ToolContext::usage`] tracker, and the live status bar through the run's
/// event channel.
///
/// Without this a subagent's tokens are spent and then reported nowhere. That
/// was survivable while `spawn_subagent` was an occasional tool call; `/ultra`
/// makes N candidate runs plus a judge the price of *every* turn, so a status
/// bar that counted the main loop alone would understate an ultra turn several
/// times over — under a chip that advertises exactly that multiplier.
///
/// It is [`crate::usage::UsageTracker::record_delegated`], not `record`: these
/// tokens belong on the totals but must not become the parent's `last_prompt`,
/// which is what decides when to compact.
async fn record_usage(ctx: &ToolContext, progress: Option<&Progress>, step: &Step) {
    if step.prompt_tokens.is_none() && step.completion_tokens.is_none() {
        return;
    }
    let prompt_tokens = step.prompt_tokens.unwrap_or(0);
    let completion_tokens = step.completion_tokens.unwrap_or(0);
    if let Some(usage) = &ctx.usage {
        usage.record_delegated(prompt_tokens, completion_tokens);
    }
    if let Some(events) = progress {
        super::emit(
            events,
            crate::agent::AgentEvent::Usage {
                prompt_tokens,
                completion_tokens,
            },
        )
        .await;
    }
}

/// The run's event channel: where a subagent's progress (and its spend) is
/// streamed for the surface to render as its pane.
type Progress = tokio::sync::mpsc::Sender<crate::agent::AgentEvent>;

/// Run `task` in an isolated context defined by `config`: fresh history,
/// scoped registry, optional step budget. The parent's lifecycle `hooks` apply to
/// the subagent's tool calls too.
///
/// The subagent reports back to the parent model as one tool result, but its
/// step-by-step activity streams to the surface as `AgentEvent::SubagentRun*`
/// events scoped to `run` (see [`next_run_id`]), which the TUI renders as that
/// subagent's own pane. The caller emits `SubagentRunStarted` (it knows the
/// background id); this function emits everything after it, including the
/// terminal `SubagentRunDone`.
#[allow(clippy::too_many_arguments)]
pub async fn spawn(
    run: u64,
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
    let native_tools = crate::llm::provider::probe_native_tools(client.as_ref(), &model).await;

    let mut history = match &options.inherited_history {
        // `/fork`: seed from the parent's conversation, then append the
        // side-quest brief. The parent's system prompt (and any mid-session
        // notes) stay at the front; we only add the fork instruction + task.
        Some(parent_history) => {
            let mut history = parent_history.clone();
            // When the parent is on the JSON tool protocol, refresh the tool
            // list against *this* run's scoped registry so the fork doesn't
            // advertise tools we stripped (spawn_subagent, exit_plan, …).
            if !native_tools
                && let Some(system) = history.first_mut()
                && system.role == Role::System
            {
                system.content.push_str("\n\n");
                system
                    .content
                    .push_str(&prompts::render_tool_protocol(&scoped.specs()));
            }
            history.push(ChatMessage::user(format!("{FORK_BRIEF}\n\n{task}")));
            history
        }
        None => {
            let mut system_prompt = config.system_prompt.clone();
            if !native_tools {
                system_prompt.push_str("\n\n");
                system_prompt.push_str(&prompts::render_tool_protocol(&scoped.specs()));
            }
            vec![
                ChatMessage::system(system_prompt),
                ChatMessage::user(task.to_string()),
            ]
        }
    };

    // The subagent reports back to the model as one tool result, but its
    // step-by-step activity streams to the surface as run-scoped events so the
    // user can open its pane and watch it work. Nested tools run with
    // `events: None` so they don't double-emit (todos, background tasks) or
    // leak into the parent's transcript; we emit our own run-scoped pair.
    let progress = ctx.events.clone();
    // Forks keep the parent's todo list (shared work, shared status bar);
    // ordinary subagents get a fresh one so their scratch todos never leak.
    let todos = if options.inherited_history.is_some() {
        Arc::clone(&ctx.todos)
    } else {
        Arc::new(std::sync::Mutex::new(crate::tools::todo::TodoList::new()))
    };
    let ctx = ToolContext {
        todos,
        events: None,
        // A subagent has no surface to drive; it must never dispatch the
        // parent's slash commands even if the parent's ctx enabled it.
        command_dispatch: CommandDispatch::None,
        ..ctx.clone()
    };

    let mut steps_used = 0;
    let mut completed = false;
    let mut last_text = String::new();
    let max_steps = config.max_steps.last_step();

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
        let step_result = loop {
            match stream_step(client, request.clone()).await {
                Ok(completion) => break completion,
                Err(err) => {
                    if !error_is_transient(&err) || attempt >= RETRY_ATTEMPTS {
                        let err = err.context(format!("subagent '{}' chat failed", config.name));
                        // Close the pane out, or it sits at "running" forever.
                        if let Some(events) = &progress {
                            super::emit(
                                events,
                                crate::agent::AgentEvent::SubagentRunDone {
                                    run,
                                    completed: false,
                                    output: String::new(),
                                    steps_used,
                                    error: Some(format!("{err:#}")),
                                },
                            )
                            .await;
                        }
                        return Err(err);
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
        record_usage(&ctx, progress.as_ref(), &step_result).await;
        let Step {
            content,
            mut tool_calls,
            images,
            ..
        } = step_result;

        // Images the subagent's model generated: persisted to the session's
        // image store (shared with the parent through the context) and
        // announced on this run's pane before they land in its history.
        let images =
            crate::agent::absorb_images(images, ctx.images.as_ref(), progress.as_ref(), |images| {
                crate::agent::AgentEvent::SubagentRunImages {
                    run,
                    source: crate::agent::ImageSource::Assistant,
                    images,
                }
            })
            .await;

        history.push(ChatMessage {
            role: Role::Assistant,
            content: content.clone(),
            tool_calls: tool_calls.clone(),
            tool_name: None,
            images,
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
            last_text = content.clone();
        }

        // The subagent's own message for this step, into its pane.
        if let Some(events) = &progress
            && !content.trim().is_empty()
        {
            super::emit(
                events,
                crate::agent::AgentEvent::SubagentRunText {
                    run,
                    text: content.clone(),
                },
            )
            .await;
        }

        for call in tool_calls {
            let name = call.function.name.clone();
            let mut args = normalize_args(&call.function.arguments);
            if let Some(events) = &progress {
                super::emit(
                    events,
                    crate::agent::AgentEvent::SubagentRunToolStarted {
                        run,
                        name: name.clone(),
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
                    crate::agent::AgentEvent::SubagentRunToolFinished {
                        run,
                        name: name.clone(),
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
                ChatMessage::tool_result(name.clone(), body)
            } else {
                ChatMessage::user(format!("Tool result for `{name}`:\n{body}"))
            });

            // Same convention as the parent loop: a tool's images ride back to
            // the model on a following user message (a `tool` result cannot
            // carry them on OpenAI), after being persisted and announced.
            if !output.images.is_empty() {
                let tool = name.clone();
                let images = crate::agent::absorb_images(
                    output.images,
                    ctx.images.as_ref(),
                    progress.as_ref(),
                    |images| crate::agent::AgentEvent::SubagentRunImages {
                        run,
                        source: crate::agent::ImageSource::Tool(tool),
                        images,
                    },
                )
                .await;
                if !images.is_empty() {
                    history.push(ChatMessage::user_with_images(
                        format!("Image(s) returned by `{name}`:"),
                        images,
                    ));
                }
            }
        }

        if let Some(events) = &progress {
            super::emit(
                events,
                crate::agent::AgentEvent::SubagentRunStep { run, step },
            )
            .await;
        }
    }

    let output = if last_text.trim().is_empty() {
        NO_FINAL_TEXT.to_string()
    } else {
        last_text
    };
    if let Some(events) = &progress {
        super::emit(
            events,
            crate::agent::AgentEvent::SubagentRunDone {
                run,
                completed,
                output: output.clone(),
                steps_used,
                error: None,
            },
        )
        .await;
    }

    Ok(SubagentResult {
        name: config.name.clone(),
        output,
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
                    "\n  - `{}` — {} (tools: {}; {})",
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
            ..Default::default()
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

        let run = next_run_id();

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
                    run,
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
            // Reserve the id and announce the run *before* attaching the
            // driver, so the pane exists by the time the subagent's first
            // event lands in it.
            let id = ctx.subagents.reserve(&name, &args.task);
            if let Some(events) = &ctx.events {
                super::emit(
                    events,
                    crate::agent::AgentEvent::SubagentRunStarted {
                        run,
                        bg: Some(id),
                        name: name.clone(),
                        task: args.task.clone(),
                    },
                )
                .await;
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
            ctx.subagents.attach(id, fut);
            return Ok(ToolOutput::ok(format!(
                "Delegated to subagent '{name}' (#{id}): {}.\nRunning in the background — \
                 you'll see its progress as it works, and the report lands in your context \
                 once it's done.",
                args.task
            )));
        }

        if let Some(events) = &ctx.events {
            super::emit(
                events,
                crate::agent::AgentEvent::SubagentRunStarted {
                    run,
                    bg: None,
                    name: config.name.clone(),
                    task: args.task.clone(),
                },
            )
            .await;
        }

        let result = spawn(
            run,
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
    use crate::llm::{ChatChunk, ChatStream, FunctionCall};

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

    /// Provider that replays canned chunk sequences (or scripted failures)
    /// and records the requests it received.
    struct ScriptedProvider {
        responses: Mutex<VecDeque<Vec<ChatChunk>>>,
        requests: Mutex<Vec<ChatRequest>>,
        /// Upcoming chat_stream calls that fail with `fail_status` before the
        /// scripted responses resume; `u32::MAX` fails every call.
        fail: Mutex<u32>,
        fail_status: u16,
        /// What `supports_native_tools` reports.
        native_tools: bool,
    }

    impl ScriptedProvider {
        fn new(responses: Vec<Vec<ChatChunk>>) -> Arc<Self> {
            Self::build(responses, 0, 0, true)
        }

        fn failing(status: u16) -> Arc<Self> {
            Self::build(Vec::new(), u32::MAX, status, true)
        }

        fn flaky(status: u16, failures: u32, responses: Vec<Vec<ChatChunk>>) -> Arc<Self> {
            Self::build(responses, failures, status, true)
        }

        fn without_native_tools(responses: Vec<Vec<ChatChunk>>) -> Arc<Self> {
            Self::build(responses, 0, 0, false)
        }

        fn build(
            responses: Vec<Vec<ChatChunk>>,
            fail: u32,
            fail_status: u16,
            native_tools: bool,
        ) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(responses.into()),
                requests: Mutex::new(Vec::new()),
                fail: Mutex::new(fail),
                fail_status,
                native_tools,
            })
        }
    }

    #[async_trait]
    impl LlmProvider for ScriptedProvider {
        async fn health(&self) -> Result<()> {
            Ok(())
        }

        async fn supports_native_tools(&self, _model: &str) -> Result<bool> {
            Ok(self.native_tools)
        }

        async fn list_models(&self) -> Result<Vec<String>> {
            Ok(Vec::new())
        }

        async fn chat_stream(&self, request: ChatRequest) -> Result<ChatStream> {
            self.requests.lock().unwrap().push(request);
            {
                let mut fail = self.fail.lock().unwrap();
                if *fail > 0 {
                    if *fail != u32::MAX {
                        *fail -= 1;
                    }
                    return Err(crate::llm::ProviderError::http(
                        self.fail_status,
                        "scripted failure",
                    )
                    .into());
                }
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
            images: Vec::new(),
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

    /// The spawn tool reads the parent's live model out of the slot handed to
    /// `Agent::bind_subagent_model`. A surface that builds the tool and drops
    /// the handle strands its subagents on the *configured* model, silently
    /// ignoring `/model` — which is what the TUI did until its registry was
    /// made to hand the handle back.
    #[tokio::test]
    async fn a_bound_model_handle_is_what_subagents_run_on() {
        let tmp = TempDir::new();
        let provider = ScriptedProvider::new(vec![vec![chunk("done", false, true)]]);
        let client: Arc<dyn LlmProvider> = provider.clone();
        let tool = SpawnSubagentTool::new(
            vec![worker()],
            Arc::clone(&client),
            Arc::new(ToolRegistry::new()),
            Arc::new(HookEngine::new(Vec::new(), tmp.0.clone(), "test".into())),
        );

        // Nothing bound: the slot is empty and the sub-loop falls back to the
        // configured model.
        assert!(tool.active_model().is_none());

        // Bound, then written through by a `/model` switch.
        let handle = tool.model_handle();
        *handle.write().unwrap() = Some("switched-model".to_string());

        tool.execute(
            serde_json::json!({ "subagent": worker().name, "task": "report" }),
            &ToolContext::new(&tmp.0),
        )
        .await
        .expect("spawn ok");

        let requests = provider.requests.lock().unwrap();
        assert_eq!(
            requests[0].model, "switched-model",
            "the subagent ran on the parent's switched model, not the configured one"
        );
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
            ..Default::default()
        };
        let result = spawn(
            next_run_id(),
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
    async fn images_inside_a_subagent_run_are_persisted_and_announced_on_the_run() {
        // A tool inside a run returns an image: it must reach the subagent's
        // model (following user message), land in the session's image store,
        // and be announced on the run's own events — not lost between panes.
        struct ShotTool;
        #[async_trait]
        impl Tool for ShotTool {
            fn name(&self) -> &str {
                "generate_image"
            }
            fn description(&self) -> &str {
                "Generate an image."
            }
            fn parameters(&self) -> Value {
                json!({ "type": "object", "properties": {} })
            }
            async fn execute(
                &self,
                _args: Value,
                _ctx: &ToolContext,
            ) -> Result<ToolOutput, ToolError> {
                let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
                bytes.extend_from_slice(b"pixels");
                Ok(ToolOutput::ok_with_images(
                    "rendered",
                    vec![Image::from_bytes(&bytes).expect("a PNG")],
                ))
            }
        }

        let tmp = TempDir::new();
        let mut call = ChatMessage::assistant("");
        call.tool_calls.push(ToolCall {
            function: FunctionCall {
                name: "generate_image".to_string(),
                arguments: json!({}),
            },
        });
        let provider = ScriptedProvider::new(vec![
            vec![ChatChunk {
                message: Some(call),
                images: Vec::new(),
                thinking: false,
                done: true,
                done_reason: None,
                eval_count: None,
                prompt_eval_count: None,
            }],
            vec![chunk("done", false, true)],
        ]);
        let hooks = HookEngine::new(Vec::new(), tmp.0.clone(), "test".to_string());
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let ctx = ToolContext::new(&tmp.0)
            .with_images(Arc::new(crate::images::ImageStore::in_dir(
                tmp.0.join("images"),
            )))
            .with_events(tx);
        let client: Arc<dyn LlmProvider> = provider.clone();
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(ShotTool));

        let run = next_run_id();
        let result = spawn(
            run,
            &worker(),
            "make a picture",
            &SpawnOptions::default(),
            &client,
            &registry,
            &hooks,
            &ctx,
        )
        .await
        .expect("spawn ok");
        assert!(result.completed);

        // The image reached the subagent's model on a following user message.
        let second = provider.requests.lock().unwrap()[1].messages.clone();
        let carried = second
            .iter()
            .find(|message| !message.images.is_empty())
            .expect("a message carrying the image");
        assert_eq!(carried.role, crate::llm::Role::User);
        assert_eq!(carried.images[0].mime, "image/png");

        // And it was announced on this run, with a path on disk.
        let mut announced = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let crate::agent::AgentEvent::SubagentRunImages {
                run: id,
                source,
                images,
            } = event
            {
                assert_eq!(id, run, "scoped to the run that produced it");
                announced.push((source, images));
            }
        }
        assert_eq!(announced.len(), 1);
        assert_eq!(
            announced[0].0,
            crate::agent::ImageSource::Tool("generate_image".to_string())
        );
        assert!(announced[0].1[0].path.is_file(), "written to disk");
    }

    #[tokio::test]
    async fn a_subagents_tokens_bill_the_parent_and_reach_the_surface() {
        let tmp = TempDir::new();
        // Two model calls: one that asks for a tool, then the report. Both
        // report counts, and both have to be accounted for — an ultra turn is
        // N of these runs, and the status bar shows one number.
        let provider = ScriptedProvider::new(vec![
            vec![ChatChunk {
                message: Some(ChatMessage {
                    role: Role::Assistant,
                    content: String::new(),
                    tool_calls: vec![ToolCall {
                        function: FunctionCall {
                            name: "probe".to_string(),
                            arguments: json!({}),
                        },
                    }],
                    tool_name: None,
                    images: Vec::new(),
                }),
                prompt_eval_count: Some(100),
                eval_count: Some(20),
                ..chunk("", false, true)
            }],
            vec![ChatChunk {
                prompt_eval_count: Some(300),
                eval_count: Some(40),
                ..chunk("the report", false, true)
            }],
        ]);
        let hooks = HookEngine::new(Vec::new(), tmp.0.clone(), "test".to_string());
        let usage = Arc::new(crate::usage::UsageTracker::new());
        let (events, mut drain) = tokio::sync::mpsc::channel(64);
        let ctx = ToolContext::new(&tmp.0)
            .with_usage(Arc::clone(&usage))
            .with_events(events);
        let client: Arc<dyn LlmProvider> = provider.clone();
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(FakeTool {
            name: "probe",
            access: ToolAccess::ReadOnly,
        }));

        let result = spawn(
            next_run_id(),
            &worker(),
            "report",
            &SpawnOptions::default(),
            &client,
            &registry,
            &hooks,
            &ctx,
        )
        .await
        .expect("spawn ok");
        assert_eq!(result.output, "the report");

        assert_eq!(
            usage.session_totals(),
            (400, 60),
            "the parent paid for both of the subagent's model calls, so both land on its totals \
             (and therefore in /cost)"
        );
        assert_eq!(
            usage.last_prompt_tokens(),
            None,
            "but never on last_prompt: that is the parent's own prompt size, and it decides when \
             to compact"
        );

        let mut reported = Vec::new();
        while let Ok(event) = drain.try_recv() {
            if let crate::agent::AgentEvent::Usage {
                prompt_tokens,
                completion_tokens,
            } = event
            {
                reported.push((prompt_tokens, completion_tokens));
            }
        }
        assert_eq!(
            reported,
            [(100, 20), (300, 40)],
            "one Usage event per model call, so the status bar counts the fan-out it advertises"
        );
    }

    #[tokio::test]
    async fn spawn_fails_fast_on_permanent_provider_errors() {
        let tmp = TempDir::new();
        let provider = ScriptedProvider::failing(401);
        let hooks = HookEngine::new(Vec::new(), tmp.0.clone(), "test".to_string());
        let ctx = ToolContext::new(&tmp.0);
        let client: Arc<dyn LlmProvider> = provider.clone();

        let err = spawn(
            next_run_id(),
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

    fn test_hooks(tmp: &TempDir) -> Arc<HookEngine> {
        Arc::new(HookEngine::new(Vec::new(), tmp.0.clone(), "test".into()))
    }

    /// `done: true` chunk carrying one tool call alongside `content`.
    fn tool_call_chunk(name: &str, content: &str) -> ChatChunk {
        let mut message = ChatMessage::assistant(content);
        message.tool_calls.push(ToolCall {
            function: FunctionCall {
                name: name.to_string(),
                arguments: json!({}),
            },
        });
        ChatChunk {
            message: Some(message),
            images: Vec::new(),
            thinking: false,
            done: true,
            done_reason: None,
            eval_count: None,
            prompt_eval_count: None,
        }
    }

    #[test]
    fn invalid_manifests_are_skipped_and_the_rest_load() {
        let tmp = TempDir::new();
        std::fs::write(tmp.0.join("bad.toml"), "name = \"broken").unwrap();
        std::fs::write(
            tmp.0.join("good.toml"),
            "name = \"helper\"\ndescription = \"d\"\nsystem_prompt = \"p\"\n",
        )
        .unwrap();
        std::fs::write(tmp.0.join("ignored.txt"), "not toml").unwrap();

        let configs = load_dir(&tmp.0).expect("load ok");
        assert_eq!(configs.len(), 1, "the bad manifest costs itself only");
        assert_eq!(configs[0].name, "helper");
        assert_eq!(
            configs[0].max_steps,
            crate::config::StepBudget::UNLIMITED,
            "an omitted budget is unlimited"
        );
    }

    #[tokio::test]
    async fn unknown_subagent_is_rejected_with_the_roster() {
        let tmp = TempDir::new();
        let provider = ScriptedProvider::new(Vec::new());
        let client: Arc<dyn LlmProvider> = provider.clone();
        let tool = SpawnSubagentTool::new(
            vec![worker()],
            client,
            Arc::new(ToolRegistry::new()),
            test_hooks(&tmp),
        );

        let err = tool
            .execute(
                json!({ "subagent": "nope", "task": "anything" }),
                &ToolContext::new(&tmp.0),
            )
            .await
            .expect_err("unknown name is invalid args");
        let message = err.to_string();
        assert!(message.contains("unknown subagent 'nope'"), "{message}");
        assert!(
            message.contains("worker"),
            "the roster is listed: {message}"
        );
        assert!(
            provider.requests.lock().unwrap().is_empty(),
            "no model call for a bad name"
        );
    }

    #[tokio::test]
    async fn a_run_that_exhausts_its_step_budget_reports_an_error_summary() {
        let tmp = TempDir::new();
        let provider = ScriptedProvider::new(vec![
            vec![tool_call_chunk("probe", "")],
            vec![tool_call_chunk("probe", "still digging")],
        ]);
        let client: Arc<dyn LlmProvider> = provider.clone();
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(FakeTool {
            name: "probe",
            access: ToolAccess::ReadOnly,
        }));
        let mut config = worker();
        config.max_steps = crate::config::StepBudget::new(2);
        let tool =
            SpawnSubagentTool::new(vec![config], client, Arc::new(registry), test_hooks(&tmp));

        let output = tool
            .execute(
                json!({ "subagent": "worker", "task": "dig" }),
                &ToolContext::new(&tmp.0),
            )
            .await
            .expect("tool output");
        assert!(output.is_error, "a budget stop is an error result");
        assert!(
            output.content.contains("hit its step budget"),
            "{}",
            output.content
        );
        assert!(output.content.contains("2 step(s)"), "{}", output.content);
        assert!(
            output.content.contains("still digging"),
            "the last text the subagent produced is the report: {}",
            output.content
        );
    }

    #[tokio::test(start_paused = true)]
    async fn transient_errors_back_off_and_retry_until_the_stream_recovers() {
        let tmp = TempDir::new();
        let provider = ScriptedProvider::flaky(429, 2, vec![vec![chunk("recovered", false, true)]]);
        let client: Arc<dyn LlmProvider> = provider.clone();
        let hooks = test_hooks(&tmp);
        let ctx = ToolContext::new(&tmp.0);

        let result = spawn(
            next_run_id(),
            &worker(),
            "report",
            &SpawnOptions::default(),
            &client,
            &ToolRegistry::new(),
            &hooks,
            &ctx,
        )
        .await
        .expect("run recovers");
        assert!(result.completed);
        assert_eq!(result.output, "recovered");
        assert_eq!(
            provider.requests.lock().unwrap().len(),
            3,
            "two transient failures, then the success"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_retry_budget_bounds_transient_failures_and_closes_the_pane() {
        use crate::agent::AgentEvent;

        let tmp = TempDir::new();
        let provider = ScriptedProvider::failing(503);
        let client: Arc<dyn LlmProvider> = provider.clone();
        let hooks = test_hooks(&tmp);
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let ctx = ToolContext::new(&tmp.0).with_events(tx);

        let run = next_run_id();
        let err = spawn(
            run,
            &worker(),
            "report",
            &SpawnOptions::default(),
            &client,
            &ToolRegistry::new(),
            &hooks,
            &ctx,
        )
        .await
        .expect_err("the budget bounds a persistent outage");
        assert!(format!("{err:#}").contains("chat failed"), "{err:#}");
        assert_eq!(
            provider.requests.lock().unwrap().len(),
            (RETRY_ATTEMPTS + 1) as usize,
            "initial attempt plus the retry budget"
        );

        let mut done = None;
        while let Ok(event) = rx.try_recv() {
            if let AgentEvent::SubagentRunDone {
                run: id,
                completed,
                error,
                ..
            } = event
            {
                done = Some((id, completed, error));
            }
        }
        let (id, completed, error) = done.expect("the pane is closed out");
        assert_eq!(id, run);
        assert!(!completed);
        assert!(
            error
                .expect("error carried on the terminal event")
                .contains("scripted failure")
        );
    }

    #[tokio::test]
    async fn plan_mode_restricts_a_spawned_run_to_read_only_tools() {
        let tmp = TempDir::new();
        let provider = ScriptedProvider::new(vec![
            vec![tool_call_chunk("mutate", "")],
            vec![chunk("gave up on writing", false, true)],
        ]);
        let client: Arc<dyn LlmProvider> = provider.clone();
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(FakeTool {
            name: "probe",
            access: ToolAccess::ReadOnly,
        }));
        registry.register(Arc::new(FakeTool {
            name: "mutate",
            access: ToolAccess::Edit,
        }));
        let tool =
            SpawnSubagentTool::new(vec![worker()], client, Arc::new(registry), test_hooks(&tmp));

        let output = tool
            .execute(
                json!({ "subagent": "worker", "task": "explore", "plan_mode": true }),
                &ToolContext::new(&tmp.0),
            )
            .await
            .expect("tool output");
        assert!(!output.is_error);

        let requests = provider.requests.lock().unwrap();
        let advertised: Vec<&str> = requests[0]
            .tools
            .iter()
            .map(|spec| spec.function.name.as_str())
            .collect();
        assert_eq!(advertised, ["probe"], "only read-only tools are offered");
        let feedback = requests[1]
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Tool)
            .expect("tool feedback");
        assert!(
            feedback.content.contains("unknown tool: mutate"),
            "the write tool does not exist inside the run: {}",
            feedback.content
        );
    }

    #[tokio::test]
    async fn json_protocol_runs_tools_for_models_without_native_calling() {
        let tmp = TempDir::new();
        let provider = ScriptedProvider::without_native_tools(vec![
            vec![chunk(r#"{"tool": "probe", "arguments": {}}"#, false, true)],
            vec![chunk("all done", false, true)],
        ]);
        let client: Arc<dyn LlmProvider> = provider.clone();
        let hooks = test_hooks(&tmp);
        let ctx = ToolContext::new(&tmp.0);
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(FakeTool {
            name: "probe",
            access: ToolAccess::ReadOnly,
        }));

        let result = spawn(
            next_run_id(),
            &worker(),
            "look around",
            &SpawnOptions::default(),
            &client,
            &registry,
            &hooks,
            &ctx,
        )
        .await
        .expect("spawn ok");
        assert!(result.completed);
        assert_eq!(result.output, "all done");
        assert_eq!(result.steps_used, 2);

        let requests = provider.requests.lock().unwrap();
        assert!(requests[0].tools.is_empty(), "no native tool specs sent");
        let system = &requests[0].messages[0].content;
        assert!(
            system.contains("do not have native function calling"),
            "the JSON protocol is taught: {system}"
        );
        assert!(
            system.contains("`probe`"),
            "the roster is rendered: {system}"
        );
        let feedback = requests[1]
            .messages
            .last()
            .expect("second request has messages");
        assert_eq!(feedback.role, Role::User, "results ride user messages");
        assert!(
            feedback.content.contains("Tool result for `probe`"),
            "{}",
            feedback.content
        );
    }

    #[tokio::test]
    async fn a_foreground_run_streams_run_scoped_events_in_order() {
        use crate::agent::AgentEvent;

        let tmp = TempDir::new();
        let provider = ScriptedProvider::new(vec![
            vec![tool_call_chunk("probe", "scouting")],
            vec![chunk("the report", false, true)],
        ]);
        let client: Arc<dyn LlmProvider> = provider.clone();
        let hooks = test_hooks(&tmp);
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let ctx = ToolContext::new(&tmp.0).with_events(tx);
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(FakeTool {
            name: "probe",
            access: ToolAccess::ReadOnly,
        }));

        let run = next_run_id();
        let result = spawn(
            run,
            &worker(),
            "scout",
            &SpawnOptions::default(),
            &client,
            &registry,
            &hooks,
            &ctx,
        )
        .await
        .expect("spawn ok");
        assert!(result.completed);
        assert_eq!(result.steps_used, 2);

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        assert_eq!(events.len(), 5, "run-scoped events only: {events:?}");
        assert!(matches!(
            &events[0],
            AgentEvent::SubagentRunText { run: id, text } if *id == run && text == "scouting"
        ));
        assert!(matches!(
            &events[1],
            AgentEvent::SubagentRunToolStarted { run: id, name, .. }
                if *id == run && name == "probe"
        ));
        assert!(matches!(
            &events[2],
            AgentEvent::SubagentRunToolFinished { run: id, name, output }
                if *id == run && name == "probe" && !output.is_error
        ));
        assert!(matches!(
            &events[3],
            AgentEvent::SubagentRunStep { run: id, step: 1 } if *id == run
        ));
        assert!(matches!(
            &events[4],
            AgentEvent::SubagentRunDone { run: id, completed: true, steps_used: 2, error: None, output }
                if *id == run && output == "the report"
        ));
    }

    #[tokio::test]
    async fn spawn_fork_inherits_parent_history_and_strips_nested_spawn() {
        let tmp = TempDir::new();
        let provider = ScriptedProvider::new(vec![vec![chunk("forked report", false, true)]]);
        let client: Arc<dyn LlmProvider> = provider.clone();
        let hooks = test_hooks(&tmp);
        let ctx = ToolContext::new(&tmp.0);

        // Parent tool set includes spawn_subagent and a normal tool; the fork
        // must keep the normal one and drop spawn.
        let mut parent_registry = ToolRegistry::new();
        parent_registry.register(Arc::new(FakeTool {
            name: "probe",
            access: ToolAccess::ReadOnly,
        }));
        parent_registry.register(Arc::new(SpawnSubagentTool::new(
            builtin_configs(),
            Arc::clone(&client),
            Arc::new(ToolRegistry::new()),
            Arc::clone(&hooks),
        )));

        let parent_history = vec![
            ChatMessage::system("you are the parent".to_string()),
            ChatMessage::user("we were discussing auth".to_string()),
            ChatMessage::assistant("right, the login flow"),
        ];

        let result = spawn_fork(
            next_run_id(),
            "summarize the auth discussion",
            parent_history.clone(),
            &SpawnOptions::default(),
            &client,
            &parent_registry,
            &hooks,
            &ctx,
        )
        .await
        .expect("fork ok");
        assert!(result.completed);
        assert_eq!(result.name, FORK_NAME);
        assert_eq!(result.output, "forked report");

        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 1, "one model call");
        let messages = &requests[0].messages;
        // Parent system + user + assistant + fork brief.
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].content, "you are the parent");
        assert!(
            messages[3]
                .content
                .contains("summarize the auth discussion"),
            "fork brief carries the task: {}",
            messages[3].content
        );
        assert!(
            messages[3].content.contains("/fork"),
            "fork brief identifies itself: {}",
            messages[3].content
        );
        // Tools advertised to the fork must exclude spawn_subagent.
        let tool_names: Vec<_> = requests[0]
            .tools
            .iter()
            .map(|t| t.function.name.as_str())
            .collect();
        assert!(
            tool_names.contains(&"probe"),
            "parent tools kept: {tool_names:?}"
        );
        assert!(
            !tool_names.contains(&SPAWN_SUBAGENT_TOOL_NAME),
            "spawn stripped: {tool_names:?}"
        );
    }

    #[test]
    fn fork_registry_strips_the_denylist() {
        let mut parent = ToolRegistry::new();
        parent.register(Arc::new(FakeTool {
            name: "probe",
            access: ToolAccess::ReadOnly,
        }));
        parent.register(Arc::new(FakeTool {
            name: "run_command",
            access: ToolAccess::Execute,
        }));
        parent.register(Arc::new(FakeTool {
            name: "exit_plan",
            access: ToolAccess::Execute,
        }));
        parent.register(Arc::new(FakeTool {
            name: "interview",
            access: ToolAccess::ReadOnly,
        }));
        parent.register(Arc::new(FakeTool {
            name: SPAWN_SUBAGENT_TOOL_NAME,
            access: ToolAccess::Execute,
        }));

        let scoped = fork_registry(&parent);
        let names: Vec<_> = scoped
            .specs()
            .into_iter()
            .map(|s| s.function.name)
            .collect();
        assert_eq!(names, vec!["probe".to_string()]);
    }
}
