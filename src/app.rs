//! TUI state machine: application state, slash commands, and the genie-mode
//! main loop. Rendering lives in [`crate::ui`]; raw events in
//! [`crate::event`].

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use serde_json::Value;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;

use crate::agent::{
    Agent, AgentEvent, DoneReason, ImageSource, InterviewQuestion, PlanVerdict, session::Session,
    subagent,
};
use crate::cli::Cli;
use crate::commands::CustomCommand;
use crate::config::{Config, Mode, ProviderConfig, ProviderKind, ReasoningEffort, StepBudget};
use crate::event::{Event, EventLoop};
use crate::evolve::{EvolveOutcome, EvolveRequest, EvolveTier, Evolver, PublishRequest, publish};
use crate::hooks::HookEngine;
use crate::image_view::ImageCache;
use crate::images::ImageRef;
use crate::import_claude::{self, ImportSelection};
use crate::llm::provider::LlmProvider;
use crate::mcp::{McpConfig, McpManager};
use crate::memory::MemoryStore;
use crate::server;
use crate::session_registry::{self, SessionRecord, SessionState};
use crate::skills::Skill;
use crate::tools::registry::ToolRegistry;
use crate::tools::todo::TodoItem;
use crate::vim::{self, Pending, VimMode, VimOp, VimState};

/// One rendered entry in the chat transcript.
#[derive(Debug, Clone)]
pub enum TranscriptEntry {
    User(String),
    Assistant(String),
    /// Model reasoning ("thinking") that preceded an assistant reply,
    /// rendered dimmed.
    Thinking(String),
    /// Collapsible tool invocation card.
    ToolCard {
        name: String,
        args: Value,
        /// `None` while the tool is still running.
        output: Option<String>,
        is_error: bool,
        collapsed: bool,
    },
    /// An image the turn produced — by the model, or by a tool
    /// ([`AgentEvent::Images`]). The file is already on disk; the entry holds
    /// only the reference to it, and [`crate::ui`] draws it (or, in a terminal
    /// that can draw nothing, prints where it is).
    Image {
        source: ImageSource,
        image: ImageRef,
    },
    /// System notice (mode switch, reload result, errors).
    Notice(String),
}

/// Lifecycle of one subagent run, as shown on its rail dot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneStatus {
    /// The sub-loop is still going.
    Running,
    /// The sub-loop finished on its own and reported back.
    Done,
    /// The run hit its step budget, errored out, or was killed.
    Failed,
}

impl PaneStatus {
    /// The rail's status glyph. Running panes animate via
    /// [`SubagentPane::glyph`]; these are the resting shapes.
    pub fn glyph(self) -> &'static str {
        match self {
            PaneStatus::Running => "●",
            PaneStatus::Done => "✔",
            PaneStatus::Failed => "✗",
        }
    }
}

/// Frames for a running pane's dot, cycled off the app tick so an active
/// subagent visibly pulses on the rail.
const PANE_SPINNER: [&str; 4] = ["●", "◉", "○", "◉"];

/// How long a finished run rests on the rail before it retires: long enough to
/// see it land, short enough that the rail stays a picture of live work. Its
/// report stays in the main chat either way.
const PANE_LINGER: Duration = Duration::from_secs(8);

/// One subagent run, surfaced on the rail below the composer and openable as
/// a full chat view.
///
/// This is the faithful record the old transcript-scraping monitor could not
/// build: the subagent's own messages and tool cards, streamed live off the
/// `AgentEvent::SubagentRun*` events and keyed by [`SubagentPane::run`].
#[derive(Debug, Clone)]
pub struct SubagentPane {
    /// Session-unique run id (`agent::subagent::next_run_id`).
    pub run: u64,
    /// Background-registry id, when the run was detached. `None` for a
    /// foreground run — which cannot be killed independently, since the
    /// parent turn is blocked on it.
    pub bg: Option<u32>,
    /// Subagent name (`researcher`, `reviewer`, …).
    pub name: String,
    /// The task it was handed.
    pub task: String,
    pub status: PaneStatus,
    /// The subagent's own conversation, rendered exactly like the main chat.
    pub transcript: Vec<TranscriptEntry>,
    /// Steps (model round-trips) completed so far.
    pub steps: u32,
    pub started: Instant,
    /// Set once the run ends; freezes the elapsed clock on the rail.
    pub finished: Option<Instant>,
    /// Entries appended since the user last had this pane open. Drives the
    /// unread badge, so you can tell which agent did something while you were
    /// looking elsewhere.
    pub unread: usize,
    /// First visible line of the pane transcript, measured from the top of the
    /// rendered content. Only consulted while [`Self::scroll_follow`] is false;
    /// when following, the live tail is always in view.
    pub scroll: u16,
    /// When true the pane sticks to the bottom as new output arrives. Scrolling
    /// up clears it; scrolling back to the bottom (or Ctrl-End) restores it.
    pub scroll_follow: bool,
    /// Last-drawn max scroll for this pane (content lines past the viewport).
    /// Written by the renderer so key handlers can convert a follow-tail view
    /// into a stable top-anchored offset without re-wrapping the transcript.
    pub max_scroll: std::cell::Cell<u16>,
}

impl SubagentPane {
    fn new(run: u64, bg: Option<u32>, name: String, task: String) -> Self {
        Self {
            run,
            bg,
            name,
            task,
            status: PaneStatus::Running,
            transcript: Vec::new(),
            steps: 0,
            started: Instant::now(),
            finished: None,
            unread: 0,
            scroll: 0,
            scroll_follow: true,
            max_scroll: std::cell::Cell::new(0),
        }
    }

    /// How long the run has been going, frozen once it ends.
    pub fn elapsed(&self) -> Duration {
        self.finished.unwrap_or_else(Instant::now) - self.started
    }

    /// The rail dot: a pulsing glyph while running, a resting one once done.
    pub fn glyph(&self, tick: u64) -> &'static str {
        match self.status {
            PaneStatus::Running => PANE_SPINNER[(tick / 2) as usize % PANE_SPINNER.len()],
            other => other.glyph(),
        }
    }

    /// One-line summary of what the subagent is doing right now: the tool it
    /// is in the middle of, else its latest message, else the task.
    pub fn activity(&self) -> &str {
        if self.status != PaneStatus::Running {
            return match self.transcript.iter().rev().find_map(|entry| match entry {
                TranscriptEntry::Assistant(text) => Some(text.as_str()),
                _ => None,
            }) {
                Some(text) => text,
                None => self.task.as_str(),
            };
        }
        for entry in self.transcript.iter().rev() {
            match entry {
                // A card still running is the most specific thing to show.
                TranscriptEntry::ToolCard {
                    name, output: None, ..
                } => return name.as_str(),
                TranscriptEntry::Assistant(text) if !text.trim().is_empty() => {
                    return text.as_str();
                }
                _ => {}
            }
        }
        self.task.as_str()
    }
}

/// Whether a finished tool's output is long enough to start collapsed: more
/// than six source lines, or a payload that would wrap well past that (one
/// giant minified line counts as 1 by `lines()` but fills the screen anyway).
fn collapse_long(content: &str) -> bool {
    content.lines().count() > 6 || content.chars().count() > 600
}

/// The images on a replayed message, as the references the live
/// [`AgentEvent::Images`] carried. An image the store could not write has no file
/// to draw and no path to print, so it is not replayed.
fn replayed_refs(images: &[crate::llm::Image]) -> Vec<ImageRef> {
    images
        .iter()
        .filter_map(|image| {
            Some(ImageRef {
                path: image.path.clone()?,
                mime: image.mime.clone(),
                bytes: image.decoded_len(),
            })
        })
        .collect()
}

/// The transcript entries an [`AgentEvent::Images`] becomes: one per image, each
/// carrying where it came from and where it landed.
fn image_entries(source: &ImageSource, images: Vec<ImageRef>) -> Vec<TranscriptEntry> {
    images
        .into_iter()
        .map(|image| TranscriptEntry::Image {
            source: source.clone(),
            image,
        })
        .collect()
}

/// Outcome of a background agent rebuild (model switch, crash recovery),
/// delivered to the main loop via [`Event::AgentRebuilt`].
pub struct AgentRebuild {
    /// Agent to restore into the main loop's slot. `None` when the rebuild
    /// failed outright and no previous agent could be preserved.
    pub agent: Option<Agent>,
    /// On a successful model switch, the tag to record in config/status.
    pub model: Option<String>,
    /// Notice appended to the transcript.
    pub notice: String,
}

impl std::fmt::Debug for AgentRebuild {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentRebuild")
            .field("agent", &self.agent.is_some())
            .field("model", &self.model)
            .field("notice", &self.notice)
            .finish()
    }
}

/// What the input line is currently doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InputMode {
    /// Composing a chat message.
    #[default]
    Chat,
    /// Composing a `/slash` command.
    Command,
    /// Answering an inline prompt (the interactive provider-setup flow): the
    /// composer collects one field at a time instead of submitting a message.
    Prompt,
}

/// A field being collected in the inline provider-setup prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptField {
    Name,
    /// Cloudflare account id — substituted into the base-URL template before
    /// the provider is built (Cloudflare setup only).
    AccountId,
    BaseUrl,
    Model,
    ApiKey,
}

/// In-progress provider setup driven by composer prompts. Each queued
/// [`PromptField`] is asked in turn; the answers fill the draft, and the last
/// answer emits a [`SlashCommand::ProviderSetup`].
#[derive(Debug, Clone)]
pub struct ProviderPrompt {
    kind: ProviderKind,
    name: String,
    base_url: String,
    model: String,
    api_key: Option<String>,
    /// Remaining fields to ask, in order.
    queue: std::collections::VecDeque<PromptField>,
}

/// Parsed `/slash` command (see the README table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    Help,
    Clear,
    /// `/model [tag]` — show current model, or switch to `tag`.
    Model(Option<String>),
    /// `/mode [genie|sovereign]` — show or switch mode.
    Mode(Option<Mode>),
    /// `/effort [low|medium|high|default]` — set the reasoning effort sent to
    /// models that support it. `None` opens the picker; `Some(None)` clears
    /// back to the provider default; `Some(Some(e))` sets the level.
    Effort(Option<Option<ReasoningEffort>>),
    /// `/evolve [--deep] <description>`.
    Evolve {
        deep: bool,
        description: String,
    },
    /// Reload skills, scripted tools, and MCP servers without restart.
    Reload,
    /// Toggle plan mode (also Shift+Tab): read-only investigation until a
    /// plan is approved via `exit_plan`.
    Plan,
    /// Toggle omakase (chef's-choice) mode: plan mode where the agent decides
    /// the approach itself and auto-approves its own plan — no interview, no
    /// review gate.
    Omakase,
    /// `/rewind [turn]` — restore file checkpoints and truncate history.
    /// `None` opens the turn picker; `Some` rewinds to before that turn.
    Rewind(Option<u64>),
    /// `/resume [id]` — reopen a past session and continue it. `None` opens
    /// the session picker; `Some` resumes that session id directly.
    Resume(Option<String>),
    /// `/compact` — summarize older history into a progress note now, instead
    /// of waiting for the automatic threshold.
    Compact,
    /// `/agents` — open the subagent roster picker (browse the available
    /// subagents and what each does; Enter pre-fills a delegation request).
    Agents,
    /// `/subagents` — toggle the in-session subagent monitor: the subagents
    /// that have run (or are running) this session, with live status.
    Subagents,
    /// Toggle the git diff sidebar.
    Diff,
    /// Toggle the todo side panel.
    Todos,
    /// Toggle the machine-wide session manager: every live Wizard session on
    /// the machine, grouped by state.
    Dashboard,
    /// Show session token usage (and cost when rates are configured).
    Cost,
    /// Show the saved project memories.
    Memory,
    /// Run the environment diagnostics (same checks as `wizard doctor`).
    Doctor,
    /// Show the session status: model, provider, mode, session id, usage,
    /// todo progress, background tasks, plan mode.
    Status,
    /// `/bashes` — list background tasks (`execute` with
    /// `run_in_background`), running and finished, with id/status/command.
    Bashes,
    /// `/goal [text]` — show the standing mission goal, or set it. `None`
    /// shows the current goal; `Some` sets it (drives sovereign/continuous
    /// mode), persisting to `<project_root>/.wizard/mission.toml`.
    Goal(Option<String>),
    /// `/publish [branch]` — fork Wizard and get a one-line installer.
    Publish {
        branch: Option<String>,
    },
    /// `/fusion [config]` — toggle model fusion (a panel of providers debate
    /// then a synthesizer answers), or open the panel configurator.
    Fusion(FusionAction),
    /// `/provider ...` — add, remove, or switch LLM providers.
    Provider(ProviderAction),
    /// Finalize an interactive provider setup: add the provider (storing the
    /// API key in `~/.wizard/credentials.toml` when present) and switch to it.
    /// Emitted internally by the inline prompt flow, never parsed from text —
    /// hence the primitive fields (so `SlashCommand` can stay `Eq`).
    ProviderSetup {
        name: String,
        kind: ProviderKind,
        base_url: String,
        model: String,
        api_key: Option<String>,
    },
    /// `/server ...` — status / start / stop the local llama-server.
    Server(ServerAction),
    /// `/login <provider>`: OAuth sign-in for providers that support it
    /// (currently `xai`).
    Login(String),
    /// `/settings` — open the in-app settings menu (a reusable picker).
    Settings,
    /// `/vim` — toggle modal (vim-style) editing of the input composer.
    Vim,
    /// Import the selected artifacts from Claude Code (`~/.claude/`). Not a
    /// typed command; dispatched from the `/settings` import picker, which is
    /// why it carries the [`ImportSelection`].
    ImportClaude(ImportSelection),
    Quit,
}

/// What a `/fusion` subcommand does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FusionAction {
    /// `/fusion` (no args) — toggle fusion mode on/off.
    Toggle,
    /// `/fusion config` — open the panel/synthesizer configurator.
    Config,
}

/// What a `/provider` subcommand does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderAction {
    /// `/provider` (no args) — open the interactive two-level picker (switch
    /// providers, or add a new one).
    Menu,
    /// `/provider list` — show configured providers.
    List,
    /// `/provider use <name>` — switch the active provider.
    Use(String),
    /// `/provider add <name> <kind> <base_url> <model> [API_KEY_ENV]`.
    Add {
        name: String,
        kind: ProviderKind,
        base_url: String,
        model: String,
        api_key_env: Option<String>,
    },
    /// `/provider remove <name>`.
    Remove(String),
}

/// Parse the arguments to `/provider` (everything after the command word).
fn parse_provider(args: &[&str]) -> Result<SlashCommand, String> {
    let action = match args.first().copied() {
        None => ProviderAction::Menu,
        Some("list") => ProviderAction::List,
        Some("use") => match args.get(1) {
            Some(name) => ProviderAction::Use((*name).to_string()),
            None => return Err("usage: /provider use <name>".to_string()),
        },
        Some("add") => {
            if args.len() < 5 {
                return Err(
                    "usage: /provider add <name> <llamacpp|ollama|openai|anthropic|openrouter|xai|xaioauth|cloudflare> <base_url> <model> [API_KEY_ENV]"
                        .to_string(),
                );
            }
            let kind = match args[2] {
                "llamacpp" => ProviderKind::LlamaCpp,
                "ollama" => ProviderKind::Ollama,
                "openai" => ProviderKind::Openai,
                "anthropic" => ProviderKind::Anthropic,
                "openrouter" => ProviderKind::OpenRouter,
                "xai" => ProviderKind::Xai,
                "xaioauth" => ProviderKind::XaiOauth,
                "cloudflare" => ProviderKind::Cloudflare,
                other => {
                    return Err(format!(
                        "unknown provider kind '{other}' (llamacpp|ollama|openai|anthropic|openrouter|xai|xaioauth|cloudflare)"
                    ));
                }
            };
            ProviderAction::Add {
                name: args[1].to_string(),
                kind,
                base_url: args[3].to_string(),
                model: args[4].to_string(),
                api_key_env: args.get(5).map(|s| s.to_string()),
            }
        }
        Some("remove") => match args.get(1) {
            Some(name) => ProviderAction::Remove((*name).to_string()),
            None => return Err("usage: /provider remove <name>".to_string()),
        },
        Some(other) => {
            return Err(format!(
                "unknown /provider subcommand '{other}' (list|use|add|remove)"
            ));
        }
    };
    Ok(SlashCommand::Provider(action))
}

/// Sentinel value of the final "add provider" row in the level-1 provider
/// picker. The dispatch also keys off the last index, but matching the value
/// keeps it robust if the list grows.
const PROVIDER_ADD_ROW: &str = "＋ Add provider…";

/// The level-2 provider-type menu: `(label, detail)` in dispatch order. The
/// Enter handler in [`App::handle_key`] matches on the row index, so this
/// order is the single source of truth for both rendering and dispatch.
const PROVIDER_TYPES: &[(&str, &str)] = &[
    ("xAI (Grok) — sign in", "OAuth · no API key"),
    (
        "xAI (Grok) — API key",
        "stored in ~/.wizard/credentials.toml",
    ),
    ("OpenRouter — API key", "openrouter.ai"),
    (
        "Cloudflare Workers AI — API token",
        "GLM 5.2 · account id + token",
    ),
    ("OpenAI — API key", "api.openai.com"),
    ("Anthropic (Claude) — API key", "api.anthropic.com"),
    ("OpenAI-compatible — custom", "any base URL + key"),
];

/// The `web_search` backend menu (`/settings`): `(id, label, detail)`. The id
/// is what gets written to `[web] search_backend` and (for keyed backends) the
/// `~/.wizard/credentials.toml` key name; the order is the display order.
const WEB_BACKENDS: &[(&str, &str, &str)] = &[
    ("duckduckgo", "DuckDuckGo", "free · no API key"),
    ("brave", "Brave Search", "API key · brave.com/search/api"),
    ("tavily", "Tavily", "API key · tavily.com"),
    ("exa", "Exa", "API key · exa.ai"),
    ("serper", "Serper (Google)", "API key · serper.dev"),
    ("xai", "xAI (Grok)", "sign in with xAI, or API key"),
];

/// Display label for a `web_search` backend id (falls back to the id itself).
fn web_backend_label(id: &str) -> &str {
    match id {
        "grok" => "xAI (Grok)",
        other => WEB_BACKENDS
            .iter()
            .find(|(value, _, _)| *value == other)
            .map(|(_, label, _)| *label)
            .unwrap_or(other),
    }
}

/// Whether a keyed `web_search` backend needs a pasted API key (vs DuckDuckGo,
/// which needs none, and xAI, which can use the OAuth session).
fn web_backend_needs_key(id: &str) -> bool {
    matches!(id, "brave" | "tavily" | "exa" | "serper")
}

/// Whether an xAI OAuth session already exists on disk (`wizard --login xai`),
/// so web search can reuse it without a fresh sign-in.
fn xai_oauth_session_present() -> bool {
    crate::llm::xai_oauth::token_path()
        .map(|path| path.exists())
        .unwrap_or(false)
}

/// Human-readable provider name for a kind, used in inline prompt questions.
fn provider_display(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Xai | ProviderKind::XaiOauth => "xAI",
        ProviderKind::ChatgptOauth => "ChatGPT",
        ProviderKind::OpenRouter => "OpenRouter",
        ProviderKind::Openai => "OpenAI-compatible",
        ProviderKind::Anthropic => "Anthropic",
        ProviderKind::Cloudflare => "Cloudflare",
        ProviderKind::LlamaCpp => "llama.cpp",
        ProviderKind::Ollama => "Ollama",
    }
}

/// The question shown when collecting `field` for the in-progress `prompt`.
fn prompt_question(field: PromptField, prompt: &ProviderPrompt) -> String {
    match field {
        PromptField::Name => "Provider name (id):".to_string(),
        PromptField::AccountId => {
            "Cloudflare account ID (dash.cloudflare.com → Workers AI → account id):".to_string()
        }
        PromptField::BaseUrl => "Base URL:".to_string(),
        PromptField::Model => "Model:".to_string(),
        PromptField::ApiKey => format!(
            "Paste your {} API key, then Enter (input hidden):",
            provider_display(prompt.kind)
        ),
    }
}

/// What a `/server` subcommand does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerAction {
    /// `/server` / `/server status` — health of the local llama-server.
    Status,
    /// `/server start` — start llama-server for the active provider.
    Start,
    /// `/server stop` — stop the llama-server Wizard started.
    Stop,
}

/// Parse the arguments to `/server` (everything after the command word).
fn parse_server(args: &[&str]) -> Result<SlashCommand, String> {
    let action = match args.first().copied() {
        None | Some("status") => ServerAction::Status,
        Some("start") => ServerAction::Start,
        Some("stop") => ServerAction::Stop,
        Some(other) => {
            return Err(format!(
                "unknown /server subcommand '{other}' (status|start|stop)"
            ));
        }
    };
    Ok(SlashCommand::Server(action))
}

impl SlashCommand {
    /// Parse a `/...` input line. `None` when `input` is not a slash
    /// command; `Some(Err(msg))` for an unknown command or bad arguments.
    pub fn parse(input: &str) -> Option<Result<Self, String>> {
        let input = input.trim();
        let rest = input.strip_prefix('/')?;
        let mut parts = rest.split_whitespace();
        let command = parts.next().unwrap_or("");
        let args: Vec<&str> = parts.collect();

        let parsed = match command {
            "help" => Ok(Self::Help),
            "clear" => Ok(Self::Clear),
            "model" => Ok(Self::Model(args.first().map(|s| s.to_string()))),
            "mode" => match args.first() {
                None => Ok(Self::Mode(None)),
                Some(&"genie") => Ok(Self::Mode(Some(Mode::Genie))),
                Some(&"sovereign") => Ok(Self::Mode(Some(Mode::Sovereign))),
                Some(other) => Err(format!("unknown mode '{other}' (genie|sovereign)")),
            },
            "genie" => Ok(Self::Mode(Some(Mode::Genie))),
            "sovereign" => Ok(Self::Mode(Some(Mode::Sovereign))),
            "effort" => match args.first().map(|s| s.to_ascii_lowercase()).as_deref() {
                None => Ok(Self::Effort(None)),
                Some("low") => Ok(Self::Effort(Some(Some(ReasoningEffort::Low)))),
                Some("medium") | Some("med") => {
                    Ok(Self::Effort(Some(Some(ReasoningEffort::Medium))))
                }
                Some("high") => Ok(Self::Effort(Some(Some(ReasoningEffort::High)))),
                Some("default") | Some("off") | Some("none") => Ok(Self::Effort(Some(None))),
                Some(other) => Err(format!(
                    "unknown effort '{other}' (low|medium|high|default)"
                )),
            },
            "evolve" => {
                let deep = args.first() == Some(&"--deep");
                let description = if deep { &args[1..] } else { &args[..] }.join(" ");
                if description.is_empty() {
                    Err("usage: /evolve [--deep] <what to add>".to_string())
                } else {
                    Ok(Self::Evolve { deep, description })
                }
            }
            "reload" => Ok(Self::Reload),
            "plan" => Ok(Self::Plan),
            "omakase" => Ok(Self::Omakase),
            "rewind" => match args.first() {
                None => Ok(Self::Rewind(None)),
                Some(arg) => arg
                    .parse::<u64>()
                    .map(|turn| Self::Rewind(Some(turn)))
                    .map_err(|_| "usage: /rewind [turn]".to_string()),
            },
            "resume" => Ok(Self::Resume(args.first().map(|s| s.to_string()))),
            "compact" => Ok(Self::Compact),
            "agents" => Ok(Self::Agents),
            "subagents" => Ok(Self::Subagents),
            "diff" => Ok(Self::Diff),
            "todos" => Ok(Self::Todos),
            "dashboard" => Ok(Self::Dashboard),
            "cost" => Ok(Self::Cost),
            "memory" => Ok(Self::Memory),
            "doctor" => Ok(Self::Doctor),
            "status" => Ok(Self::Status),
            "bashes" => Ok(Self::Bashes),
            "goal" => {
                let text = args.join(" ");
                if text.is_empty() {
                    Ok(Self::Goal(None))
                } else {
                    Ok(Self::Goal(Some(text)))
                }
            }
            "publish" => Ok(Self::Publish {
                branch: args.first().map(|s| s.to_string()),
            }),
            "provider" => parse_provider(&args),
            "fusion" => match args.first().copied() {
                None => Ok(Self::Fusion(FusionAction::Toggle)),
                Some("config") => Ok(Self::Fusion(FusionAction::Config)),
                Some(other) => Err(format!(
                    "unknown /fusion subcommand '{other}' — use /fusion or /fusion config"
                )),
            },
            "server" => parse_server(&args),
            "login" => match args.first() {
                Some(provider) => Ok(Self::Login((*provider).to_string())),
                None => Err("usage: /login xai".to_string()),
            },
            "settings" => Ok(Self::Settings),
            "vim" => Ok(Self::Vim),
            "quit" | "q" | "exit" => Ok(Self::Quit),
            other => Err(format!("unknown command '/{other}' — try /help")),
        };
        Some(parsed)
    }

    /// Whether the agent may invoke this command itself (via the `run_command`
    /// tool), and if not, the reason to report back to the model.
    ///
    /// Allowed: read-only status/info commands and state changes the agent can
    /// sensibly apply to its own session (effort, model, mode, goal, planning
    /// modes, reload, compact, and the UI toggles). Refused: commands that need
    /// a human at an interactive picker (the no-argument forms), that end or
    /// rewind the session, or that reach outside it to set up providers.
    pub fn agent_runnable(&self) -> Result<(), String> {
        use SlashCommand::*;
        match self {
            // State the agent can set on itself, plus read-only info toggles.
            Model(Some(_))
            | Mode(Some(_))
            | Effort(Some(_))
            | Goal(_)
            | Diff
            | Todos
            | Subagents
            | Dashboard
            | Cost
            | Memory
            | Doctor
            | Status
            | Bashes
            | Compact
            | Reload
            | Plan
            | Omakase
            | Settings
            | Vim
            | Help
            | Fusion(FusionAction::Toggle) => Ok(()),

            // Interactive pickers: there is no human at the keyboard mid-turn,
            // so require the argument that names the choice directly.
            Model(None) => Err("name a model, e.g. `/model claude-sonnet-5`".into()),
            Mode(None) => Err("name a mode, e.g. `/mode sovereign`".into()),
            Effort(None) => Err("name a level, e.g. `/effort high`".into()),
            Fusion(FusionAction::Config) => {
                Err("`/fusion config` opens an interactive editor; use `/fusion` to toggle".into())
            }
            Agents => Err(
                "`/agents` opens a picker for the user; spawn subagents with the spawn tool".into(),
            ),

            // Session-ending, destructive, or external-setup commands are off
            // limits to the agent.
            Quit => Err("refusing to quit the session on the user's behalf".into()),
            Clear => Err("refusing to clear the conversation on the user's behalf".into()),
            Rewind(_) => Err("`/rewind` restores checkpoints and is the user's call".into()),
            Resume(_) => Err("`/resume` switches sessions and is the user's call".into()),
            Evolve { .. } => {
                Err("`/evolve` is a heavyweight self-edit; leave it to the user".into())
            }
            Publish { .. } => Err("`/publish` forks the tool; leave it to the user".into()),
            Provider(_) | ProviderSetup { .. } => {
                Err("provider setup is the user's call; use `/model` to switch models".into())
            }
            Server(_) => {
                Err("`/server` manages the local model server; leave it to the user".into())
            }
            Login(_) => Err("`/login` is an interactive sign-in; leave it to the user".into()),
            ImportClaude(_) => {
                Err("`/settings` import is driven from a picker; leave it to the user".into())
            }
        }
    }
}

/// One entry in the slash-command completion table. Drives the suggestion
/// popup and the inline ghost-text prediction.
#[derive(Debug)]
pub struct CommandSpec {
    pub name: &'static str,
    /// Argument hint shown after the name (e.g. `[tag]`).
    pub args: &'static str,
    pub description: &'static str,
    /// Completion appends a trailing space and waits for arguments instead
    /// of submitting immediately.
    pub takes_args: bool,
}

/// All slash commands, in display order.
pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "model",
        args: "[tag]",
        description: "pick or switch the model",
        takes_args: false,
    },
    CommandSpec {
        name: "mode",
        args: "[genie|sovereign]",
        description: "pick or switch personality mode",
        takes_args: false,
    },
    CommandSpec {
        name: "genie",
        args: "",
        description: "switch to genie mode",
        takes_args: false,
    },
    CommandSpec {
        name: "sovereign",
        args: "",
        description: "switch to sovereign mode",
        takes_args: false,
    },
    CommandSpec {
        name: "effort",
        args: "[low|medium|high|default]",
        description: "set reasoning effort (Grok 4.x, OpenAI o-series / gpt-5)",
        takes_args: false,
    },
    CommandSpec {
        name: "plan",
        args: "",
        description: "toggle plan mode: read-only until a plan is approved",
        takes_args: false,
    },
    CommandSpec {
        name: "omakase",
        args: "",
        description: "toggle omakase: chef's-choice plan mode, the agent decides",
        takes_args: false,
    },
    CommandSpec {
        name: "rewind",
        args: "[turn]",
        description: "rewind files and conversation to before a turn",
        takes_args: false,
    },
    CommandSpec {
        name: "resume",
        args: "",
        description: "reopen and continue a past session",
        takes_args: false,
    },
    CommandSpec {
        name: "compact",
        args: "",
        description: "summarize older history into a progress note now",
        takes_args: false,
    },
    CommandSpec {
        name: "agents",
        args: "",
        description: "browse subagents and delegate to one",
        takes_args: false,
    },
    CommandSpec {
        name: "subagents",
        args: "",
        description: "monitor the subagents running in this session",
        takes_args: false,
    },
    CommandSpec {
        name: "evolve",
        args: "[--deep] <desc>",
        description: "self-extend: add a skill, tool, or MCP server",
        takes_args: true,
    },
    CommandSpec {
        name: "publish",
        args: "[branch]",
        description: "fork & publish your Wizard, get a one-line installer",
        takes_args: false,
    },
    CommandSpec {
        name: "provider",
        args: "",
        description: "add or switch LLM providers (interactive)",
        takes_args: false,
    },
    CommandSpec {
        name: "fusion",
        args: "[config]",
        description: "toggle model fusion, or configure the panel",
        takes_args: false,
    },
    CommandSpec {
        name: "server",
        args: "[status|start|stop]",
        description: "manage the local llama-server",
        takes_args: false,
    },
    CommandSpec {
        name: "login",
        args: "<xai>",
        description: "sign in to a provider account (xAI OAuth)",
        takes_args: true,
    },
    CommandSpec {
        name: "diff",
        args: "",
        description: "toggle the git diff sidebar",
        takes_args: false,
    },
    CommandSpec {
        name: "todos",
        args: "",
        description: "toggle the todo side panel",
        takes_args: false,
    },
    CommandSpec {
        name: "dashboard",
        args: "",
        description: "session manager: all live wizard sessions on this machine",
        takes_args: false,
    },
    CommandSpec {
        name: "cost",
        args: "",
        description: "show session token usage and cost",
        takes_args: false,
    },
    CommandSpec {
        name: "memory",
        args: "",
        description: "show saved project memories",
        takes_args: false,
    },
    CommandSpec {
        name: "status",
        args: "",
        description: "show session status: model, usage, todos, tasks",
        takes_args: false,
    },
    CommandSpec {
        name: "bashes",
        args: "",
        description: "list background tasks: id, status, command",
        takes_args: false,
    },
    CommandSpec {
        name: "goal",
        args: "[text]",
        description: "show or set the standing mission goal",
        takes_args: false,
    },
    CommandSpec {
        name: "settings",
        args: "",
        description: "open the settings menu (change config anytime)",
        takes_args: false,
    },
    CommandSpec {
        name: "vim",
        args: "",
        description: "toggle vim-style modal editing of the input line",
        takes_args: false,
    },
    CommandSpec {
        name: "doctor",
        args: "",
        description: "diagnose config, providers, MCP, hooks, state dirs",
        takes_args: false,
    },
    CommandSpec {
        name: "reload",
        args: "",
        description: "reload skills, scripted tools, and MCP servers",
        takes_args: false,
    },
    CommandSpec {
        name: "clear",
        args: "",
        description: "clear the conversation",
        takes_args: false,
    },
    CommandSpec {
        name: "help",
        args: "",
        description: "show available commands and keys",
        takes_args: false,
    },
    CommandSpec {
        name: "quit",
        args: "",
        description: "exit wizard",
        takes_args: false,
    },
    CommandSpec {
        name: "exit",
        args: "",
        description: "exit wizard",
        takes_args: false,
    },
];

/// One row in the suggestion popup: a builtin [`CommandSpec`] or a custom
/// command loaded from `~/.wizard/commands/` / `<project>/.wizard/commands/`.
#[derive(Debug, Clone)]
pub struct Suggestion {
    pub name: String,
    /// Argument hint shown after the name.
    pub args: String,
    pub description: String,
    /// Completion appends a trailing space and waits for arguments instead
    /// of submitting immediately.
    pub takes_args: bool,
}

impl From<&CommandSpec> for Suggestion {
    fn from(spec: &CommandSpec) -> Self {
        Self {
            name: spec.name.to_string(),
            args: spec.args.to_string(),
            description: spec.description.to_string(),
            takes_args: spec.takes_args,
        }
    }
}

impl From<&CustomCommand> for Suggestion {
    fn from(command: &CustomCommand) -> Self {
        let takes_args = command.expects_args();
        Self {
            name: command.name.clone(),
            args: if takes_args {
                "[args]".to_string()
            } else {
                String::new()
            },
            description: command
                .description
                .clone()
                .unwrap_or_else(|| "custom command".to_string()),
            takes_args,
        }
    }
}

/// True when `name` is a builtin command word ([`COMMANDS`] plus the parse
/// aliases that have no table entry). Unknown words fall through to the
/// model as a normal prompt.
fn is_builtin_command(name: &str) -> bool {
    COMMANDS.iter().any(|spec| spec.name == name) || matches!(name, "q" | "exit")
}

/// What an open [`Picker`] selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    Model,
    Mode,
    /// Reasoning-effort level (item values are `low`/`medium`/`high`/`default`).
    Effort,
    /// A turn to rewind to (item values are turn ids).
    Rewind,
    /// A past session to resume (item values are session ids).
    Resume,
    /// A subagent to delegate to (item values are subagent names). Selecting
    /// one pre-fills the input with a delegation request rather than running a
    /// command, since subagents are invoked by the model, not directly.
    Subagent,
    /// The settings menu. Rows are dispatched by index against
    /// [`App::settings_rows`]; toggles mutate config inline and re-open.
    Settings,
    /// "Import from Claude Code": a multi-select where Space toggles a row
    /// (MCP servers / commands / spinner verbs) and Enter runs the import.
    ClaudeImport,
    /// Level 1 of `/provider`: configured providers (Enter switches) plus a
    /// final "add provider" row that opens [`PickerKind::ProviderType`].
    Provider,
    /// Level 2 of `/provider`: the menu of provider kinds to add. Rows are
    /// dispatched by index against [`PROVIDER_TYPES`].
    ProviderType,
    /// The `web_search` backend picker (from `/settings`). Item values are
    /// backend ids ([`WEB_BACKENDS`]); selecting a keyed backend starts an
    /// inline API-key prompt, xAI reuses the OAuth session, DuckDuckGo applies
    /// immediately.
    WebBackend,
    /// `/fusion config`: a multi-select where Space toggles a provider into the
    /// fusion panel and Enter saves `[fusion]` (the first toggled row becomes
    /// the synthesizer). Reuses [`PickerItem::current`] as the checkbox.
    FusionPanel,
}

/// One selectable row in a picker popup.
#[derive(Debug)]
pub struct PickerItem {
    /// Value applied on selection (model tag / mode name).
    pub value: String,
    /// Secondary text shown dimmed next to the value.
    pub detail: String,
    /// Marks the currently active item.
    pub current: bool,
}

/// An interactive selection popup (↑/↓ to move, Enter to select, Esc to
/// cancel).
#[derive(Debug)]
pub struct Picker {
    pub kind: PickerKind,
    pub title: String,
    pub items: Vec<PickerItem>,
    pub selected: usize,
}

impl Picker {
    /// Footer hint shown along the modal's bottom border. The Claude-import
    /// picker is a multi-select (Space toggles, Enter runs), so it needs a
    /// different hint than the Enter-to-select pickers.
    pub fn footer_hint(&self) -> &'static str {
        match self.kind {
            PickerKind::ClaudeImport => " ↑↓ move · space toggles · enter runs · Esc cancel ",
            PickerKind::FusionPanel => " ↑↓ move · space toggles · enter saves · Esc cancel ",
            _ => " ↑↓ move · Enter select · Esc cancel ",
        }
    }
}

/// In-flight plan review (plan mode): the model called `exit_plan` and the
/// turn is paused inside the tool until a [`PlanVerdict`] is sent back.
#[derive(Debug)]
pub struct PlanReview {
    /// The plan markdown, rendered in the review modal.
    pub plan: String,
    /// Verdict channel back into the paused `exit_plan` call; taken exactly
    /// once when the review finishes.
    respond: Option<tokio::sync::oneshot::Sender<PlanVerdict>>,
    /// `Some` while collecting rejection feedback (the text typed so far).
    pub feedback: Option<String>,
    /// Scroll offset from the top of the plan.
    pub scroll: u16,
}

/// In-flight interview (plan mode): the model called `interview` and the turn
/// is paused inside the tool until the user answers every question or
/// dismisses the modal.
#[derive(Debug)]
pub struct Interview {
    /// The questions, in order.
    pub questions: Vec<InterviewQuestion>,
    /// Answers collected so far, one per answered question (parallel to
    /// `questions[..current]`).
    pub answers: Vec<String>,
    /// Index of the question currently being answered.
    pub current: usize,
    /// The answer being typed for the current question.
    pub input: String,
    /// Answer channel back into the paused `interview` call; taken exactly
    /// once when the interview finishes (`Some(answers)`) or is dismissed
    /// (`None`).
    respond: Option<tokio::sync::oneshot::Sender<Option<Vec<String>>>>,
}

impl Interview {
    /// The question now being answered, if any remain.
    pub fn current_question(&self) -> Option<&InterviewQuestion> {
        self.questions.get(self.current)
    }
}

/// Status bar contents.
#[derive(Debug, Default)]
pub struct StatusLine {
    pub model: String,
    pub mode: Mode,
    /// Current step within the running turn (0 when idle).
    pub step: u32,
    /// The turn's step budget — unlimited unless `max_steps` is configured.
    pub max_steps: StepBudget,
    /// True while a turn is streaming.
    pub busy: bool,
    /// Session prompt-token total (from [`AgentEvent::Usage`]). Used by
    /// `/cost` for lifetime session usage / estimated spend — *not* the
    /// status-bar context meter.
    pub prompt_tokens: u64,
    /// Session completion-token total.
    pub completion_tokens: u64,
    /// Tokens that will load into context on the next model call (last
    /// reported prompt size, or a post-compact / post-clear estimate).
    /// This is what the status bar displays.
    pub context_tokens: u64,
    /// Background tasks (`execute` with `run_in_background`) still running.
    pub background_tasks: usize,
    /// Backgrounded subagents (`spawn_subagent` with `background: true`)
    /// still running.
    pub background_subagents: usize,
}

/// A mouse text selection over the rendered screen. Coordinates are absolute
/// terminal cells. Because wizard captures the mouse (so the wheel scrolls the
/// transcript), the terminal's own click-drag-to-select is pre-empted — so the
/// app draws the highlight itself ([`crate::ui`]) and copies the covered cells
/// to the clipboard via OSC 52 on release.
#[derive(Debug, Clone, Copy)]
pub struct Selection {
    /// Cell where the drag began (mouse-down).
    pub anchor: (u16, u16),
    /// Cell under the cursor now: tracks the drag, frozen on release.
    pub head: (u16, u16),
    /// True while the button is held down.
    pub dragging: bool,
}

impl Selection {
    /// The endpoints in reading order: `(start, end)` such that `start`
    /// precedes `end` row-major (top-to-bottom, then left-to-right).
    pub fn ordered(&self) -> ((u16, u16), (u16, u16)) {
        // Compare by (row, column) so a point lower on screen always sorts last.
        let key = |(x, y): (u16, u16)| (y, x);
        if key(self.anchor) <= key(self.head) {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// A click that never dragged: anchor and head are the same cell, so there
    /// is nothing to highlight or copy.
    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }
}

/// Full TUI state. [`crate::ui::draw`] renders it; [`App::handle_event`]
/// mutates it.
#[derive(Debug)]
pub struct App {
    pub config: Config,
    pub mode: Mode,
    pub input: String,
    /// Cursor position in `input`, in characters.
    pub cursor: usize,
    pub input_mode: InputMode,
    /// Modal (vim-style) editing state for the composer. Inert (always
    /// insert-like) unless `vim.enabled`.
    pub vim: VimState,
    pub transcript: Vec<TranscriptEntry>,
    /// Partial assistant text of the in-flight turn (moved into the
    /// transcript when the turn ends).
    pub streaming: String,
    /// Partial model reasoning of the in-flight turn, rendered dimmed and
    /// flushed to the transcript alongside `streaming`.
    pub streaming_thinking: String,
    pub status: StatusLine,
    /// Latched once the user submits anything — a slash command dispatches
    /// without adding transcript entries, so `has_conversation` alone would
    /// leave the welcome screen up after it.
    pub welcome_dismissed: bool,
    /// Git diff sidebar visibility and cached contents.
    pub show_diff: bool,
    pub diff_text: String,
    /// Scroll offset (in lines, from the top) into the diff sidebar. Held
    /// here so PgUp/PgDn can page a diff that's taller than the pane; the
    /// renderer clamps it to the content height.
    pub diff_scroll: u16,
    /// Todo side panel visibility (toggled by `/todos`; auto-shown on the
    /// first todo update of the session).
    pub show_todos: bool,
    /// The agent's current todo list, mirrored from
    /// [`AgentEvent::TodoUpdated`].
    pub todos: Vec<TodoItem>,
    /// Whether a todo update has arrived yet (drives the one-time
    /// auto-show).
    todos_seen: bool,
    /// Full-screen agent dashboard visibility (toggled by `/dashboard`).
    pub show_dashboard: bool,
    /// Every subagent run this session, oldest first — the rail below the
    /// composer. Fed by the `AgentEvent::SubagentRun*` events.
    pub panes: Vec<SubagentPane>,
    /// Background-subagent registry, so the rail can kill a detached run even
    /// while a turn holds the agent. `None` until the agent is built.
    pub subagents: Option<Arc<crate::tools::subagent_tasks::SubagentTaskRegistry>>,
    /// Selected rail row while the rail has keyboard focus (↓ from the
    /// composer). `None` means the composer has focus and the rail is just
    /// on display. Indexes [`App::panes`].
    pub rail_focus: Option<usize>,
    /// The pane the user is *inside*: its transcript replaces the main chat
    /// until Esc. Indexes [`App::panes`].
    pub attached: Option<usize>,
    /// This session's id (heartbeat filename + dashboard identity).
    pub session_id: String,
    /// This session's display name (from the first prompt, or the id).
    pub session_name: String,
    /// Unix start time, stamped once at registration.
    pub session_started_unix: u64,
    /// Live sessions on the machine, refreshed from the registry while the
    /// dashboard is open.
    pub sessions: Vec<SessionRecord>,
    /// Armed by a first Ctrl-C; a second one exits. Disarmed by any other key.
    pub ctrl_c_armed: bool,
    /// Selected row in the dashboard list.
    pub dashboard_selected: usize,
    /// Dispatch input at the bottom of the dashboard (the prompt for a new
    /// background session).
    pub dashboard_input: String,
    /// Recent transcript of the selected session (role, text), shown in the
    /// dashboard's peek panel; refreshed as the selection moves.
    pub peek_lines: Vec<(String, String)>,
    /// First visible line of the main transcript, measured from the top of the
    /// rendered content. Only consulted while [`Self::scroll_follow`] is false;
    /// when following, the live tail is always in view.
    pub scroll: u16,
    /// When true the transcript sticks to the bottom as new output streams in.
    /// Scrolling up (wheel, PgUp) clears it so the viewport stays put; scrolling
    /// back down to the bottom, Esc, or Ctrl-End restores follow.
    pub scroll_follow: bool,
    /// Last-drawn max scroll for the main transcript (content lines past the
    /// viewport). Written by [`crate::ui::draw`] so key handlers can turn a
    /// follow-tail view into a stable top-anchored offset without re-wrapping.
    pub transcript_max_scroll: std::cell::Cell<u16>,
    /// Active or just-completed mouse text selection, if any. Drives the
    /// highlight overlay and clipboard copy.
    pub selection: Option<Selection>,
    /// Screen rows of tool-card header lines visible in the last-drawn frame,
    /// as `(row, transcript index)` — the click-to-toggle hit map. Rebuilt by
    /// [`crate::ui::draw`] every frame (hence the interior mutability: draw
    /// takes `&App`) and emptied while an overlay covers the transcript.
    pub card_hits: std::cell::RefCell<Vec<(u16, usize)>>,
    /// What this terminal can draw an image with, and every image it has drawn
    /// recently. Starts at the half-block floor so a frame can be rendered
    /// before anything has asked the terminal; `run_tui` replaces it with
    /// [`ImageCache::detect`] before it takes the screen. Interior mutability
    /// for the same reason as `card_hits` — draw takes `&App`, and decoding a
    /// PNG once per image is exactly what a cache is for.
    pub images: std::cell::RefCell<ImageCache>,
    pub should_quit: bool,
    /// Tick counter driving the busy spinner.
    pub tick: u64,
    /// Matching commands (builtin [`COMMANDS`] plus custom commands) for the
    /// current `/input`, shown as the suggestion popup.
    pub suggestions: Vec<Suggestion>,
    /// Highlighted row in `suggestions`.
    pub suggestion_index: usize,
    /// Custom commands loaded from `~/.wizard/commands/` and
    /// `<project>/.wizard/commands/` (set by `run_tui`, refreshed by
    /// `/reload`).
    pub custom_commands: Vec<CustomCommand>,
    /// Project root `@file` references resolve against.
    pub project_root: PathBuf,
    /// Open selection popup (model / mode / rewind / subagent picker), if any.
    pub picker: Option<Picker>,
    /// In-progress interactive provider setup, when the composer is collecting
    /// fields ([`InputMode::Prompt`]).
    pub prompt: Option<ProviderPrompt>,
    /// When the composer is collecting a pasted API key for a keyed
    /// `web_search` backend (the backend name); set from the `/settings` web
    /// search picker, consumed by [`App::submit_web_key`].
    pub web_key_backend: Option<String>,
    /// Image files staged for the next submit (from paste of image paths or
    /// `data:image/...;base64,...` blobs). Merged with `@file` image refs on
    /// submit, then cleared.
    pub pending_images: Vec<PathBuf>,
    /// Whether plan mode is active (mirrors the agent's flag for the status
    /// bar; toggled by `/plan` and Shift+Tab).
    pub plan_mode: bool,
    /// Whether omakase (chef's-choice) mode is active (mirrors the agent's
    /// flag; toggled by `/omakase`). Implies `plan_mode`.
    pub omakase: bool,
    /// Whether the active client is a [`FusionProvider`](crate::llm::fusion)
    /// (`/fusion` toggled on). Drives the loud status-bar indicator and lets
    /// `/fusion` toggle back to the underlying single provider.
    pub fusion_active: bool,
    /// Open plan-review modal (the turn is paused inside `exit_plan` until
    /// it resolves), if any.
    pub plan_review: Option<PlanReview>,
    /// Open interview modal (the turn is paused inside the `interview` tool
    /// until the user answers or dismisses), if any.
    pub interview: Option<Interview>,
    /// Previously submitted inputs, oldest first (↑/↓ recall).
    pub history: Vec<String>,
    /// Position while browsing `history`; `None` when composing fresh input.
    history_pos: Option<usize>,
    /// The in-progress input saved when history browsing starts.
    history_draft: String,
    /// When the in-flight turn started (drives the elapsed-time display).
    pub turn_started: Option<Instant>,
    /// Label of an in-progress background agent rebuild (model switch,
    /// crash recovery); rendered as a spinner in the status bar. Input that
    /// needs the agent is rejected with a notice while this is `Some`.
    pub rebuilding: Option<String>,
    /// Verb shown next to the busy spinner ("Conjuring…"); re-rolled at the
    /// start of each busy period by [`App::roll_spinner_verb`].
    pub spinner_verb: String,
    /// Number of verb rolls so far, mixed into the roll seed so back-to-back
    /// turns starting on the same tick still draw fresh verbs.
    verb_rolls: u64,
    /// Set by the `/settings` "Open config file" row; the main loop (which owns
    /// the terminal) suspends the TUI, opens `$EDITOR` on the config file, then
    /// reloads config. Cleared once handled.
    pub pending_edit_config: bool,
    /// Set by Ctrl-G; the main loop suspends the TUI, opens the composer draft
    /// in `$EDITOR`, and reads the result back. Cleared once handled.
    pub pending_edit_prompt: bool,
    /// Set by `/compact`; the main loop takes the agent and runs compaction in
    /// the background. Cleared once the task is spawned.
    pub pending_compact: bool,
    /// True while a background `/compact` is running: the status bar shows an
    /// animated progress bar instead of its usual contents.
    pub compacting: bool,
    /// Set when the background MCP connect finishes while a turn is running
    /// (so the agent is out of its slot and can't take the rebuilt registry
    /// yet). The main loop merges the MCP tools once the turn returns the
    /// agent. Cleared then.
    pub mcp_merge_pending: bool,
    /// Slash commands the agent asked to run via the `run_command` tool during
    /// the current turn (raw command lines, e.g. `/effort high`). A turn in
    /// flight cannot be reconfigured, so the main loop drains and dispatches
    /// these once the turn ends and the agent is back in its slot.
    pub pending_agent_commands: Vec<String>,
    /// True while the background MCP connect is in flight (servers spawning,
    /// `initialize` round-trips). Drives a transient status-bar indicator so a
    /// message sent before the tools arrive isn't a silent surprise. Cleared on
    /// the no-servers early-return and once the connect finishes or fails.
    pub mcp_connecting: bool,
    /// Set by the deferred cloud-provider health probe when it fails, so the
    /// breakage is visible at launch (welcome screen + status bar) instead of
    /// only on the first message. Cleared once a turn completes successfully —
    /// the provider has proven itself, so a transient blip self-heals.
    pub provider_health_error: Option<String>,
}

impl App {
    pub fn new(config: Config) -> Self {
        let mode = config.mode;
        // Omakase implies plan mode (the read-only exploration phase).
        let omakase = config.omakase;
        let plan_mode = config.plan_first || omakase;
        let spinner_verb = config.ui.spinner_verb(0).to_string();
        // Vim mode starts in Insert so typing works immediately; `Esc` drops
        // to Normal.
        let vim = VimState {
            enabled: config.ui.vim,
            ..VimState::default()
        };
        let status = StatusLine {
            model: config.active().model,
            mode,
            step: 0,
            max_steps: config.max_steps,
            busy: false,
            prompt_tokens: 0,
            completion_tokens: 0,
            context_tokens: 0,
            background_tasks: 0,
            background_subagents: 0,
        };
        Self {
            config,
            mode,
            input: String::new(),
            cursor: 0,
            input_mode: InputMode::default(),
            vim,
            transcript: Vec::new(),
            streaming: String::new(),
            streaming_thinking: String::new(),
            status,
            welcome_dismissed: false,
            show_diff: false,
            diff_text: String::new(),
            diff_scroll: 0,
            show_todos: false,
            todos: Vec::new(),
            todos_seen: false,
            show_dashboard: false,
            panes: Vec::new(),
            subagents: None,
            rail_focus: None,
            attached: None,
            session_id: String::new(),
            session_name: String::new(),
            session_started_unix: 0,
            sessions: Vec::new(),
            ctrl_c_armed: false,
            dashboard_selected: 0,
            dashboard_input: String::new(),
            peek_lines: Vec::new(),
            scroll: 0,
            scroll_follow: true,
            transcript_max_scroll: std::cell::Cell::new(0),
            selection: None,
            card_hits: std::cell::RefCell::new(Vec::new()),
            images: std::cell::RefCell::new(ImageCache::fallback()),
            should_quit: false,
            tick: 0,
            suggestions: Vec::new(),
            suggestion_index: 0,
            custom_commands: Vec::new(),
            project_root: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            picker: None,
            prompt: None,
            web_key_backend: None,
            pending_images: Vec::new(),
            plan_mode,
            omakase,
            fusion_active: false,
            plan_review: None,
            interview: None,
            history: Vec::new(),
            history_pos: None,
            history_draft: String::new(),
            turn_started: None,
            rebuilding: None,
            spinner_verb,
            verb_rolls: 0,
            pending_edit_config: false,
            pending_edit_prompt: false,
            pending_compact: false,
            compacting: false,
            mcp_merge_pending: false,
            pending_agent_commands: Vec::new(),
            mcp_connecting: false,
            provider_health_error: None,
        }
    }

    /// True while the home screen should remain up: the conversation hasn't
    /// begun. Early system notices (e.g. a provider-health warning) land in the
    /// transcript before the user sends anything; those alone shouldn't dismiss
    /// the opening screen, so only non-`Notice` entries count as conversation.
    pub fn has_conversation(&self) -> bool {
        self.transcript
            .iter()
            .any(|entry| !matches!(entry, TranscriptEntry::Notice(_)))
    }

    /// True while the welcome screen should render: the conversation hasn't
    /// begun, nothing was ever submitted (a slash command counts even though
    /// it adds no transcript entries), and no turn is in flight.
    pub fn welcome_visible(&self) -> bool {
        !self.has_conversation()
            && !self.welcome_dismissed
            && self.streaming.is_empty()
            && !self.status.busy
    }

    /// Pick a fresh spinner verb for a new busy period. The verb stays fixed
    /// until the next roll, so one turn reads as one activity.
    pub fn roll_spinner_verb(&mut self) {
        self.verb_rolls = self.verb_rolls.wrapping_add(1);
        let seed = self.tick.wrapping_add(self.verb_rolls);
        self.spinner_verb = self.config.ui.spinner_verb(seed).to_string();
    }

    /// Append a system notice to the transcript.
    pub fn notice(&mut self, message: impl Into<String>) {
        self.transcript
            .push(TranscriptEntry::Notice(message.into()));
    }

    /// The settings menu rows, in display order: `(action id, label, current
    /// value)`. [`open_settings_picker`](Self::open_settings_picker) renders
    /// the label/value and [`apply_setting`](Self::apply_setting) dispatches by
    /// the row index, so both share this single ordered source of truth.
    ///
    /// Numeric/list fields (`max_steps`, retry/compaction knobs, spinner verbs,
    /// gateway, …) are intentionally absent — the overlay has no text input, so
    /// they live behind the "Open config file" row.
    fn settings_rows(&self) -> Vec<(&'static str, String, String)> {
        let on = |b: bool| if b { "on" } else { "off" }.to_string();
        let providers = self.config.providers.len();
        let import_detail = if import_claude::claude_home().is_some() {
            "MCP servers, commands, spinner verbs".to_string()
        } else {
            "no ~/.claude found".to_string()
        };
        let config_path = Config::path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "~/.wizard/config.toml".to_string());
        vec![
            ("model", "Model".to_string(), self.config.active().model),
            ("mode", "Mode".to_string(), self.mode.to_string()),
            (
                "plan_first",
                "Plan mode at startup".to_string(),
                on(self.config.plan_first),
            ),
            (
                "continuous",
                "Continuous (sovereign)".to_string(),
                on(self.config.continuous),
            ),
            (
                "plan_each_cycle",
                "Plan each cycle".to_string(),
                on(self.config.plan_each_cycle),
            ),
            (
                "rollback",
                "Rollback failed cycles".to_string(),
                on(self.config.rollback_failed_cycles),
            ),
            (
                "vim",
                "Vim mode (modal input)".to_string(),
                on(self.config.ui.vim),
            ),
            (
                "web_backend",
                "Web search backend".to_string(),
                self.config.web.search_backend.clone(),
            ),
            (
                "web_allow_local",
                "Web: allow localhost".to_string(),
                on(self.config.web.allow_local),
            ),
            (
                "fleet_synthesize",
                "Fleet: synthesis turn".to_string(),
                on(self.config.fleet.synthesize),
            ),
            (
                "import",
                "Import from Claude Code".to_string(),
                import_detail,
            ),
            (
                "provider",
                "Manage providers…".to_string(),
                format!("{providers} configured"),
            ),
            ("config_file", "Open config file…".to_string(), config_path),
        ]
    }

    /// Open the `/settings` menu as a [`Picker`]. Re-callable: toggles re-open
    /// it so the new value is visible.
    pub fn open_settings_picker(&mut self) {
        let items: Vec<PickerItem> = self
            .settings_rows()
            .into_iter()
            .map(|(_, label, detail)| PickerItem {
                value: label,
                detail,
                current: false,
            })
            .collect();
        self.picker = Some(Picker {
            kind: PickerKind::Settings,
            title: " settings · ↑/↓ move · enter select · esc close ".to_string(),
            items,
            selected: 0,
        });
    }

    /// Dispatch the settings row at `selected`. Routing rows return an
    /// [`AppAction`] to run a command; inline toggle/cycle rows mutate config,
    /// persist, and re-open the menu (keeping the cursor on the same row).
    fn apply_setting(&mut self, selected: usize) -> Option<AppAction> {
        let rows = self.settings_rows();
        let (id, _, _) = rows.get(selected)?;
        match *id {
            "model" => return Some(AppAction::Command(SlashCommand::Model(None))),
            "mode" => return Some(AppAction::Command(SlashCommand::Mode(None))),
            "provider" => {
                return Some(AppAction::Command(SlashCommand::Provider(
                    ProviderAction::List,
                )));
            }
            "import" => {
                self.open_claude_import_picker();
                return None;
            }
            "config_file" => {
                // Handled by the main loop, which owns the terminal.
                self.pending_edit_config = true;
                return None;
            }
            "vim" => {
                let on = !self.config.ui.vim;
                self.config.ui.vim = on;
                // Keep the live composer state in step with the persisted flag.
                self.vim = VimState {
                    enabled: on,
                    mode: VimMode::Insert,
                    ..VimState::default()
                };
            }
            "plan_first" => self.config.plan_first = !self.config.plan_first,
            "continuous" => self.config.continuous = !self.config.continuous,
            "plan_each_cycle" => self.config.plan_each_cycle = !self.config.plan_each_cycle,
            "rollback" => {
                self.config.rollback_failed_cycles = !self.config.rollback_failed_cycles;
            }
            "web_backend" => {
                self.open_web_backend_picker();
                return None;
            }
            "web_allow_local" => self.config.web.allow_local = !self.config.web.allow_local,
            "fleet_synthesize" => self.config.fleet.synthesize = !self.config.fleet.synthesize,
            _ => return None,
        }
        // Inline change: persist and re-open, restoring the cursor so repeated
        // toggles stay on the same row. (These flags take effect at the next
        // cycle / startup, not mid-session.)
        if let Err(err) = self.config.save() {
            self.notice(format!("could not save config: {err:#}"));
        }
        self.open_settings_picker();
        if let Some(picker) = self.picker.as_mut() {
            picker.selected = selected.min(picker.items.len().saturating_sub(1));
        }
        None
    }

    /// Open the "import from Claude Code" multi-select. Each row is a toggleable
    /// artifact (Space toggles, Enter runs); order is mcp / commands / verbs to
    /// match the [`ImportSelection`] built in the Enter handler.
    fn open_claude_import_picker(&mut self) {
        if import_claude::claude_home().is_none() {
            self.notice("no Claude Code install found (~/.claude)");
            return;
        }
        let (mcp, commands, verbs) = import_claude::counts();
        let items = vec![
            PickerItem {
                value: format!("MCP servers ({mcp})"),
                detail: "merge into ~/.wizard/mcp.toml".to_string(),
                current: false,
            },
            PickerItem {
                value: format!("Custom commands ({commands})"),
                detail: "copy into ~/.wizard/commands/".to_string(),
                current: false,
            },
            PickerItem {
                value: format!("Spinner verbs ({verbs})"),
                detail: "adopt Claude Code's spinner verbs".to_string(),
                current: false,
            },
        ];
        self.picker = Some(Picker {
            kind: PickerKind::ClaudeImport,
            title: " import from claude code · space toggles · enter runs ".to_string(),
            items,
            selected: 0,
        });
    }

    /// Open the `/fusion config` multi-select: one row per configured provider,
    /// pre-toggled to the current/effective panel. Space toggles membership;
    /// Enter saves `[fusion]` (first toggled row = synthesizer).
    pub fn open_fusion_picker(&mut self) {
        if self.config.providers.is_empty() {
            self.notice(
                "fusion needs configured providers — add at least two with /provider first",
            );
            return;
        }
        let in_panel: std::collections::HashSet<String> = self
            .config
            .effective_fusion()
            .map(|fusion| fusion.panel.into_iter().collect())
            .unwrap_or_default();
        let items = self
            .config
            .providers
            .iter()
            .map(|provider| PickerItem {
                value: provider.name.clone(),
                detail: format!("{} · {}", provider.kind, provider.model),
                current: in_panel.contains(&provider.name),
            })
            .collect();
        self.picker = Some(Picker {
            kind: PickerKind::FusionPanel,
            title: " fusion panel · space toggles · enter saves ".to_string(),
            items,
            selected: 0,
        });
    }

    /// Open the `/resume` picker: every past session on disk, newest first,
    /// each row labeled with its first prompt. The current session is marked
    /// and selecting it is a no-op.
    pub fn open_resume_picker(&mut self) {
        let dir = match crate::config::Config::sessions_dir() {
            Ok(dir) => dir,
            Err(err) => {
                self.notice(format!("cannot locate sessions: {err:#}"));
                return;
            }
        };
        let summaries = crate::agent::session::summaries(&dir);
        if summaries.is_empty() {
            self.notice("no past sessions to resume");
            return;
        }
        let items: Vec<PickerItem> = summaries
            .into_iter()
            .map(|session| {
                let plural = if session.messages == 1 { "" } else { "s" };
                PickerItem {
                    detail: format!("{} · {} msg{plural}", session.summary, session.messages),
                    current: session.id == self.session_id,
                    value: session.id,
                }
            })
            .collect();
        self.picker = Some(Picker {
            kind: PickerKind::Resume,
            title: " resume session · ↑/↓ move · enter select · esc close ".to_string(),
            items,
            selected: 0,
        });
    }

    /// Rebuild the transcript view from a session's persisted messages, so a
    /// resumed conversation reads back the way it was left. Mirrors the live
    /// event handling: assistant text and tool calls become cards, tool
    /// results fill the matching open card. System messages are dropped.
    fn load_transcript(&mut self, messages: Vec<crate::llm::ChatMessage>) {
        use crate::llm::Role;
        self.transcript.clear();
        // A tool's images ride back to the model on a user message the agent
        // wrote for it (`Agent::run_tool`), so a user message full of images
        // right after a tool answered is that tool's images — not something a
        // person said. Same reading as the GUI's replay
        // ([`crate::gui::transcript`]), so both surfaces rebuild the same
        // conversation.
        let mut just_answered: Option<String> = None;
        for message in messages {
            match message.role {
                Role::System => {}
                Role::User => {
                    if let Some(tool) = just_answered.take()
                        && !message.images.is_empty()
                    {
                        let source = ImageSource::Tool(tool);
                        self.transcript
                            .extend(image_entries(&source, replayed_refs(&message.images)));
                        continue;
                    }
                    if !message.content.trim().is_empty() {
                        self.transcript.push(TranscriptEntry::User(message.content));
                    }
                }
                Role::Assistant => {
                    just_answered = None;
                    if !message.content.trim().is_empty() {
                        self.transcript
                            .push(TranscriptEntry::Assistant(message.content));
                    }
                    self.transcript.extend(image_entries(
                        &ImageSource::Assistant,
                        replayed_refs(&message.images),
                    ));
                    for call in message.tool_calls {
                        self.transcript.push(TranscriptEntry::ToolCard {
                            name: call.function.name,
                            args: call.function.arguments,
                            output: None,
                            is_error: false,
                            collapsed: true,
                        });
                    }
                }
                Role::Tool => {
                    let name = message.tool_name.unwrap_or_default();
                    just_answered = Some(name.clone());
                    // Fill the most recent open card for this tool, as a live
                    // ToolFinished would.
                    let card = self
                        .transcript
                        .iter_mut()
                        .rev()
                        .find_map(|entry| match entry {
                            TranscriptEntry::ToolCard {
                                name: card_name,
                                output: slot @ None,
                                ..
                            } if *card_name == name => Some(slot),
                            _ => None,
                        });
                    match card {
                        Some(slot) => *slot = Some(message.content),
                        None => self.transcript.push(TranscriptEntry::ToolCard {
                            name,
                            args: Value::Null,
                            output: Some(message.content),
                            is_error: false,
                            collapsed: true,
                        }),
                    }
                }
            }
        }
        self.streaming.clear();
        self.streaming_thinking.clear();
        self.scroll_to_bottom();
    }

    /// Open the provider picker (level 1): the configured providers (Enter
    /// switches) plus a final "＋ Add provider…" row that opens the type
    /// picker. With no providers configured, only the add row shows.
    pub fn open_provider_picker(&mut self) {
        let active = self.config.active().name;
        let mut items: Vec<PickerItem> = self
            .config
            .providers
            .iter()
            .map(|provider| PickerItem {
                value: provider.name.clone(),
                detail: format!(
                    "{} · {} @ {}",
                    provider.kind, provider.model, provider.base_url
                ),
                current: provider.name == active,
            })
            .collect();
        items.push(PickerItem {
            value: PROVIDER_ADD_ROW.to_string(),
            detail: "configure a new provider".to_string(),
            current: false,
        });
        self.picker = Some(Picker {
            kind: PickerKind::Provider,
            title: " providers · ↑/↓ move · enter select · esc close ".to_string(),
            items,
            selected: 0,
        });
    }

    /// Open the provider-type picker (level 2): the menu of provider kinds to
    /// add. Rows are dispatched by index against the fixed order in
    /// [`PROVIDER_TYPES`], so the labels stay human-readable.
    pub fn open_provider_type_picker(&mut self) {
        let items: Vec<PickerItem> = PROVIDER_TYPES
            .iter()
            .map(|(label, detail)| PickerItem {
                value: (*label).to_string(),
                detail: (*detail).to_string(),
                current: false,
            })
            .collect();
        self.picker = Some(Picker {
            kind: PickerKind::ProviderType,
            title: " add provider · ↑/↓ move · enter select · esc close ".to_string(),
            items,
            selected: 0,
        });
    }

    /// Open the `web_search` backend picker (from `/settings`). Marks the
    /// current backend so the user sees what is active.
    pub fn open_web_backend_picker(&mut self) {
        let active = self.config.web.search_backend.trim().to_ascii_lowercase();
        let items: Vec<PickerItem> = WEB_BACKENDS
            .iter()
            .map(|(value, label, detail)| PickerItem {
                value: (*value).to_string(),
                detail: format!("{label} — {detail}"),
                current: *value == active || (*value == "xai" && active == "grok"),
            })
            .collect();
        self.picker = Some(Picker {
            kind: PickerKind::WebBackend,
            title: " web search · ↑/↓ move · enter select · esc close ".to_string(),
            items,
            selected: 0,
        });
    }

    /// Apply a `web_search` backend selection that needs no key entry
    /// (DuckDuckGo, or xAI once a session/key exists): persist and report.
    fn set_web_backend(&mut self, id: &str, note: &str) {
        self.config.web.search_backend = id.to_string();
        if let Err(err) = self.config.save() {
            self.notice(format!("could not save config: {err:#}"));
            return;
        }
        self.notice(note.to_string());
    }

    /// Handle a row from the `web_search` backend picker: DuckDuckGo applies at
    /// once; keyed backends start an inline key prompt; xAI reuses the OAuth
    /// session when present (no re-login) and otherwise points the user at
    /// `/login xai`.
    fn select_web_backend(&mut self, id: &str) {
        match id {
            "xai" | "grok" => {
                if xai_oauth_session_present() {
                    self.set_web_backend(
                        "xai",
                        "web search: using your xAI sign-in (no new login needed)",
                    );
                } else if crate::credentials::get("xai").is_some() {
                    self.set_web_backend("xai", "web search: using xAI (stored API key)");
                } else {
                    self.set_web_backend(
                        "xai",
                        "web search set to xAI — run /login xai to sign in, or set XAI_API_KEY",
                    );
                }
            }
            keyed if web_backend_needs_key(keyed) => self.begin_web_key_prompt(keyed),
            other => {
                let label = web_backend_label(other).to_string();
                self.set_web_backend(other, &format!("web search: using {label}"));
            }
        }
    }

    /// Start the inline prompt that collects (and stores) a pasted API key for
    /// a keyed `web_search` backend.
    fn begin_web_key_prompt(&mut self, id: &str) {
        self.web_key_backend = Some(id.to_string());
        self.input_mode = InputMode::Prompt;
        self.clear_input();
        self.suggestions.clear();
        self.suggestion_index = 0;
        self.notice(format!(
            "paste your {} API key, then Enter (Esc to cancel):",
            web_backend_label(id)
        ));
    }

    /// Consume the composer input as the pasted API key: store it under the
    /// backend name in `~/.wizard/credentials.toml`, switch to that backend,
    /// and return to normal input. An empty entry cancels.
    fn submit_web_key(&mut self) -> Option<AppAction> {
        let id = self.web_key_backend.take()?;
        let key = self.input.trim().to_string();
        self.input.clear();
        self.cursor = 0;
        self.input_mode = InputMode::Chat;
        self.sync_input_mode();
        if key.is_empty() {
            self.notice("cancelled (no key entered)");
            return None;
        }
        if let Err(err) = crate::credentials::store(&id, &key) {
            self.notice(format!("could not save the {id} API key: {err:#}"));
            return None;
        }
        let label = web_backend_label(&id).to_string();
        self.set_web_backend(
            &id,
            &format!("web search: using {label} (key saved to ~/.wizard/credentials.toml)"),
        );
        None
    }

    /// True when the composer is collecting a masked field (an API key) in an
    /// inline prompt — provider setup or web-search key entry. Drives the
    /// bullet masking in [`crate::ui`].
    pub fn prompt_is_masked(&self) -> bool {
        if self.web_key_backend.is_some() {
            return true;
        }
        self.input_mode == InputMode::Prompt
            && self
                .prompt
                .as_ref()
                .and_then(|prompt| prompt.queue.front())
                .copied()
                == Some(PromptField::ApiKey)
    }

    /// Start the inline provider-setup prompt: switch the composer into
    /// [`InputMode::Prompt`] and ask the first queued field.
    pub fn begin_provider_prompt(&mut self, prompt: ProviderPrompt) {
        self.prompt = Some(prompt);
        self.input_mode = InputMode::Prompt;
        self.clear_input();
        self.suggestions.clear();
        self.suggestion_index = 0;
        if let Some(prompt) = self.prompt.as_ref()
            && let Some(field) = prompt.queue.front().copied()
        {
            let question = prompt_question(field, prompt);
            self.notice(question);
        }
    }

    /// Cancel an in-progress provider-setup prompt and return to normal input.
    fn cancel_prompt(&mut self) {
        self.prompt = None;
        self.web_key_backend = None;
        self.input.clear();
        self.cursor = 0;
        self.input_mode = InputMode::Chat;
        self.sync_input_mode();
        self.notice("cancelled");
    }

    /// Consume the current input as the answer to the front prompt field. When
    /// more fields remain, ask the next and stay in prompt mode; when the queue
    /// drains, emit a [`SlashCommand::ProviderSetup`].
    fn submit_prompt_field(&mut self) -> Option<AppAction> {
        let value = self.input.trim().to_string();
        let prompt = self.prompt.as_mut()?;
        let field = prompt.queue.pop_front()?;
        match field {
            PromptField::Name => prompt.name = value,
            PromptField::AccountId => {
                if value.is_empty() {
                    // No account id is treated as "never mind".
                    self.cancel_prompt();
                    return None;
                }
                // Substitute the account id into the base-URL template
                // (e.g. `.../accounts/{account_id}/ai/v1`).
                prompt.base_url = prompt
                    .base_url
                    .replace(crate::llm::cloudflare::ACCOUNT_ID_PLACEHOLDER, &value);
            }
            PromptField::BaseUrl => prompt.base_url = value,
            PromptField::Model => prompt.model = value,
            PromptField::ApiKey => {
                if value.is_empty() {
                    // An empty key is treated as "never mind".
                    self.cancel_prompt();
                    return None;
                }
                prompt.api_key = Some(value);
            }
        }
        self.input.clear();
        self.cursor = 0;
        if let Some(next) = prompt.queue.front().copied() {
            let question = prompt_question(next, prompt);
            self.notice(question);
            return None;
        }
        // Queue drained: build the setup command and return to normal input.
        let prompt = self.prompt.take().expect("prompt is set");
        self.input_mode = InputMode::Chat;
        self.sync_input_mode();
        Some(AppAction::Command(SlashCommand::ProviderSetup {
            name: prompt.name,
            kind: prompt.kind,
            base_url: prompt.base_url,
            model: prompt.model,
            api_key: prompt.api_key,
        }))
    }

    /// Current state for this session's heartbeat: needs-input when paused on a
    /// plan review, working while a turn streams, otherwise idle.
    fn session_state(&self) -> SessionState {
        if self.plan_review.is_some() || self.interview.is_some() {
            SessionState::NeedsInput
        } else if self.status.busy {
            SessionState::Working
        } else {
            SessionState::Idle
        }
    }

    /// One-line summary of what this session is doing, for the dashboard row.
    fn session_activity(&self) -> String {
        if self.plan_review.is_some() {
            return "waiting for plan approval".to_string();
        }
        if self.interview.is_some() {
            return "waiting for interview answers".to_string();
        }
        if !self.status.busy {
            return "idle".to_string();
        }
        // The newest in-flight tool call reads best; fall back to the verb.
        for entry in self.transcript.iter().rev() {
            if let TranscriptEntry::ToolCard {
                name, output: None, ..
            } = entry
            {
                return name.clone();
            }
        }
        format!("{}…", self.spinner_verb)
    }

    // ---- Subagent rail -------------------------------------------------
    //
    // The rail is the row of dots under the composer, one per subagent run.
    // ↓ from the composer focuses it, ↑/↓ move between dots, Enter opens the
    // selected one as a full chat view, Esc backs out.

    /// Index of the pane for `run`, if it is still on the rail.
    fn pane_index(&self, run: u64) -> Option<usize> {
        self.panes.iter().position(|pane| pane.run == run)
    }

    /// Append to a pane's transcript and bump its unread badge — unless the
    /// user is currently watching that pane, in which case they have already
    /// seen it. Scroll position is left alone: a following pane stays pinned
    /// by its follow flag, a scrolled-up pane keeps its top-anchored offset.
    fn push_pane(&mut self, run: u64, entry: TranscriptEntry) {
        let Some(index) = self.pane_index(run) else {
            return;
        };
        let attached = self.attached == Some(index);
        let pane = &mut self.panes[index];
        pane.transcript.push(entry);
        if !attached {
            pane.unread += 1;
        }
    }

    /// The pane the user is inside, if any.
    pub fn attached_pane(&self) -> Option<&SubagentPane> {
        self.attached.and_then(|index| self.panes.get(index))
    }

    /// Number of runs still going — the count shown on the rail header.
    pub fn running_panes(&self) -> usize {
        self.panes
            .iter()
            .filter(|pane| pane.status == PaneStatus::Running)
            .count()
    }

    /// Move the rail selection by `delta`, clamped at both ends. Moving up off
    /// the top row returns focus to the composer, which is what makes ↑/↓ feel
    /// continuous between the two.
    fn rail_select(&mut self, delta: isize) {
        let Some(current) = self.rail_focus else {
            return;
        };
        let next = current as isize + delta;
        if next < 0 {
            self.rail_focus = None;
            return;
        }
        self.rail_focus = Some((next as usize).min(self.panes.len().saturating_sub(1)));
    }

    /// Give the rail keyboard focus, selecting the first running pane if there
    /// is one (that is the one you almost always want) and the last pane
    /// otherwise. No-op when nothing has been delegated yet.
    pub fn focus_rail(&mut self) -> bool {
        if self.panes.is_empty() {
            return false;
        }
        let target = self
            .panes
            .iter()
            .position(|pane| pane.status == PaneStatus::Running)
            .unwrap_or(self.panes.len() - 1);
        self.rail_focus = Some(target);
        true
    }

    /// Open a pane as the main chat view: its transcript takes over the
    /// screen until Esc. Clears the unread badge — you are looking at it now.
    /// Starts following the live tail so opening a running agent shows the
    /// newest work rather than whatever offset it last held.
    pub fn attach_pane(&mut self, index: usize) {
        let Some(pane) = self.panes.get_mut(index) else {
            return;
        };
        pane.unread = 0;
        pane.scroll = 0;
        pane.scroll_follow = true;
        self.attached = Some(index);
        self.rail_focus = Some(index);
    }

    /// Attach the pane `delta` rows away from `index`, wrapping around the
    /// rail so ↓ always lands on another run and the browse never dead-ends at
    /// the last one.
    ///
    /// With a single run there is nowhere to step, so ↑/↓ fall back to their
    /// other job and scroll the pane you are reading.
    fn step_pane(&mut self, index: usize, delta: isize) {
        let len = self.panes.len();
        if len < 2 {
            self.scroll_pane(index, if delta < 0 { 1 } else { -1 });
            return;
        }
        let next = (index as isize + delta).rem_euclid(len as isize) as usize;
        self.attach_pane(next);
    }

    /// Leave the attached pane and go all the way back to the main chat, with
    /// focus in the composer — one Esc, and you are typing again. (Leaving
    /// focus parked on the rail meant a second Esc to actually get out, which
    /// is one too many for the way back.)
    pub fn detach_pane(&mut self) {
        if let Some(index) = self.attached.take()
            && let Some(pane) = self.panes.get_mut(index)
        {
            pane.unread = 0;
        }
        self.rail_focus = None;
        // A run that finished while you were watching it has been sitting on
        // the rail with its linger clock stopped; let it retire now.
        self.retire_finished_panes();
    }

    /// Scroll the pane at `index` by `delta` lines. Positive moves toward older
    /// content (up); negative toward the live tail (down). Leaving the bottom
    /// clears follow so new output does not yank the view; returning to the
    /// bottom re-enables it. `scroll` is the first visible line from the top.
    fn scroll_pane(&mut self, index: usize, delta: i16) {
        let Some(pane) = self.panes.get_mut(index) else {
            return;
        };
        let max = pane.max_scroll.get();
        let current = if pane.scroll_follow {
            max
        } else {
            pane.scroll.min(max)
        };
        // Top-anchored: older content is a smaller start offset.
        let next = if delta >= 0 {
            current.saturating_sub(delta as u16)
        } else {
            current.saturating_add(delta.unsigned_abs()).min(max)
        };
        if next >= max {
            pane.scroll = 0;
            pane.scroll_follow = true;
        } else {
            pane.scroll = next;
            pane.scroll_follow = false;
        }
    }

    /// Jump a pane (or the main transcript when no pane is attached) to the
    /// live tail and re-enable stick-to-bottom.
    fn scroll_to_bottom(&mut self) {
        if let Some(index) = self.attached {
            if let Some(pane) = self.panes.get_mut(index) {
                pane.scroll = 0;
                pane.scroll_follow = true;
            }
            return;
        }
        self.scroll = 0;
        self.scroll_follow = true;
    }

    /// Scroll the main transcript by `delta` lines. Positive moves toward older
    /// content (up); negative toward the live tail (down). Same stick-to-bottom
    /// rule as [`Self::scroll_pane`].
    fn scroll_transcript(&mut self, delta: i16) {
        let max = self.transcript_max_scroll.get();
        let current = if self.scroll_follow {
            max
        } else {
            self.scroll.min(max)
        };
        let next = if delta >= 0 {
            current.saturating_sub(delta as u16)
        } else {
            current.saturating_add(delta.unsigned_abs()).min(max)
        };
        if next >= max {
            self.scroll = 0;
            self.scroll_follow = true;
        } else {
            self.scroll = next;
            self.scroll_follow = false;
        }
    }

    /// Drop finished runs off the rail once they have been resting long enough
    /// to notice, so the rail shows live work instead of accumulating every
    /// subagent the session ever ran.
    ///
    /// Nothing is lost: a foreground run's report is the output of its
    /// `spawn_subagent` card in the main chat, and a background run's report is
    /// written back into that same card when it lands (see
    /// [`App::record_subagent_report`]).
    ///
    /// The pane you are *inside* never retires under you — its clock starts
    /// when you leave it.
    pub fn retire_finished_panes(&mut self) {
        if self.panes.is_empty() {
            return;
        }
        // Selections are indices, and retiring shifts them — remember what they
        // point *at*, then re-find it afterwards.
        let attached_run = self.attached.and_then(|i| self.panes.get(i)).map(|p| p.run);
        let focus_run = self
            .rail_focus
            .and_then(|i| self.panes.get(i))
            .map(|p| p.run);

        let now = Instant::now();
        let before = self.panes.len();
        self.panes.retain(|pane| match pane.finished {
            _ if Some(pane.run) == attached_run => true,
            Some(at) => now.duration_since(at) < PANE_LINGER,
            None => true,
        });
        if self.panes.len() == before {
            return;
        }

        self.attached = attached_run.and_then(|run| self.pane_index(run));
        // If the run the rail was sitting on just retired, focus falls back to
        // the composer rather than silently jumping to some other subagent.
        self.rail_focus = focus_run.and_then(|run| self.pane_index(run));
    }

    /// Write a finished background run's report into the `spawn_subagent` card
    /// that launched it, replacing the "delegated, running in the background"
    /// placeholder. The card is the durable record of the run once its pane
    /// retires off the rail.
    fn record_subagent_report(&mut self, name: &str, task: &str, report: &str, is_error: bool) {
        let card = self.transcript.iter_mut().rev().find(|entry| {
            matches!(
                entry,
                TranscriptEntry::ToolCard { name: card, args, .. }
                    if card == "spawn_subagent"
                        && args.get("subagent").and_then(|v| v.as_str()) == Some(name)
                        && args.get("task").and_then(|v| v.as_str()) == Some(task)
            )
        });
        if let Some(TranscriptEntry::ToolCard {
            output,
            is_error: card_error,
            collapsed,
            ..
        }) = card
        {
            *output = Some(report.to_string());
            *card_error = is_error;
            *collapsed = true;
        }
    }

    /// Kill the selected run. Only background runs can be killed — a
    /// foreground run has the parent turn blocked on it, so the way to stop it
    /// is to interrupt the turn (Ctrl-C).
    fn kill_pane(&mut self, index: usize) {
        let Some(pane) = self.panes.get(index) else {
            return;
        };
        let (name, bg) = (pane.name.clone(), pane.bg);
        let Some(bg) = bg else {
            self.notice(format!(
                "subagent '{name}' is running in the foreground — Ctrl-C interrupts the turn it \
                 is blocking"
            ));
            return;
        };
        let Some(registry) = self.subagents.clone() else {
            return;
        };
        if registry.kill(bg) {
            // Aborting the driver task means the run emits no closing event of
            // its own, so retire the pane here.
            if let Some(pane) = self.panes.get_mut(index) {
                pane.status = PaneStatus::Failed;
                pane.finished = Some(Instant::now());
                pane.transcript
                    .push(TranscriptEntry::Notice("killed on request".to_string()));
            }
            self.notice(format!("killed subagent '{name}' (#{bg})"));
        } else {
            self.notice(format!("subagent '{name}' (#{bg}) already finished"));
        }
    }

    /// Build this session's heartbeat record from current state.
    pub fn session_record(&self) -> SessionRecord {
        SessionRecord {
            id: self.session_id.clone(),
            name: self.session_name.clone(),
            cwd: self.project_root.display().to_string(),
            model: self.status.model.clone(),
            mode: self.mode.to_string(),
            state: self.session_state(),
            activity: self.session_activity(),
            pid: std::process::id(),
            started_unix: self.session_started_unix,
            updated_unix: 0, // stamped by session_registry::write
        }
    }

    /// Reload the live-session list from the registry, keeping the selection
    /// in range. Cheap (a few small files); safe to poll. The peek panel is
    /// refreshed separately on a slower cadence — see [`App::refresh_peek`].
    pub fn refresh_sessions(&mut self) {
        self.sessions = session_registry::list();
        if self.dashboard_selected >= self.sessions.len() {
            self.dashboard_selected = self.sessions.len().saturating_sub(1);
        }
    }

    /// Reload the peek panel with the selected session's recent transcript.
    /// Reads only the tail of the session file, so it is cheap enough to call
    /// on selection changes and a ~1s poll, but not every frame.
    pub fn refresh_peek(&mut self) {
        self.peek_lines = match self.sessions.get(self.dashboard_selected) {
            Some(session) => crate::agent::session::peek(&session.id, 50),
            None => Vec::new(),
        };
    }

    /// Spawn a detached background session for `prompt`: a headless sovereign
    /// `wizard --bg` run that registers in the session registry, so it shows up
    /// in every dashboard on the machine and survives this session exiting.
    fn dispatch_session(&mut self, prompt: String) {
        use std::os::unix::process::CommandExt;
        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(err) => {
                self.notice(format!("could not locate the wizard binary: {err}"));
                return;
            }
        };
        let spawned = std::process::Command::new(exe)
            .arg("--bg")
            .arg("--mode")
            .arg("sovereign")
            .arg("-p")
            .arg(&prompt)
            .arg("--cwd")
            .arg(&self.project_root)
            .current_dir(&self.project_root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            // Own process group: detached from the TUI's job control, and
            // killable as a group when stopped.
            .process_group(0)
            .spawn();
        match spawned {
            Ok(_) => {
                self.notice(format!("dispatched background session: {prompt}"));
                self.refresh_sessions();
            }
            Err(err) => self.notice(format!("dispatch failed: {err}")),
        }
    }

    /// Stop the selected background session (Ctrl-X): SIGTERM its process group
    /// and drop its registry row. Refuses to stop the session you're in.
    fn stop_selected_session(&mut self) {
        let Some(session) = self.sessions.get(self.dashboard_selected) else {
            return;
        };
        if session.id == self.session_id {
            self.notice("that's this session — use /quit to leave it");
            return;
        }
        let (id, name, pid) = (session.id.clone(), session.name.clone(), session.pid as i32);
        // Signal the whole group (dispatched sessions are group leaders, so
        // their tool subprocesses die too); fall back to the bare pid.
        unsafe {
            if libc::kill(-pid, libc::SIGTERM) != 0 {
                libc::kill(pid, libc::SIGTERM);
            }
        }
        session_registry::remove(&id);
        self.notice(format!("stopped session: {name}"));
        self.refresh_sessions();
    }

    /// Move the dashboard selection up/down, clamped to the session list.
    fn dashboard_select(&mut self, delta: isize) {
        let len = self.sessions.len();
        if len == 0 {
            self.dashboard_selected = 0;
            return;
        }
        let last = len - 1;
        self.dashboard_selected = match delta {
            d if d < 0 => self.dashboard_selected.checked_sub(1).unwrap_or(last),
            _ if self.dashboard_selected >= last => 0,
            _ => self.dashboard_selected + 1,
        };
        self.refresh_peek();
    }

    /// Recompute [`InputMode`] from the input text, then refresh the command
    /// suggestions.
    fn sync_input_mode(&mut self) {
        // While answering an inline prompt the composer stays in Prompt mode no
        // matter what is typed (a key never flips it to Command/Chat), and the
        // suggestion popup is suppressed.
        if self.prompt.is_some() || self.web_key_backend.is_some() {
            return;
        }
        self.input_mode = if self.input.trim_start().starts_with('/') {
            InputMode::Command
        } else {
            InputMode::Chat
        };
        self.refresh_suggestions();
    }

    /// Rebuild the suggestion list from the typed `/command` prefix.
    /// Prefix matches rank above substring matches; suggestions disappear
    /// once arguments are being typed.
    fn refresh_suggestions(&mut self) {
        // Remember an actively moved highlight (off the top row) so it does
        // not jump identity when the list is rebuilt; the default highlight
        // must keep tracking the best match.
        let previous = if self.suggestion_index > 0 {
            self.suggestions
                .get(self.suggestion_index)
                .map(|spec| spec.name.clone())
        } else {
            None
        };
        self.suggestions.clear();
        if self.input_mode != InputMode::Command || self.picker.is_some() {
            self.suggestion_index = 0;
            return;
        }
        let Some(token) = self.input.trim_start().strip_prefix('/') else {
            self.suggestion_index = 0;
            return;
        };
        if token.contains(char::is_whitespace) {
            self.suggestion_index = 0;
            return;
        }
        // Builtins in display order, then custom commands (already sorted).
        let candidates: Vec<Suggestion> = COMMANDS
            .iter()
            .map(Suggestion::from)
            .chain(self.custom_commands.iter().map(Suggestion::from))
            .collect();
        // Rank: exact match, then prefix matches, then substring matches.
        self.suggestions
            .extend(candidates.iter().filter(|spec| spec.name == token).cloned());
        self.suggestions.extend(
            candidates
                .iter()
                .filter(|spec| spec.name != token && spec.name.starts_with(token))
                .cloned(),
        );
        self.suggestions.extend(
            candidates
                .iter()
                .filter(|spec| !spec.name.starts_with(token) && spec.name.contains(token))
                .cloned(),
        );
        self.suggestion_index = previous
            .and_then(|name| self.suggestions.iter().position(|spec| spec.name == name))
            .unwrap_or(0);
    }

    /// Replace the input with the highlighted suggestion. Returns the
    /// completed suggestion, or `None` when nothing is highlighted.
    fn accept_suggestion(&mut self) -> Option<Suggestion> {
        let spec = self.suggestions.get(self.suggestion_index)?.clone();
        let mut text = format!("/{}", spec.name);
        if spec.takes_args {
            text.push(' ');
        }
        self.set_input(text);
        Some(spec)
    }

    // --- input editing (cursor is a character index into `input`) ---

    /// Byte offset of the cursor in `input`.
    fn byte_index(&self) -> usize {
        self.char_byte(self.cursor)
    }

    /// Byte offset of character index `n` in `input` (its end when out of
    /// range).
    fn char_byte(&self, n: usize) -> usize {
        self.input
            .char_indices()
            .nth(n)
            .map(|(index, _)| index)
            .unwrap_or(self.input.len())
    }

    fn set_input(&mut self, text: String) {
        self.cursor = text.chars().count();
        self.input = text;
        self.sync_input_mode();
    }

    fn clear_input(&mut self) {
        self.input.clear();
        self.cursor = 0;
        self.history_pos = None;
        self.sync_input_mode();
    }

    /// Replace the composer with text edited externally (Ctrl-G). Editors
    /// append a trailing newline, so at most one line ending is trimmed; the
    /// cursor lands at the end.
    fn set_input_from_editor(&mut self, mut text: String) {
        if text.ends_with('\n') {
            text.pop();
            if text.ends_with('\r') {
                text.pop();
            }
        }
        self.history_pos = None;
        self.set_input(text);
    }

    fn insert_char(&mut self, c: char) {
        let index = self.byte_index();
        self.input.insert(index, c);
        self.cursor += 1;
    }

    /// Insert a hard line break at the cursor (Shift/Alt+Enter). The composer
    /// grows to a multi-line box; Enter alone still submits.
    fn insert_newline(&mut self) {
        self.insert_char('\n');
    }

    fn insert_str(&mut self, text: &str) {
        let index = self.byte_index();
        self.input.insert_str(index, text);
        self.cursor += text.chars().count();
    }

    fn delete_back(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor -= 1;
        let index = self.byte_index();
        self.input.remove(index);
    }

    fn delete_forward(&mut self) {
        if self.cursor < self.input.chars().count() {
            let index = self.byte_index();
            self.input.remove(index);
        }
    }

    /// Delete the word before the cursor (Ctrl-W).
    fn delete_word_back(&mut self) {
        let chars: Vec<char> = self.input.chars().collect();
        while self.cursor > 0 && chars[self.cursor - 1].is_whitespace() {
            self.delete_back();
        }
        let chars: Vec<char> = self.input.chars().collect();
        let mut end = self.cursor;
        while end > 0 && !chars[end - 1].is_whitespace() {
            end -= 1;
        }
        while self.cursor > end {
            self.delete_back();
        }
    }

    // --- vim modal editing ---

    /// `/vim`: toggle modal editing, persist the choice to `[ui] vim`, and
    /// reset to Insert so typing works immediately when enabling.
    pub fn toggle_vim(&mut self) {
        let on = !self.vim.enabled;
        self.vim = VimState {
            enabled: on,
            mode: VimMode::Insert,
            ..VimState::default()
        };
        self.config.ui.vim = on;
        if let Err(err) = self.config.save() {
            self.notice(format!("could not save config: {err:#}"));
        }
        self.notice(if on {
            "vim mode on — Esc for NORMAL (hjkl/w/b/e move · i/a/I/A insert · \
             x/dd/dw/cw edit · u undo), i to type. /vim to leave"
        } else {
            "vim mode off"
        });
    }

    /// Enter Insert mode (text entry resumes).
    fn enter_insert(&mut self) {
        self.vim.mode = VimMode::Insert;
        self.vim.clear_pending();
    }

    /// Leave Insert for Normal mode. Vim nudges the cursor one cell left so it
    /// sits on the last typed character rather than past it.
    fn enter_normal_mode(&mut self) {
        self.vim.mode = VimMode::Normal;
        self.vim.clear_pending();
        self.cursor = self.cursor.saturating_sub(1);
        self.clamp_normal_cursor();
    }

    /// In Normal mode the cursor sits *on* a character, never past the last
    /// one (an empty line keeps it at 0).
    fn clamp_normal_cursor(&mut self) {
        if self.vim.mode != VimMode::Normal {
            return;
        }
        let len = self.input.chars().count();
        self.cursor = if len == 0 {
            0
        } else {
            self.cursor.min(len - 1)
        };
    }

    /// Snapshot the line for `u` before a Normal-mode edit.
    fn vim_snapshot(&mut self) {
        let cursor = self.cursor;
        self.vim.push_undo(&self.input, cursor);
    }

    /// Drop the most recent undo snapshot back into the line (`u`).
    fn vim_undo(&mut self) {
        if let Some((input, cursor)) = self.vim.undo.pop() {
            self.input = input;
            self.cursor = cursor;
            self.clamp_normal_cursor();
        }
    }

    /// Remove characters `[start, end)` and return them; leaves the cursor at
    /// `start`. Used by `x`/`D` and the `d`/`c` operators.
    fn vim_delete_range(&mut self, start: usize, end: usize) -> String {
        let len = self.input.chars().count();
        let start = start.min(len);
        let end = end.min(len);
        if start >= end {
            return String::new();
        }
        let bstart = self.char_byte(start);
        let bend = self.char_byte(end);
        let removed = self.input[bstart..bend].to_string();
        self.input.replace_range(bstart..bend, "");
        self.cursor = start;
        removed
    }

    /// Replace the character under the cursor with `c` (`r`).
    fn vim_replace_char(&mut self, c: char) {
        let len = self.input.chars().count();
        if self.cursor >= len {
            return;
        }
        self.vim_snapshot();
        let idx = self.byte_index();
        self.input.remove(idx);
        self.input.insert(idx, c);
    }

    /// Apply an operator over the character range `[start, end)` (`dw`, `c$`,
    /// `ye`, …). Delete/Change stash the text in the register; Change then
    /// enters Insert. Yank only copies.
    fn vim_apply_op(&mut self, op: VimOp, start: usize, end: usize) {
        let (start, end) = (start.min(end), start.max(end));
        if start >= end {
            return;
        }
        match op {
            VimOp::Yank => {
                let bstart = self.char_byte(start);
                let bend = self.char_byte(end);
                self.vim.register = self.input[bstart..bend].to_string();
            }
            VimOp::Delete => {
                self.vim_snapshot();
                self.vim.register = self.vim_delete_range(start, end);
                self.clamp_normal_cursor();
            }
            VimOp::Change => {
                self.vim_snapshot();
                self.vim.register = self.vim_delete_range(start, end);
                self.enter_insert();
            }
        }
    }

    /// Linewise operator (`dd`/`cc`/`yy`): the whole single-line buffer.
    fn vim_apply_linewise(&mut self, op: VimOp) {
        match op {
            VimOp::Yank => self.vim.register = self.input.clone(),
            VimOp::Delete => {
                self.vim_snapshot();
                self.vim.register = std::mem::take(&mut self.input);
                self.cursor = 0;
            }
            VimOp::Change => {
                self.vim_snapshot();
                self.vim.register = std::mem::take(&mut self.input);
                self.cursor = 0;
                self.enter_insert();
            }
        }
    }

    /// Paste the register `n` times, after the cursor (`p`) or before it
    /// (`P`); the cursor lands on the last pasted character.
    fn vim_paste(&mut self, after: bool, n: usize) {
        if self.vim.register.is_empty() {
            return;
        }
        self.vim_snapshot();
        let text = self.vim.register.repeat(n.max(1));
        let len = self.input.chars().count();
        let at = if after && len > 0 {
            (self.cursor + 1).min(len)
        } else {
            self.cursor
        };
        let byte = self.char_byte(at);
        self.input.insert_str(byte, &text);
        self.cursor = at + text.chars().count().saturating_sub(1);
        self.clamp_normal_cursor();
    }

    /// Handle one key in Normal mode. Returns an [`AppAction`] only for keys
    /// that submit (Enter); everything else mutates composer state in place.
    fn handle_vim_normal(&mut self, key: KeyEvent) -> Result<Option<AppAction>> {
        let mut action = None;
        let printable = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        'dispatch: {
            // `r` armed: the next printable key replaces the char under cursor.
            if self.vim.pending == Some(Pending::Replace) {
                self.vim.pending = None;
                if let (KeyCode::Char(c), true) = (key.code, printable) {
                    self.vim_replace_char(c);
                }
                break 'dispatch;
            }

            // Count prefix: digits accumulate (a leading `0` is the motion, not
            // a count).
            if let KeyCode::Char(c @ '0'..='9') = key.code
                && printable
                && !(c == '0' && self.vim.count.is_none())
            {
                let digit = c as usize - '0' as usize;
                let next = self.vim.count.unwrap_or(0).saturating_mul(10) + digit;
                self.vim.count = Some(next.min(100_000));
                break 'dispatch;
            }

            // An operator is pending: read this key as its motion, or as the
            // linewise form when the operator key repeats (`dd`/`cc`/`yy`).
            if let Some(Pending::Operator(op)) = self.vim.pending {
                self.vim.pending = None;
                let n = self.vim.count.take().unwrap_or(1);
                let repeated = matches!(
                    (op, key.code),
                    (VimOp::Delete, KeyCode::Char('d'))
                        | (VimOp::Change, KeyCode::Char('c'))
                        | (VimOp::Yank, KeyCode::Char('y'))
                );
                if repeated {
                    self.vim_apply_linewise(op);
                } else if let KeyCode::Char(motion) = key.code {
                    let chars: Vec<char> = self.input.chars().collect();
                    if let Some(m) = vim::resolve_motion(motion, n, &chars, self.cursor) {
                        self.vim_apply_op(op, m.start, m.end);
                    }
                }
                break 'dispatch;
            }

            let len = self.input.chars().count();
            match key.code {
                // --- motions ---
                KeyCode::Char('h') | KeyCode::Left => {
                    let n = self.vim.count.take().unwrap_or(1);
                    self.cursor = self.cursor.saturating_sub(n);
                }
                KeyCode::Char('l') | KeyCode::Right | KeyCode::Char(' ') => {
                    let n = self.vim.count.take().unwrap_or(1);
                    self.cursor = (self.cursor + n).min(len);
                    self.clamp_normal_cursor();
                }
                KeyCode::Char('0') => {
                    self.vim.count = None;
                    self.cursor = 0;
                }
                KeyCode::Char('^') => {
                    self.vim.count = None;
                    let chars: Vec<char> = self.input.chars().collect();
                    self.cursor = vim::first_non_blank(&chars);
                }
                KeyCode::Char('$') => {
                    self.vim.count = None;
                    self.cursor = len;
                    self.clamp_normal_cursor();
                }
                KeyCode::Char('w') => {
                    let n = self.vim.count.take().unwrap_or(1);
                    let chars: Vec<char> = self.input.chars().collect();
                    for _ in 0..n {
                        self.cursor = vim::word_forward(&chars, self.cursor);
                    }
                    self.clamp_normal_cursor();
                }
                KeyCode::Char('b') => {
                    let n = self.vim.count.take().unwrap_or(1);
                    let chars: Vec<char> = self.input.chars().collect();
                    for _ in 0..n {
                        self.cursor = vim::word_back(&chars, self.cursor);
                    }
                }
                KeyCode::Char('e') => {
                    let n = self.vim.count.take().unwrap_or(1);
                    let chars: Vec<char> = self.input.chars().collect();
                    for _ in 0..n {
                        self.cursor = vim::word_end(&chars, self.cursor);
                    }
                    self.clamp_normal_cursor();
                }
                // Single-line analog of j/k: walk the input history.
                KeyCode::Char('k') | KeyCode::Up => {
                    self.vim.count = None;
                    self.history_prev();
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    self.vim.count = None;
                    self.history_next();
                }

                // --- insert transitions ---
                KeyCode::Char('i') => self.enter_insert(),
                KeyCode::Char('I') => {
                    let chars: Vec<char> = self.input.chars().collect();
                    self.cursor = vim::first_non_blank(&chars);
                    self.enter_insert();
                }
                KeyCode::Char('a') => {
                    self.cursor = (self.cursor + 1).min(len);
                    self.enter_insert();
                }
                KeyCode::Char('A') => {
                    self.cursor = len;
                    self.enter_insert();
                }
                // Single-line: `o`/`O` have no new line to open, so they map to
                // append-at-end / insert-at-start.
                KeyCode::Char('o') => {
                    self.cursor = len;
                    self.enter_insert();
                }
                KeyCode::Char('O') => {
                    self.cursor = 0;
                    self.enter_insert();
                }

                // --- edits ---
                KeyCode::Char('x') => {
                    let n = self.vim.count.take().unwrap_or(1);
                    self.vim_snapshot();
                    self.vim.register = self.vim_delete_range(self.cursor, self.cursor + n);
                    self.clamp_normal_cursor();
                }
                KeyCode::Char('X') => {
                    let n = self.vim.count.take().unwrap_or(1);
                    let start = self.cursor.saturating_sub(n);
                    self.vim_snapshot();
                    self.vim.register = self.vim_delete_range(start, self.cursor);
                }
                KeyCode::Char('D') => {
                    self.vim.count = None;
                    self.vim_snapshot();
                    self.vim.register = self.vim_delete_range(self.cursor, len);
                    self.clamp_normal_cursor();
                }
                KeyCode::Char('C') => {
                    self.vim.count = None;
                    self.vim_snapshot();
                    self.vim.register = self.vim_delete_range(self.cursor, len);
                    self.enter_insert();
                }
                KeyCode::Char('s') => {
                    let n = self.vim.count.take().unwrap_or(1);
                    self.vim_snapshot();
                    self.vim.register = self.vim_delete_range(self.cursor, self.cursor + n);
                    self.enter_insert();
                }
                KeyCode::Char('S') => {
                    self.vim.count = None;
                    self.vim_apply_linewise(VimOp::Change);
                }
                KeyCode::Char('r') => self.vim.pending = Some(Pending::Replace),

                // --- operators (await a motion) ---
                KeyCode::Char('d') => self.vim.pending = Some(Pending::Operator(VimOp::Delete)),
                KeyCode::Char('c') => self.vim.pending = Some(Pending::Operator(VimOp::Change)),
                KeyCode::Char('y') => self.vim.pending = Some(Pending::Operator(VimOp::Yank)),

                // --- paste / undo ---
                KeyCode::Char('p') => {
                    let n = self.vim.count.take().unwrap_or(1);
                    self.vim_paste(true, n);
                }
                KeyCode::Char('P') => {
                    let n = self.vim.count.take().unwrap_or(1);
                    self.vim_paste(false, n);
                }
                KeyCode::Char('u') => {
                    self.vim.count = None;
                    self.vim_undo();
                }

                // --- still-useful editing keys in Normal mode ---
                KeyCode::Enter
                    if key
                        .modifiers
                        .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
                {
                    self.vim.count = None;
                    self.insert_newline();
                }
                KeyCode::Enter => {
                    self.vim.count = None;
                    action = self.submit();
                }
                KeyCode::Backspace => {
                    self.vim.count = None;
                    self.cursor = self.cursor.saturating_sub(1);
                }
                // Esc keeps wizard's escape hatches (close diff, dismiss
                // todos, reset scroll) since Normal-mode Esc would otherwise
                // be a no-op.
                KeyCode::Esc => {
                    self.vim.clear_pending();
                    if self.show_diff {
                        self.show_diff = false;
                        self.diff_scroll = 0;
                    } else if self.show_todos {
                        // Then the todo sidebar (it auto-opens on the first
                        // todo update, so it needs a way out that isn't
                        // `/todos`).
                        self.show_todos = false;
                    } else if !self.scroll_follow {
                        self.scroll_to_bottom();
                    }
                }
                KeyCode::PageUp => self.scroll_transcript(10),
                KeyCode::PageDown => self.scroll_transcript(-10),
                _ => self.vim.count = None,
            }
        }
        self.sync_input_mode();
        Ok(action)
    }

    // --- input history (↑/↓ recall, shell-style) ---

    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let pos = match self.history_pos {
            None => {
                self.history_draft = self.input.clone();
                self.history.len() - 1
            }
            Some(0) => return,
            Some(pos) => pos - 1,
        };
        self.set_input(self.history[pos].clone());
        self.history_pos = Some(pos);
    }

    fn history_next(&mut self) {
        match self.history_pos {
            None => {}
            Some(pos) if pos + 1 < self.history.len() => {
                self.set_input(self.history[pos + 1].clone());
                self.history_pos = Some(pos + 1);
            }
            Some(_) => {
                let draft = std::mem::take(&mut self.history_draft);
                self.set_input(draft);
                self.history_pos = None;
            }
        }
    }

    /// Move any in-flight streaming text into the transcript. Reasoning
    /// flushes first: it streams before the visible reply.
    fn flush_streaming(&mut self) {
        if !self.streaming_thinking.is_empty() {
            let text = std::mem::take(&mut self.streaming_thinking);
            self.transcript.push(TranscriptEntry::Thinking(text));
        }
        if !self.streaming.is_empty() {
            let text = std::mem::take(&mut self.streaming);
            self.transcript.push(TranscriptEntry::Assistant(text));
        }
    }

    /// Toggle the expansion of the most recent finished tool card (Ctrl-T).
    fn toggle_last_tool_card(&mut self) {
        for entry in self.transcript.iter_mut().rev() {
            if let TranscriptEntry::ToolCard { collapsed, .. } = entry {
                *collapsed = !*collapsed;
                return;
            }
        }
    }

    /// Toggle the tool card whose header line was drawn on screen row `row`
    /// in the last frame (a plain click on it). No-op off-card, on a
    /// still-running card, or while an overlay covers the transcript (the
    /// hit map is empty then — see `card_hits`).
    fn toggle_card_at(&mut self, row: u16) {
        let hit = self
            .card_hits
            .borrow()
            .iter()
            .find(|(y, _)| *y == row)
            .map(|(_, index)| *index);
        if let Some(index) = hit
            && let Some(TranscriptEntry::ToolCard {
                output, collapsed, ..
            }) = self.transcript.get_mut(index)
            && output.is_some()
        {
            *collapsed = !*collapsed;
        }
    }

    /// Dispatch one event from the merged stream. Returns the user action
    /// the main loop must perform (start a turn, run a slash command, ...).
    pub fn handle_event(&mut self, event: Event) -> Result<Option<AppAction>> {
        match event {
            Event::Key(key) => self.handle_key(key),
            Event::Mouse(mouse) => {
                let cell = (mouse.column, mouse.row);
                match mouse.kind {
                    MouseEventKind::ScrollUp => {
                        self.scroll_transcript(3);
                        // The content under each cell just moved, so the old
                        // selection no longer maps to it.
                        self.selection = None;
                    }
                    MouseEventKind::ScrollDown => {
                        self.scroll_transcript(-3);
                        self.selection = None;
                    }
                    MouseEventKind::Down(MouseButton::Left) => {
                        self.selection = Some(Selection {
                            anchor: cell,
                            head: cell,
                            dragging: true,
                        });
                    }
                    MouseEventKind::Drag(MouseButton::Left) => {
                        if let Some(sel) = self.selection.as_mut() {
                            sel.head = cell;
                        }
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        if let Some(sel) = self.selection.as_mut() {
                            sel.head = cell;
                            sel.dragging = false;
                            if sel.is_empty() {
                                // A plain click (no drag) clears any previous
                                // selection, and on a tool-card header line
                                // toggles that card's output.
                                self.selection = None;
                                self.toggle_card_at(cell.1);
                            } else {
                                // Hand off to the main loop: it owns the
                                // terminal, so it reads the rendered cells and
                                // copies them.
                                return Ok(Some(AppAction::CopySelection));
                            }
                        }
                    }
                    _ => {}
                }
                Ok(None)
            }
            Event::Paste(text) => {
                self.handle_paste(&text);
                Ok(None)
            }
            Event::Resize(_, _) => Ok(None),
            Event::Tick => {
                self.tick = self.tick.wrapping_add(1);
                // Keep the dashboard's session list current while it's open.
                if self.show_dashboard {
                    // List is cheap (small files); peek reads a transcript tail
                    // so poll it less often.
                    if self.tick.is_multiple_of(4) {
                        self.refresh_sessions();
                    }
                    if self.tick.is_multiple_of(10) {
                        self.refresh_peek();
                    }
                }
                // Age finished runs off the rail (the tick is ~100ms, so this
                // is a cheap retain over a handful of panes).
                self.retire_finished_panes();
                Ok(None)
            }
            Event::Agent(agent_event) => {
                self.handle_agent_event(agent_event);
                Ok(None)
            }
            Event::Notice(message) => {
                self.notice(message);
                Ok(None)
            }
            // Owned by the main loop (it holds the agent slot / config); never
            // reach here.
            Event::AgentRebuilt(_)
            | Event::ProviderActivated(_)
            | Event::McpConnected { .. }
            | Event::ProviderHealthFailed(_) => Ok(None),
        }
    }

    /// Keyboard handling for the current [`InputMode`]. Priority: global
    /// chords, open picker, then line editing.
    pub fn handle_key(&mut self, key: KeyEvent) -> Result<Option<AppAction>> {
        if key.kind == KeyEventKind::Release {
            return Ok(None);
        }

        // Any keystroke dismisses a lingering text selection (it was copied on
        // release; the highlight is just a leftover once the user moves on).
        self.selection = None;

        // Any key other than Ctrl-C disarms the "press again to exit" latch.
        let is_ctrl_c =
            key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c');
        if !is_ctrl_c {
            self.ctrl_c_armed = false;
        }

        // Global chords, regardless of input mode.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => {
                    // Once interrupts a running turn; pressed again (while
                    // armed) it exits. Idle: first press arms, second exits.
                    if self.ctrl_c_armed {
                        self.should_quit = true;
                        return Ok(None);
                    }
                    self.ctrl_c_armed = true;
                    if self.status.busy {
                        self.notice("interrupting… (Ctrl-C again to exit)");
                        return Ok(Some(AppAction::Interrupt));
                    }
                    self.notice("press Ctrl-C again to exit");
                    return Ok(None);
                }
                KeyCode::Char('d') => {
                    self.should_quit = true;
                    return Ok(None);
                }
                // Ctrl-End jumps the transcript (or attached pane) to the live
                // tail and re-enables stick-to-bottom after reading history
                // during a long stream.
                KeyCode::End => {
                    self.scroll_to_bottom();
                    return Ok(None);
                }
                KeyCode::Char('u') => {
                    // Readline-style: kill from the line start to the cursor.
                    let index = self.byte_index();
                    self.input.drain(..index);
                    self.cursor = 0;
                    self.sync_input_mode();
                    return Ok(None);
                }
                KeyCode::Char('w') => {
                    self.delete_word_back();
                    self.sync_input_mode();
                    return Ok(None);
                }
                KeyCode::Char('a') => {
                    self.cursor = 0;
                    return Ok(None);
                }
                KeyCode::Char('e') => {
                    self.cursor = self.input.chars().count();
                    return Ok(None);
                }
                KeyCode::Char('k') => {
                    let index = self.byte_index();
                    self.input.truncate(index);
                    self.sync_input_mode();
                    return Ok(None);
                }
                KeyCode::Char('t') => {
                    self.toggle_last_tool_card();
                    return Ok(None);
                }
                KeyCode::Char('p') => {
                    // Shortcut for the interactive model picker; ignored
                    // while a turn runs.
                    if self.status.busy {
                        return Ok(None);
                    }
                    return Ok(Some(AppAction::Command(SlashCommand::Model(None))));
                }
                _ => {}
            }
        }

        // The dashboard is modal: ↑/↓ move the selection, typing fills the
        // dispatch input, Enter dispatches a background session, Ctrl-X stops
        // the selected one, Esc clears the input or closes. (Enter will also
        // attach to the selected session once the supervisor lands.)
        if self.show_dashboard {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Up | KeyCode::BackTab => self.dashboard_select(-1),
                KeyCode::Down | KeyCode::Tab => self.dashboard_select(1),
                KeyCode::Char('x') if ctrl => self.stop_selected_session(),
                KeyCode::Enter => {
                    let prompt = self.dashboard_input.trim().to_string();
                    if !prompt.is_empty() {
                        self.dashboard_input.clear();
                        self.dispatch_session(prompt);
                    }
                }
                KeyCode::Backspace => {
                    self.dashboard_input.pop();
                }
                KeyCode::Esc => {
                    if self.dashboard_input.is_empty() {
                        self.show_dashboard = false;
                    } else {
                        self.dashboard_input.clear();
                    }
                }
                KeyCode::Char(c) if !ctrl => self.dashboard_input.push(c),
                _ => {}
            }
            return Ok(None);
        }

        // Inside a subagent's pane. Its transcript has taken over the screen,
        // so navigation keys scroll *it* — but the composer stays live
        // underneath, so anything else falls through to normal typing and you
        // can keep driving the main conversation while you watch.
        if let Some(index) = self.attached {
            // Every navigation key is captured here. Letting an arrow fall
            // through to the composer underneath would scroll the *main*
            // chat's history while the user is plainly looking at a pane —
            // the keys have to belong to what is on screen.
            match key.code {
                KeyCode::Esc => {
                    self.detach_pane();
                    return Ok(None);
                }
                // Plain ↑/↓ keep doing what they did on the rail: walk the
                // subagents. Opening one is not supposed to end the browse —
                // you keep arrowing and each run takes over the screen in
                // turn, wrapping around, so there is never a reason to back
                // out to the rail just to see the next one.
                KeyCode::Up if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                    self.step_pane(index, -1);
                    return Ok(None);
                }
                KeyCode::Down if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                    self.step_pane(index, 1);
                    return Ok(None);
                }
                // Scrolling the run you are reading moves to Shift+↑/↓ (and
                // PageUp/PageDown below).
                KeyCode::Up => {
                    self.scroll_pane(index, 1);
                    return Ok(None);
                }
                KeyCode::Down => {
                    self.scroll_pane(index, -1);
                    return Ok(None);
                }
                KeyCode::PageUp => {
                    self.scroll_pane(index, 10);
                    return Ok(None);
                }
                KeyCode::PageDown => {
                    self.scroll_pane(index, -10);
                    return Ok(None);
                }
                KeyCode::Char('x') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.kill_pane(index);
                    return Ok(None);
                }
                _ => {}
            }
        }

        // The rail has keyboard focus: ↑/↓ move between subagent dots, Enter
        // opens the selected one, Esc drops back to the composer.
        if let Some(index) = self.rail_focus
            && self.attached.is_none()
        {
            match key.code {
                // Arrows only — no j/k. The rail is a focus you land in from a
                // live text composer, so every letter has to fall through and
                // be typed; binding letters here would eat the first character
                // of "just do X".
                KeyCode::Up => {
                    self.rail_select(-1);
                    return Ok(None);
                }
                KeyCode::Down => {
                    self.rail_select(1);
                    return Ok(None);
                }
                KeyCode::Enter => {
                    self.attach_pane(index);
                    return Ok(None);
                }
                KeyCode::Char('x') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.kill_pane(index);
                    return Ok(None);
                }
                KeyCode::Esc => {
                    self.rail_focus = None;
                    return Ok(None);
                }
                // Anything else means the user is done browsing and wants to
                // type: hand focus back to the composer and let the key land
                // there, so you never lose a keystroke to the rail.
                _ => self.rail_focus = None,
            }
        }

        // An open plan review captures all keys: the turn is paused inside
        // exit_plan until a verdict is sent.
        if self.plan_review.is_some() {
            self.handle_plan_review_key(key);
            return Ok(None);
        }

        // An open interview captures all keys: the turn is paused inside the
        // interview tool until the user answers or dismisses it.
        if self.interview.is_some() {
            self.handle_interview_key(key);
            return Ok(None);
        }

        // An open picker captures navigation keys.
        if let Some(picker) = self.picker.as_mut() {
            match key.code {
                KeyCode::Up | KeyCode::BackTab | KeyCode::Char('k') => {
                    picker.selected = if picker.selected == 0 {
                        picker.items.len().saturating_sub(1)
                    } else {
                        picker.selected - 1
                    };
                }
                KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') => {
                    picker.selected = if picker.selected + 1 >= picker.items.len() {
                        0
                    } else {
                        picker.selected + 1
                    };
                }
                // Space toggles a checkbox row in a multi-select picker.
                KeyCode::Char(' ')
                    if matches!(
                        picker.kind,
                        PickerKind::ClaudeImport | PickerKind::FusionPanel
                    ) =>
                {
                    if let Some(item) = picker.items.get_mut(picker.selected) {
                        item.current = !item.current;
                    }
                }
                KeyCode::Esc => {
                    self.picker = None;
                }
                KeyCode::Enter => {
                    let picker = self.picker.take().expect("picker is open");
                    let Some(item) = picker.items.get(picker.selected) else {
                        return Ok(None);
                    };
                    let action = match picker.kind {
                        PickerKind::Model => {
                            AppAction::Command(SlashCommand::Model(Some(item.value.clone())))
                        }
                        PickerKind::Mode => {
                            let mode = if item.value == "sovereign" {
                                Mode::Sovereign
                            } else {
                                Mode::Genie
                            };
                            AppAction::Command(SlashCommand::Mode(Some(mode)))
                        }
                        PickerKind::Effort => {
                            let effort = match item.value.as_str() {
                                "low" => Some(ReasoningEffort::Low),
                                "medium" => Some(ReasoningEffort::Medium),
                                "high" => Some(ReasoningEffort::High),
                                _ => None,
                            };
                            AppAction::Command(SlashCommand::Effort(Some(effort)))
                        }
                        PickerKind::Rewind => {
                            // Item values are always turn ids we formatted.
                            let Ok(turn) = item.value.parse::<u64>() else {
                                return Ok(None);
                            };
                            AppAction::Command(SlashCommand::Rewind(Some(turn)))
                        }
                        PickerKind::Resume => {
                            // Item values are session ids.
                            AppAction::Command(SlashCommand::Resume(Some(item.value.clone())))
                        }
                        PickerKind::Subagent => {
                            // Subagents are spawned by the model, not run as a
                            // command. Pre-fill a delegation request so the user
                            // just types the task and submits.
                            self.set_input(format!("Use the {} subagent to ", item.value));
                            return Ok(None);
                        }
                        PickerKind::Settings => {
                            let selected = picker.selected;
                            return Ok(self.apply_setting(selected));
                        }
                        PickerKind::ClaudeImport => {
                            // Build the selection from the toggled rows (order
                            // matches `open_claude_import_picker`: mcp, commands,
                            // verbs) and hand off to the command handler, which
                            // has the live MCP manager.
                            let flags: Vec<bool> = picker.items.iter().map(|i| i.current).collect();
                            let selection = ImportSelection {
                                mcp: flags.first().copied().unwrap_or(false),
                                commands: flags.get(1).copied().unwrap_or(false),
                                verbs: flags.get(2).copied().unwrap_or(false),
                            };
                            if selection.is_empty() {
                                self.notice("nothing selected to import");
                                return Ok(None);
                            }
                            AppAction::Command(SlashCommand::ImportClaude(selection))
                        }
                        PickerKind::FusionPanel => {
                            // Panel = toggled rows; the first becomes the
                            // synthesizer (the sole tool-caller). Persist
                            // [fusion]; the new config takes effect next time
                            // /fusion turns on.
                            let panel: Vec<String> = picker
                                .items
                                .iter()
                                .filter(|i| i.current)
                                .map(|i| i.value.clone())
                                .collect();
                            if panel.is_empty() {
                                self.notice(
                                    "select at least one provider for the panel (Space toggles)",
                                );
                                return Ok(None);
                            }
                            let synthesizer = panel[0].clone();
                            let rounds = self.config.fusion.as_ref().map(|f| f.rounds).unwrap_or(1);
                            self.config.fusion = Some(crate::config::FusionConfig {
                                panel: panel.clone(),
                                synthesizer: synthesizer.clone(),
                                rounds,
                            });
                            if let Err(err) = self.config.save() {
                                self.notice(format!("could not save fusion config: {err:#}"));
                                return Ok(None);
                            }
                            let tail = if self.fusion_active {
                                " — /fusion off then on to apply"
                            } else {
                                " — /fusion to turn on"
                            };
                            self.notice(format!(
                                "fusion: {} · synthesizer {synthesizer} · {rounds} round(s){tail}",
                                panel.join("+")
                            ));
                            return Ok(None);
                        }
                        PickerKind::Provider => {
                            // The final row opens the add-provider type menu;
                            // every other row switches to that provider.
                            if picker.selected + 1 == picker.items.len()
                                || item.value == PROVIDER_ADD_ROW
                            {
                                self.open_provider_type_picker();
                                return Ok(None);
                            }
                            AppAction::Command(SlashCommand::Provider(ProviderAction::Use(
                                item.value.clone(),
                            )))
                        }
                        PickerKind::ProviderType => {
                            use crate::llm::{cloudflare, openrouter, xai_oauth};
                            use std::collections::VecDeque;
                            match picker.selected {
                                // xAI sign-in: run the OAuth flow; login()
                                // auto-adds the provider on success.
                                0 => {
                                    return Ok(Some(AppAction::Command(SlashCommand::Login(
                                        "xai".to_string(),
                                    ))));
                                }
                                // xAI API key.
                                1 => {
                                    self.begin_provider_prompt(ProviderPrompt {
                                        kind: ProviderKind::Xai,
                                        name: "xai".to_string(),
                                        base_url: xai_oauth::DEFAULT_BASE_URL.to_string(),
                                        model: xai_oauth::DEFAULT_MODEL.to_string(),
                                        api_key: None,
                                        queue: VecDeque::from([PromptField::ApiKey]),
                                    });
                                }
                                // OpenRouter — model is unknown, so prompt for
                                // it alongside the key.
                                2 => {
                                    self.begin_provider_prompt(ProviderPrompt {
                                        kind: ProviderKind::OpenRouter,
                                        name: "openrouter".to_string(),
                                        base_url: openrouter::DEFAULT_BASE_URL.to_string(),
                                        model: String::new(),
                                        api_key: None,
                                        queue: VecDeque::from([
                                            PromptField::Model,
                                            PromptField::ApiKey,
                                        ]),
                                    });
                                }
                                // Cloudflare Workers AI — account id (folded
                                // into the base URL) + token; model defaults to
                                // GLM 5.2 and can be changed later via /model.
                                3 => {
                                    self.begin_provider_prompt(ProviderPrompt {
                                        kind: ProviderKind::Cloudflare,
                                        name: "cloudflare".to_string(),
                                        base_url: cloudflare::BASE_URL_TEMPLATE.to_string(),
                                        model: cloudflare::DEFAULT_MODEL.to_string(),
                                        api_key: None,
                                        queue: VecDeque::from([
                                            PromptField::AccountId,
                                            PromptField::ApiKey,
                                        ]),
                                    });
                                }
                                // OpenAI — model + key.
                                4 => {
                                    self.begin_provider_prompt(ProviderPrompt {
                                        kind: ProviderKind::Openai,
                                        name: "openai".to_string(),
                                        base_url: "https://api.openai.com/v1".to_string(),
                                        model: String::new(),
                                        api_key: None,
                                        queue: VecDeque::from([
                                            PromptField::Model,
                                            PromptField::ApiKey,
                                        ]),
                                    });
                                }
                                // Anthropic — model + key.
                                5 => {
                                    self.begin_provider_prompt(ProviderPrompt {
                                        kind: ProviderKind::Anthropic,
                                        name: "claude".to_string(),
                                        base_url: "https://api.anthropic.com".to_string(),
                                        model: String::new(),
                                        api_key: None,
                                        queue: VecDeque::from([
                                            PromptField::Model,
                                            PromptField::ApiKey,
                                        ]),
                                    });
                                }
                                // OpenAI-compatible custom — everything is
                                // prompted, starting with the name.
                                6 => {
                                    self.begin_provider_prompt(ProviderPrompt {
                                        kind: ProviderKind::Openai,
                                        name: String::new(),
                                        base_url: String::new(),
                                        model: String::new(),
                                        api_key: None,
                                        queue: VecDeque::from([
                                            PromptField::Name,
                                            PromptField::BaseUrl,
                                            PromptField::Model,
                                            PromptField::ApiKey,
                                        ]),
                                    });
                                }
                                _ => {}
                            }
                            return Ok(None);
                        }
                        PickerKind::WebBackend => {
                            let id = item.value.clone();
                            self.select_web_backend(&id);
                            return Ok(None);
                        }
                    };
                    return Ok(Some(action));
                }
                _ => {}
            }
            return Ok(None);
        }

        // In the inline provider-setup prompt, Esc cancels; every other key
        // falls through to normal line editing and the Enter→submit path
        // (which `submit` routes to `submit_prompt_field`).
        if (self.prompt.is_some() || self.web_key_backend.is_some()) && key.code == KeyCode::Esc {
            self.cancel_prompt();
            return Ok(None);
        }

        // Modal (vim) editing. In Normal mode keys are motions/operators, not
        // text; in Insert mode the only extra binding is Esc → Normal, and
        // everything else falls through to ordinary line editing below.
        if self.vim.enabled {
            match self.vim.mode {
                VimMode::Normal => return self.handle_vim_normal(key),
                VimMode::Insert => {
                    if key.code == KeyCode::Esc
                        && !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    {
                        self.enter_normal_mode();
                        return Ok(None);
                    }
                }
            }
        }

        let suggesting = !self.suggestions.is_empty();
        let action = match key.code {
            // Shift+Enter (terminals with keyboard enhancement) or Alt+Enter
            // (the fallback elsewhere) inserts a newline instead of submitting.
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
            {
                self.insert_newline();
                None
            }
            KeyCode::Enter => self.submit(),
            KeyCode::Backspace => {
                self.delete_back();
                None
            }
            KeyCode::Delete => {
                self.delete_forward();
                None
            }
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                None
            }
            KeyCode::Right => {
                let len = self.input.chars().count();
                if self.cursor < len {
                    self.cursor += 1;
                } else if suggesting {
                    // Cursor at the end: → accepts the ghost-text prediction.
                    self.accept_suggestion();
                }
                None
            }
            KeyCode::Home => {
                self.cursor = 0;
                None
            }
            KeyCode::End => {
                self.cursor = self.input.chars().count();
                None
            }
            KeyCode::Tab => {
                if suggesting {
                    self.accept_suggestion();
                } else {
                    self.complete_at_path();
                }
                None
            }
            // Shift+Tab toggles plan mode (same as /plan, welcome screen
            // included).
            KeyCode::BackTab => {
                self.welcome_dismissed = true;
                Some(AppAction::Command(SlashCommand::Plan))
            }
            KeyCode::Esc => {
                if self.show_diff {
                    // Esc closes the diff sidebar before touching the input.
                    self.show_diff = false;
                    self.diff_scroll = 0;
                } else if self.show_todos {
                    // Then the todo sidebar (it auto-opens on the first todo
                    // update, so it needs a way out that isn't `/todos`).
                    self.show_todos = false;
                } else if !self.scroll_follow {
                    self.scroll_to_bottom();
                } else {
                    self.clear_input();
                }
                None
            }
            // While the diff sidebar is open it owns paging: read a long diff
            // top-to-bottom (offset from the top). Otherwise PgUp/PgDn scroll
            // the transcript; leaving the bottom freezes the viewport while
            // output streams, returning to it re-enables stick-to-bottom.
            KeyCode::PageUp if self.show_diff => {
                self.diff_scroll = self.diff_scroll.saturating_sub(10);
                None
            }
            KeyCode::PageDown if self.show_diff => {
                self.diff_scroll = self.diff_scroll.saturating_add(10);
                None
            }
            KeyCode::PageUp => {
                self.scroll_transcript(10);
                None
            }
            KeyCode::PageDown => {
                self.scroll_transcript(-10);
                None
            }
            KeyCode::Up => {
                // While browsing history, ↑/↓ keep navigating history even
                // when a recalled slash command repopulates suggestions.
                if suggesting && self.history_pos.is_none() {
                    self.suggestion_index = if self.suggestion_index == 0 {
                        self.suggestions.len() - 1
                    } else {
                        self.suggestion_index - 1
                    };
                } else {
                    self.history_prev();
                }
                None
            }
            KeyCode::Down => {
                if suggesting && self.history_pos.is_none() {
                    self.suggestion_index = if self.suggestion_index + 1 >= self.suggestions.len() {
                        0
                    } else {
                        self.suggestion_index + 1
                    };
                } else if self.history_pos.is_some() {
                    // Mid-history: ↓ keeps walking forward through it.
                    self.history_next();
                } else if !self.focus_rail() {
                    // Past the end of history with no subagents to drop into:
                    // ↓ is a no-op, which is what history_next already does.
                    self.history_next();
                }
                None
            }
            // Ctrl-G drafts the prompt in an external editor (handled by the
            // main loop, which owns the terminal). Masked key entry stays
            // inline — a secret must not land in a temp file.
            KeyCode::Char('g')
                if key.modifiers.contains(KeyModifiers::CONTROL) && !self.prompt_is_masked() =>
            {
                self.pending_edit_prompt = true;
                None
            }
            KeyCode::Char(c) => {
                // Unbound Ctrl/Alt chords must not insert their literal char.
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                {
                    self.insert_char(c);
                }
                None
            }
            _ => None,
        };
        self.sync_input_mode();
        Ok(action)
    }

    /// Enter pressed: complete the highlighted suggestion if the command is
    /// still partial, then parse the input line into an action.
    fn submit(&mut self) -> Option<AppAction> {
        // The inline prompts intercept Enter: each submission is an answer to a
        // field (provider setup) or a pasted web-search key, not a message.
        if self.web_key_backend.is_some() {
            return self.submit_web_key();
        }
        if self.prompt.is_some() {
            return self.submit_prompt_field();
        }
        if self.input_mode == InputMode::Command && !self.suggestions.is_empty() {
            let typed = self
                .input
                .trim_start()
                .strip_prefix('/')
                .unwrap_or_default()
                .to_string();
            let spec =
                self.suggestions[self.suggestion_index.min(self.suggestions.len() - 1)].clone();
            // An exactly-typed command always runs as typed; otherwise Enter
            // completes the highlighted suggestion first.
            let exact = COMMANDS.iter().any(|command| command.name == typed)
                || self.custom_commands.iter().any(|c| c.name == typed);
            if !exact && typed != spec.name {
                let takes_args = spec.takes_args;
                self.accept_suggestion();
                if takes_args {
                    // Completed to "/evolve " — wait for the arguments.
                    return None;
                }
            }
        }

        let input = self.input.trim().to_string();
        if input.is_empty() {
            return None;
        }
        match SlashCommand::parse(&input) {
            Some(Ok(command)) => {
                // A dispatched command counts as activity even though it adds
                // no transcript entries; drop the welcome screen.
                self.welcome_dismissed = true;
                self.push_history(&input);
                self.clear_input();
                Some(AppAction::Command(command))
            }
            Some(Err(message)) => {
                let word = input
                    .trim_start()
                    .strip_prefix('/')
                    .unwrap_or_default()
                    .split_whitespace()
                    .next()
                    .unwrap_or_default();
                // A known builtin with bad arguments keeps its usage notice;
                // custom commands and unknown `/words` go to the model (the
                // custom expansion happens in `submit_prompt`).
                if is_builtin_command(word) {
                    self.welcome_dismissed = true;
                    self.push_history(&input);
                    self.clear_input();
                    self.notice(message);
                    None
                } else {
                    self.submit_prompt(input)
                }
            }
            None => self.submit_prompt(input),
        }
    }

    /// Submit `input` as a user prompt: record it verbatim in history and
    /// the transcript, hand the preprocessed form (custom-command and `@file`
    /// expansion, plus any staged image attachments) to the agent.
    fn submit_prompt(&mut self, input: String) -> Option<AppAction> {
        if self.status.busy {
            // Rejected input never ran; do not record it in history.
            self.notice("the agent is busy — wait for the current turn to finish");
            return None;
        }
        if self.rebuilding.is_some() {
            self.notice("the agent is rebuilding — try again in a moment");
            return None;
        }
        let mut prepared =
            crate::commands::preprocess(&input, &self.custom_commands, &self.project_root);
        // Merge staged paste attachments; prefer absolute unique paths.
        for path in self.pending_images.drain(..) {
            if !prepared.images.iter().any(|p| p == &path) {
                prepared.images.push(path);
            }
        }
        self.push_history(&input);
        self.clear_input();
        self.transcript.push(TranscriptEntry::User(input));
        self.scroll_to_bottom();
        Some(AppAction::Submit(prepared))
    }

    /// Handle a bracketed paste: stage image file paths / data-URL images as
    /// attachments, otherwise insert the text into the composer.
    fn handle_paste(&mut self, text: &str) {
        // data:image/...;base64,... → write under ~/.wizard/attachments and attach.
        if let Some((mime, b64)) = parse_data_image_url(text.trim()) {
            match save_pasted_image_bytes(mime, b64) {
                Ok(path) => {
                    let name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("image")
                        .to_string();
                    self.stage_image(path, &name);
                }
                Err(err) => self.notice(format!("could not save pasted image: {err}")),
            }
            self.sync_input_mode();
            return;
        }

        // One or more existing image paths (whitespace / newline separated).
        let tokens: Vec<&str> = text.split_whitespace().filter(|t| !t.is_empty()).collect();
        if !tokens.is_empty() && tokens.iter().all(|t| looks_like_image_path_token(t)) {
            let mut any = false;
            for token in tokens {
                if let Some(path) = resolve_pasted_image_path(token, &self.project_root) {
                    let name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(token)
                        .to_string();
                    self.stage_image(path, &name);
                    any = true;
                }
            }
            if any {
                self.sync_input_mode();
                return;
            }
        }

        self.insert_str(text);
        self.sync_input_mode();
    }

    /// Stage `path` for the next submit and insert a visible `[image: name]` token.
    fn stage_image(&mut self, path: PathBuf, name: &str) {
        if !self.pending_images.iter().any(|p| p == &path) {
            self.pending_images.push(path);
        }
        let token = format!("[image: {name}]");
        if !self.input.is_empty() && !self.input.chars().last().is_some_and(|c| c.is_whitespace()) {
            self.insert_char(' ');
        }
        self.insert_str(&token);
        self.notice(format!("attached {name}"));
    }

    /// Minimal Tab path-completion for `@path` tokens: complete the token
    /// under the cursor from its directory listing (longest common prefix;
    /// a unique directory match gains a trailing `/`).
    fn complete_at_path(&mut self) {
        let chars: Vec<char> = self.input.chars().collect();
        let mut start = self.cursor.min(chars.len());
        while start > 0 && !chars[start - 1].is_whitespace() {
            start -= 1;
        }
        let token: String = chars[start..self.cursor.min(chars.len())].iter().collect();
        let Some(partial) = token.strip_prefix('@') else {
            return;
        };
        if partial.starts_with('@') {
            return;
        }
        // Split the partial path into the directory to list and the name
        // prefix to match.
        let (dir_part, prefix) = match partial.rfind('/') {
            Some(slash) => (&partial[..=slash], &partial[slash + 1..]),
            None => ("", partial),
        };
        let expanded = shellexpand::tilde(dir_part);
        let dir_path = Path::new(expanded.as_ref());
        let dir = if dir_path.is_absolute() {
            dir_path.to_path_buf()
        } else {
            self.project_root.join(dir_path)
        };
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return;
        };
        let mut matches: Vec<(String, bool)> = entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_str()?.to_string();
                let is_dir = entry.file_type().ok()?.is_dir();
                name.starts_with(prefix).then_some((name, is_dir))
            })
            .collect();
        matches.sort();
        let completion = match matches.as_slice() {
            [] => return,
            [(name, is_dir)] => {
                let mut full = name.clone();
                if *is_dir {
                    full.push('/');
                }
                full
            }
            many => {
                let mut common = many[0].0.clone();
                for (name, _) in &many[1..] {
                    let shared = common
                        .char_indices()
                        .zip(name.chars())
                        .take_while(|((_, a), b)| a == b)
                        .count();
                    common = common.chars().take(shared).collect();
                }
                common
            }
        };
        if completion.len() <= prefix.len() {
            return;
        }
        self.insert_str(&completion[prefix.len()..]);
    }

    /// Keys while the plan-review modal is open. Review state: `y`/Enter
    /// approves, `n` opens a feedback line, ↑/↓/PgUp/PgDn scroll the plan.
    /// Feedback state: typing edits, Enter sends the rejection, Esc returns
    /// to the review.
    fn handle_plan_review_key(&mut self, key: KeyEvent) {
        let Some(review) = self.plan_review.as_mut() else {
            return;
        };
        if let Some(feedback) = review.feedback.as_mut() {
            match key.code {
                KeyCode::Enter => {
                    let feedback = review.feedback.take().unwrap_or_default();
                    self.finish_plan_review(PlanVerdict::reject(feedback));
                }
                KeyCode::Esc => review.feedback = None,
                KeyCode::Backspace => {
                    feedback.pop();
                }
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    feedback.push(c);
                }
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Char('y') | KeyCode::Enter => {
                self.finish_plan_review(PlanVerdict::approve());
            }
            KeyCode::Char('n') => review.feedback = Some(String::new()),
            KeyCode::Up | KeyCode::Char('k') => review.scroll = review.scroll.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => review.scroll = review.scroll.saturating_add(1),
            KeyCode::PageUp => review.scroll = review.scroll.saturating_sub(10),
            KeyCode::PageDown => review.scroll = review.scroll.saturating_add(10),
            _ => {}
        }
    }

    /// Close the plan review and send `verdict` back into the paused
    /// `exit_plan` call. Approval mirrors the agent clearing its plan-mode
    /// flag; rejection stays in plan mode.
    fn finish_plan_review(&mut self, verdict: PlanVerdict) {
        let Some(mut review) = self.plan_review.take() else {
            return;
        };
        let approved = verdict.approved;
        if let Some(respond) = review.respond.take() {
            let _ = respond.send(verdict);
        }
        if approved {
            self.plan_mode = false;
            self.notice("plan approved — executing it");
        } else {
            self.notice("plan rejected — still in plan mode");
        }
    }

    /// Drive the interview modal: number keys pick a suggested option, typing
    /// composes a free-text answer, Enter commits the current question and
    /// advances (committing the last one sends every answer back), and Esc
    /// dismisses the whole interview (the model proceeds on its own judgment).
    fn handle_interview_key(&mut self, key: KeyEvent) {
        let Some(interview) = self.interview.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => {
                self.finish_interview(None);
            }
            KeyCode::Enter => {
                // Commit the current answer (the typed text wins; empty means
                // "skip this one") and advance.
                let answer = interview.input.trim().to_string();
                interview.answers.push(answer);
                interview.input.clear();
                interview.current += 1;
                if interview.current >= interview.questions.len() {
                    let answers = std::mem::take(&mut interview.answers);
                    self.finish_interview(Some(answers));
                }
            }
            KeyCode::Backspace => {
                interview.input.pop();
            }
            // 1-9 fill the input with the matching suggested option, so the
            // user can accept it with Enter or edit it first.
            KeyCode::Char(c)
                if c.is_ascii_digit()
                    && c != '0'
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                let idx = (c as u8 - b'1') as usize;
                if let Some(option) = interview
                    .current_question()
                    .and_then(|q| q.options.get(idx))
                {
                    interview.input = option.clone();
                } else {
                    interview.input.push(c);
                }
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                interview.input.push(c);
            }
            _ => {}
        }
    }

    /// Close the interview and send `answers` back into the paused
    /// `interview` call: `Some(answers)` aligned with the questions, or `None`
    /// when the user dismissed it (the model then uses its best judgment).
    fn finish_interview(&mut self, answers: Option<Vec<String>>) {
        let Some(mut interview) = self.interview.take() else {
            return;
        };
        let answered = answers.is_some();
        if let Some(respond) = interview.respond.take() {
            let _ = respond.send(answers);
        }
        self.notice(if answered {
            "answers sent — the agent is finishing its plan"
        } else {
            "interview dismissed — the agent will use its best judgment"
        });
    }

    /// Record a submitted input for ↑/↓ recall (skipping immediate repeats).
    fn push_history(&mut self, input: &str) {
        if self.history.last().map(String::as_str) != Some(input) {
            self.history.push(input.to_string());
        }
    }

    /// Fold an agent event into the transcript / status.
    pub fn handle_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::TextDelta(delta) => {
                self.streaming.push_str(&delta);
            }
            AgentEvent::ThinkingDelta(delta) => {
                self.streaming_thinking.push_str(&delta);
            }
            AgentEvent::ToolStarted { name, args } => {
                self.flush_streaming();
                self.transcript.push(TranscriptEntry::ToolCard {
                    name,
                    args,
                    output: None,
                    is_error: false,
                    collapsed: false,
                });
            }
            AgentEvent::ToolFinished { name, output } => {
                let card = self
                    .transcript
                    .iter_mut()
                    .rev()
                    .find_map(|entry| match entry {
                        TranscriptEntry::ToolCard {
                            name: card_name,
                            output: slot,
                            is_error,
                            collapsed,
                            ..
                        } if *card_name == name && slot.is_none() => {
                            Some((slot, is_error, collapsed))
                        }
                        _ => None,
                    });
                match card {
                    Some((slot, is_error, collapsed)) => {
                        *is_error = output.is_error;
                        // Long successful outputs start collapsed, and so do
                        // errors — the ✗ glyph carries the signal without
                        // dumping the payload; a click or Ctrl-T expands it.
                        *collapsed = output.is_error || collapse_long(&output.content);
                        *slot = Some(output.content);
                    }
                    None => {
                        // No matching running card (e.g. denied before start
                        // was emitted) — record the result standalone.
                        let collapsed = output.is_error || collapse_long(&output.content);
                        self.transcript.push(TranscriptEntry::ToolCard {
                            name,
                            args: Value::Null,
                            output: Some(output.content),
                            is_error: output.is_error,
                            collapsed,
                        });
                    }
                }
            }
            AgentEvent::Images { source, images } => {
                // The model's own images arrive right after its reply, a tool's
                // right after that tool's card — so appending puts each one
                // under the thing that made it.
                self.flush_streaming();
                self.transcript.extend(image_entries(&source, images));
            }
            AgentEvent::StepCompleted { step } => {
                self.status.step = step;
            }
            AgentEvent::Error(message) => {
                self.flush_streaming();
                self.notice(format!("error: {message}"));
            }
            AgentEvent::Notice(message) => {
                self.flush_streaming();
                self.notice(message);
            }
            AgentEvent::StreamRetrying => {
                // The partial completion is being re-generated from scratch;
                // flushing it would double the text once the retry streams.
                self.streaming.clear();
                self.streaming_thinking.clear();
            }
            AgentEvent::HookFired {
                event,
                command,
                outcome,
            } => {
                self.notice(format!("hook {event}: {outcome} ({command})"));
            }
            AgentEvent::PlanReady { plan, respond } => {
                self.flush_streaming();
                // A plan awaiting review implies plan mode is on, however
                // the turn was started (e.g. `--plan`).
                self.plan_mode = true;
                self.plan_review = Some(PlanReview {
                    plan,
                    respond: Some(respond),
                    feedback: None,
                    scroll: 0,
                });
            }
            AgentEvent::Interview { questions, respond } => {
                self.flush_streaming();
                // Defensive: an empty set would leave the modal with nothing
                // to answer and the turn wedged — decline immediately.
                if questions.is_empty() {
                    let _ = respond.send(None);
                } else {
                    self.interview = Some(Interview {
                        questions,
                        answers: Vec::new(),
                        current: 0,
                        input: String::new(),
                        respond: Some(respond),
                    });
                }
            }
            AgentEvent::OmakaseProceeding { plan } => {
                self.flush_streaming();
                // Chef's choice: no review gate. Mirror the agent clearing its
                // flags and surface the plan it chose.
                self.plan_mode = false;
                self.omakase = false;
                self.transcript.push(TranscriptEntry::ToolCard {
                    name: "omakase plan (chef's choice)".to_string(),
                    args: Value::Null,
                    output: Some(plan),
                    is_error: false,
                    collapsed: false,
                });
                self.notice("omakase — chef's choice: proceeding with the agent's own plan");
            }
            AgentEvent::Usage {
                prompt_tokens,
                completion_tokens,
            } => {
                // Session lifetime totals (for /cost).
                self.status.prompt_tokens += prompt_tokens;
                self.status.completion_tokens += completion_tokens;
                // Context meter: the most recent prompt size *is* what the
                // next turn will load (history grows by completion tokens
                // too, but the next call's reported prompt will supersede
                // this; until then the last prompt is the best known figure).
                if prompt_tokens > 0 {
                    self.status.context_tokens = prompt_tokens;
                }
            }
            AgentEvent::ContextSize { tokens } => {
                // History just shrank (auto-compaction): replace the meter
                // with the post-compact estimate without touching /cost totals.
                self.status.context_tokens = tokens;
            }
            // TaskStarted is also mirrored to the gateway's JSON stream (see
            // output.rs); the TUI additionally bumps the live status-bar
            // counter (see draw_status_bar) so a running task stays visible
            // without waiting for the finish notice.
            AgentEvent::TaskStarted { .. } => {
                self.status.background_tasks += 1;
            }
            AgentEvent::TaskFinished {
                id,
                command,
                status,
            } => {
                self.status.background_tasks = self.status.background_tasks.saturating_sub(1);
                self.notice(format!(
                    "background task #{id} finished ({}): {command}",
                    status.describe()
                ));
            }
            // Same pattern as TaskStarted/TaskFinished above, for subagents
            // delegated with `background: true`.
            AgentEvent::SubagentStarted { .. } => {
                self.status.background_subagents += 1;
            }
            AgentEvent::SubagentFinished {
                id,
                name,
                task,
                completed,
                ..
            } => {
                self.status.background_subagents =
                    self.status.background_subagents.saturating_sub(1);
                self.notice(format!(
                    "background subagent #{id} '{name}' {}: {task}",
                    if completed {
                        "finished"
                    } else {
                        "hit its step budget"
                    }
                ));
            }
            // ---- The subagent rail --------------------------------------
            //
            // These carry a `run` id, so concurrent runs (even two of the same
            // subagent) each land in their own pane instead of interleaving
            // into the parent transcript.
            AgentEvent::SubagentRunStarted {
                run,
                bg,
                name,
                task,
            } => {
                self.panes.push(SubagentPane::new(run, bg, name, task));
            }
            AgentEvent::SubagentRunText { run, text } => {
                self.push_pane(run, TranscriptEntry::Assistant(text));
            }
            AgentEvent::SubagentRunToolStarted { run, name, args } => {
                self.push_pane(
                    run,
                    TranscriptEntry::ToolCard {
                        name,
                        args,
                        output: None,
                        is_error: false,
                        collapsed: false,
                    },
                );
            }
            AgentEvent::SubagentRunToolFinished { run, name, output } => {
                let Some(index) = self.pane_index(run) else {
                    return;
                };
                let card = self.panes[index]
                    .transcript
                    .iter_mut()
                    .rev()
                    .find_map(|entry| match entry {
                        TranscriptEntry::ToolCard {
                            name: card_name,
                            output: slot,
                            is_error,
                            collapsed,
                            ..
                        } if *card_name == name && slot.is_none() => {
                            Some((slot, is_error, collapsed))
                        }
                        _ => None,
                    });
                // Same collapse policy as the main transcript: errors and long
                // payloads land folded, so a pane stays skimmable.
                if let Some((slot, is_error, collapsed)) = card {
                    *is_error = output.is_error;
                    *collapsed = output.is_error || collapse_long(&output.content);
                    *slot = Some(output.content);
                }
            }
            AgentEvent::SubagentRunImages {
                run,
                source,
                images,
            } => {
                for entry in image_entries(&source, images) {
                    self.push_pane(run, entry);
                }
            }
            AgentEvent::SubagentRunStep { run, step } => {
                if let Some(index) = self.pane_index(run) {
                    self.panes[index].steps = step;
                }
            }
            AgentEvent::SubagentRunDone {
                run,
                completed,
                output,
                error,
                ..
            } => {
                let Some(index) = self.pane_index(run) else {
                    return;
                };
                let attached = self.attached == Some(index);
                let pane = &mut self.panes[index];
                pane.status = if completed {
                    PaneStatus::Done
                } else {
                    PaneStatus::Failed
                };
                pane.finished = Some(Instant::now());
                // The subagent's final message is the step that made no tool
                // call, so the sub-loop ends on it without streaming it — it
                // arrives here, as the report. Without this the pane would show
                // all of the work and none of the conclusion.
                if !output.trim().is_empty() {
                    let already_last = matches!(
                        pane.transcript.last(),
                        Some(TranscriptEntry::Assistant(text)) if text == &output
                    );
                    if !already_last {
                        pane.transcript
                            .push(TranscriptEntry::Assistant(output.clone()));
                    }
                }
                match &error {
                    Some(error) => pane
                        .transcript
                        .push(TranscriptEntry::Notice(format!("failed: {error}"))),
                    None if !completed => pane
                        .transcript
                        .push(TranscriptEntry::Notice("hit its step budget".to_string())),
                    None => {}
                }
                if !attached {
                    pane.unread += 1;
                }

                // The pane retires off the rail shortly; fold its report back
                // into the `spawn_subagent` card in the main chat so the run is
                // still there to read afterwards. A foreground run's card
                // already carries the report (it is the tool's own result), so
                // only a detached one needs writing back.
                let (name, task, bg) = (pane.name.clone(), pane.task.clone(), pane.bg);
                if bg.is_some() {
                    let report = match &error {
                        Some(error) => format!("failed: {error}"),
                        None if !completed => {
                            format!(
                                "hit its step budget after {} step(s).\n\n{output}",
                                pane.steps
                            )
                        }
                        None => output,
                    };
                    self.record_subagent_report(&name, &task, &report, !completed);
                }
            }
            AgentEvent::TodoUpdated(items) => {
                self.todos = items;
                // Auto-show the panel the first time the agent starts a
                // list; afterwards /todos controls visibility.
                if !self.todos_seen && !self.todos.is_empty() {
                    self.todos_seen = true;
                    self.show_todos = true;
                }
            }
            AgentEvent::CommandRequested(line) => {
                // A turn in flight can't be reconfigured, so queue the command;
                // the main loop drains it once the agent is back in its slot.
                self.notice(format!("agent requested {line} (runs after this turn)"));
                self.pending_agent_commands.push(line);
            }
            AgentEvent::Done { reason } => {
                self.flush_streaming();
                self.status.busy = false;
                self.turn_started = None;
                match reason {
                    DoneReason::Completed => {}
                    DoneReason::MaxSteps => self.notice(format!(
                        "step budget reached ({}) — send another message to continue",
                        self.status.max_steps
                    )),
                    DoneReason::TimeLimit => self.notice("time limit reached"),
                    DoneReason::Stopped => self.notice("turn stopped"),
                    DoneReason::CircuitBreaker => {
                        self.notice("circuit breaker tripped: repeated identical failures");
                    }
                }
            }
        }
    }
}

/// Side effects the main loop performs on behalf of [`App`] (the app itself
/// stays synchronous and side-effect free).
#[derive(Debug)]
pub enum AppAction {
    /// Start an agent turn with this user message (text + optional image paths).
    Submit(crate::commands::Preprocessed),
    /// Execute a parsed slash command.
    Command(SlashCommand),
    /// Interrupt the running turn (Ctrl-C): abort the turn task and rebuild
    /// the agent from the last session.
    Interrupt,
    /// Copy the current mouse selection to the clipboard. Handled in the main
    /// loop because it owns the terminal (and thus the rendered cell buffer).
    CopySelection,
}

/// Parse a `data:image/<subtype>;base64,<payload>` URL. Returns `(mime, b64)`.
fn parse_data_image_url(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix("data:")?;
    let (meta, payload) = rest.split_once(',')?;
    if !meta.contains(";base64") {
        return None;
    }
    let mime = meta.split(';').next()?.trim();
    if !mime.starts_with("image/") {
        return None;
    }
    Some((mime, payload.trim()))
}

/// Write base64 image bytes under `~/.wizard/attachments/`.
fn save_pasted_image_bytes(mime: &str, b64: &str) -> Result<PathBuf, String> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|err| format!("invalid base64: {err}"))?;
    if bytes.len() > crate::llm::MAX_IMAGE_BYTES {
        return Err(format!(
            "image is {} bytes (max {} MB)",
            bytes.len(),
            crate::llm::MAX_IMAGE_BYTES / (1024 * 1024)
        ));
    }
    let ext = match mime {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "bin",
    };
    let dir = crate::config::Config::wizard_dir()
        .map_err(|err| err.to_string())?
        .join("attachments");
    std::fs::create_dir_all(&dir).map_err(|err| format!("mkdir {}: {err}", dir.display()))?;
    let name = format!(
        "paste-{}-{}.{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
        &uuid::Uuid::new_v4().to_string()[..8],
        ext
    );
    let path = dir.join(name);
    std::fs::write(&path, bytes).map_err(|err| format!("write {}: {err}", path.display()))?;
    Ok(path)
}

/// Whether a paste token looks like an image path (extension only — existence
/// is checked separately).
fn looks_like_image_path_token(token: &str) -> bool {
    let cleaned = token
        .trim()
        .trim_matches(|c| c == '"' || c == '\'')
        .strip_prefix("file://")
        .unwrap_or(token.trim().trim_matches(|c| c == '"' || c == '\''));
    crate::commands::is_image_path(Path::new(cleaned))
}

/// Resolve a pasted path token to an existing image file.
fn resolve_pasted_image_path(token: &str, project_root: &Path) -> Option<PathBuf> {
    let cleaned = token.trim().trim_matches(|c| c == '"' || c == '\'');
    let cleaned = cleaned.strip_prefix("file://").unwrap_or(cleaned);
    let expanded = shellexpand::tilde(cleaned);
    let candidate = Path::new(expanded.as_ref());
    let path = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        project_root.join(candidate)
    };
    if path.is_file() && crate::commands::is_image_path(&path) {
        Some(path.canonicalize().unwrap_or(path))
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Terminal lifecycle
// ---------------------------------------------------------------------------

type Tui = Terminal<CrosstermBackend<std::io::Stdout>>;

fn setup_terminal() -> Result<Tui> {
    crossterm::terminal::enable_raw_mode().context("enabling raw mode")?;
    let mut stdout = std::io::stdout();
    // Capture the mouse so the scroll wheel scrolls the transcript (see the
    // ScrollUp/ScrollDown handler in `handle_event`). Without capture, the
    // terminal translates the wheel into ↑/↓ arrow keys in the alternate
    // screen, which the composer reads as input-history recall — so spinning
    // the wheel cycled previous messages instead of scrolling the text.
    // Tradeoff: capture pre-empts the terminal's native click-drag-to-select,
    // so wizard draws its own selection instead — drag to highlight, and the
    // covered text is copied to the clipboard (OSC 52) on release (see the
    // Down/Drag/Up handlers in `handle_event` and the highlight overlay in
    // `crate::ui`). Holding Shift still forces the terminal's own selection as
    // a fallback. Bracketed paste stays on so pasted text lands in the composer
    // as one chunk.
    crossterm::execute!(
        stdout,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableBracketedPaste,
        crossterm::event::EnableMouseCapture,
    )
    .context("entering alternate screen")?;
    // Kitty keyboard protocol (best-effort): with disambiguation on, terminals
    // report Shift+Enter as Enter+SHIFT instead of a bare Enter, which lets the
    // composer bind it to a newline. Terminals that don't support it are left
    // untouched (Alt+Enter is the fallback there). Popped in `restore_terminal`.
    if crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false) {
        let _ = crossterm::execute!(
            stdout,
            crossterm::event::PushKeyboardEnhancementFlags(
                crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES,
            ),
        );
    }
    Terminal::new(CrosstermBackend::new(stdout)).context("creating terminal")
}

/// Resolve the external editor: `$VISUAL`, then `$EDITOR`, then `nvim` when
/// it's on PATH. `None` means nothing usable is configured.
fn resolve_editor() -> Option<String> {
    for var in ["VISUAL", "EDITOR"] {
        if let Ok(editor) = std::env::var(var)
            && !editor.trim().is_empty()
        {
            return Some(editor);
        }
    }
    let nvim_on_path = std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join("nvim").is_file()));
    nvim_on_path.then(|| "nvim".to_string())
}

/// Suspend the TUI, run `editor` on `path`, then restore the TUI. Returns the
/// editor's exit status, or `None` when the TUI could not be suspended or
/// restored (a notice is posted either way; an unrestored terminal is fatal
/// to the session, so the caller must not continue).
fn run_editor_suspended(
    app: &mut App,
    terminal: &mut Tui,
    editor: &str,
    path: &std::path::Path,
) -> Option<std::io::Result<std::process::ExitStatus>> {
    // Leave the alternate screen so the editor draws on the real terminal.
    if let Err(err) = restore_terminal() {
        app.notice(format!("could not suspend the TUI: {err:#}"));
        return None;
    }
    // `sh -c` so editors with flags ("code --wait", "emacsclient -t") work.
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"{}\"", path.display()))
        .status();

    // Re-enter the TUI regardless of how the editor exited.
    match setup_terminal() {
        Ok(new_terminal) => {
            *terminal = new_terminal;
            let _ = terminal.clear();
            Some(status)
        }
        Err(err) => {
            app.notice(format!(
                "could not restore the TUI: {err:#} — /quit and relaunch"
            ));
            None
        }
    }
}

/// Suspend the TUI, open the external editor on `~/.wizard/config.toml`, then
/// restore the TUI and reload the edited config. Driven by the `/settings`
/// "Open config file" row; runs from the main loop because it owns `terminal`.
/// Falls back to a path notice when no editor is configured.
fn edit_config_file(app: &mut App, terminal: &mut Tui) {
    let path = match Config::path() {
        Ok(path) => path,
        Err(err) => {
            app.notice(format!("could not locate config: {err:#}"));
            return;
        }
    };
    let Some(editor) = resolve_editor() else {
        app.notice(format!(
            "no $EDITOR set — edit {} by hand, then /reload",
            path.display()
        ));
        return;
    };

    let Some(status) = run_editor_suspended(app, terminal, &editor, &path) else {
        return;
    };
    match status {
        Ok(status) if status.success() => match Config::load() {
            Ok(config) => {
                app.config = config;
                app.mode = app.config.mode;
                app.status.mode = app.config.mode;
                app.status.model = app.config.active().model;
                app.notice("config reloaded — restart for provider/model changes to take effect");
            }
            Err(err) => app.notice(format!("config not reloaded (parse error): {err:#}")),
        },
        Ok(_) => app.notice("editor exited without success — config not reloaded"),
        Err(err) => app.notice(format!("could not launch editor: {err:#}")),
    }
}

/// Suspend the TUI and open the composer draft in the external editor
/// (Ctrl-G); on a clean exit the edited file replaces the input with the
/// cursor at the end. A nonzero exit leaves the composer untouched. Runs from
/// the main loop because it owns `terminal`.
fn edit_prompt_in_editor(app: &mut App, terminal: &mut Tui) {
    let Some(editor) = resolve_editor() else {
        app.notice("no $VISUAL/$EDITOR set and nvim not on PATH — cannot edit the prompt");
        return;
    };

    let path = std::env::temp_dir().join(format!("wizard-prompt-{}.md", std::process::id()));
    if let Err(err) = std::fs::write(&path, &app.input) {
        app.notice(format!("could not stage the prompt: {err:#}"));
        return;
    }

    let Some(status) = run_editor_suspended(app, terminal, &editor, &path) else {
        return;
    };
    match status {
        Ok(status) if status.success() => match std::fs::read_to_string(&path) {
            Ok(text) => app.set_input_from_editor(text),
            Err(err) => app.notice(format!("could not read the edited prompt: {err:#}")),
        },
        Ok(_) => app.notice("editor exited without success — prompt unchanged"),
        Err(err) => app.notice(format!("could not launch editor: {err:#}")),
    }
    let _ = std::fs::remove_file(&path);
}

/// Copy `text` to the system clipboard with the OSC 52 terminal escape. This
/// needs no clipboard daemon and works over SSH, as long as the terminal
/// supports OSC 52 (most modern ones do). The sequence is written straight to
/// stdout — it's non-printing, so it doesn't disturb the rendered frame.
fn copy_to_clipboard(text: &str) -> Result<()> {
    use base64::Engine;
    use std::io::Write;

    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let mut stdout = std::io::stdout();
    // OSC 52: ESC ] 52 ; c ; <base64> BEL  — `c` targets the clipboard.
    write!(stdout, "\x1b]52;c;{encoded}\x07").context("writing clipboard escape")?;
    stdout.flush().context("flushing clipboard escape")?;
    Ok(())
}

fn restore_terminal() -> Result<()> {
    // Pop the keyboard-enhancement flags pushed in `setup_terminal`. Done
    // unconditionally (and ignoring errors): popping an empty/absent stack is a
    // no-op on supporting terminals and an ignored escape elsewhere, which is
    // safer than re-querying support from a panic/teardown path.
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::event::PopKeyboardEnhancementFlags,
    );
    crossterm::execute!(
        std::io::stdout(),
        crossterm::event::DisableMouseCapture,
        crossterm::event::DisableBracketedPaste,
        crossterm::terminal::LeaveAlternateScreen,
    )
    .context("leaving alternate screen")?;
    crossterm::terminal::disable_raw_mode().context("disabling raw mode")?;
    Ok(())
}

/// Restore the terminal if (and only if) raw mode is active. Safe to call
/// from a panic hook or after a headless run — it does nothing when the TUI
/// never started.
pub fn restore_terminal_best_effort() {
    if crossterm::terminal::is_raw_mode_enabled().unwrap_or(false) {
        let _ = restore_terminal();
    }
}

/// Restores the terminal when the main loop unwinds or errors out.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal_best_effort();
    }
}

// ---------------------------------------------------------------------------
// Agent wiring helpers
// ---------------------------------------------------------------------------

/// Load skills from the canonical roots (repo checkout, bundled beside the
/// binary, `~/.wizard/skills/`; later roots shadow earlier ones).
fn load_skill_roots() -> Vec<Skill> {
    let roots = crate::skills::default_roots();
    match crate::skills::load_skills(&roots) {
        Ok(skills) => skills,
        Err(err) => {
            tracing::warn!("loading skills: {err:#}");
            Vec::new()
        }
    }
}

/// Native + scripted + MCP tools, freshly composed, with the subagent
/// spawner layered on top. The spawn tool captures the base registry
/// (without itself) so subagents cannot recurse, plus the lifecycle `hooks`
/// so subagent tool calls fire the same hooks as the parent's.
/// Returns the registry and the spawn tool's shared model slot, which the
/// caller must hand to `Agent::bind_subagent_model` — otherwise subagents read
/// the *configured* model and quietly ignore `/model`. Every registry rebuild
/// mints a fresh spawn tool, so every rebuild has to rebind.
async fn build_registry(
    manager: &McpManager,
    client: &Arc<dyn LlmProvider>,
    hooks: &Arc<HookEngine>,
) -> Result<(ToolRegistry, subagent::SharedActiveModel)> {
    let mut base = ToolRegistry::with_native_tools();
    match Config::scripted_tools_dir() {
        Ok(dir) => {
            if let Err(err) = base.load_scripted(&dir) {
                tracing::warn!("loading scripted tools: {err:#}");
            }
        }
        Err(err) => tracing::warn!("resolving ~/.wizard/tools: {err:#}"),
    }
    if let Err(err) = base.attach_mcp(manager).await {
        tracing::warn!("attaching MCP tools: {err:#}");
    }
    base.apply_harness_overrides();

    let subagents_dir = Config::subagents_dir()?;
    let subagent_configs = subagent::available_configs(&subagents_dir);
    let base = Arc::new(base);
    let mut registry = subagent::scoped_registry(&base, None);
    let spawn_tool = Arc::new(subagent::SpawnSubagentTool::new(
        subagent_configs,
        Arc::clone(client),
        Arc::clone(&base),
        Arc::clone(hooks),
    ));
    let subagent_model = spawn_tool.model_handle();
    registry.register(spawn_tool);
    Ok((registry, subagent_model))
}

/// Attach the config-dependent tools (evolve, publish) to a registry built
/// by [`build_registry`]. Called by [`build_agent`] after the base registry
/// is assembled.
fn attach_config_tools(registry: &mut crate::tools::registry::ToolRegistry, config: &Config) {
    registry.register(Arc::new(crate::tools::evolve::EvolveTool::new(
        config.clone(),
    )));
    registry.register(Arc::new(crate::tools::publish::PublishTool::new(
        config.clone(),
    )));
}

/// Which session a freshly built [`Agent`] attaches to.
#[derive(Debug, Clone)]
enum SessionTarget {
    /// Start a brand-new session file.
    Fresh,
    /// Reopen the most recent session (`--resume`).
    Latest,
    /// Reopen a specific session by id (`/resume`, and crash/interrupt
    /// recovery of the active session so it survives a prior `/resume`).
    Id(String),
}

async fn build_agent(
    client: &Arc<dyn LlmProvider>,
    config: &Config,
    skills: &[Skill],
    project_root: &Path,
    manager: &McpManager,
    resume: SessionTarget,
) -> Result<Agent> {
    // Session first: the hook engine carries its id in every payload.
    let sessions_dir = Config::sessions_dir()?;
    let open_latest_or_fresh = || match Session::open_latest(&sessions_dir)? {
        Some(session) => Ok(session),
        None => Session::create(&sessions_dir),
    };
    let session = match resume {
        SessionTarget::Fresh => Session::create(&sessions_dir)?,
        SessionTarget::Latest => open_latest_or_fresh()?,
        SessionTarget::Id(id) => match Session::open_by_id(&sessions_dir, &id)? {
            Some(session) => session,
            // The id vanished (deleted, or empty after a fallback) — degrade
            // to the latest session rather than silently starting blank.
            None => open_latest_or_fresh()?,
        },
    };
    let hooks = Arc::new(HookEngine::new(
        crate::hooks::load(project_root),
        project_root.to_path_buf(),
        session.id.clone(),
    ));
    let (mut registry, subagent_model) = build_registry(manager, client, &hooks).await?;
    attach_config_tools(&mut registry, config);
    let model = config.active().model;
    let native_tools = match client.supports_native_tools(&model).await {
        Ok(supported) => supported,
        Err(err) => {
            tracing::warn!("probing tool support for {model}: {err:#}");
            false
        }
    };
    let mut agent = Agent::new(
        Arc::clone(client),
        registry,
        config.clone(),
        skills.to_vec(),
        project_root.to_path_buf(),
        session,
        native_tools,
        hooks,
    )?;
    // The TUI is the one surface that drains queued slash commands, so its
    // agent is the only one where `run_command` does anything.
    agent.set_command_dispatch(true);
    agent.bind_subagent_model(subagent_model);
    Ok(agent)
}

/// Run `git <args>` in `root` and return stdout.
async fn git_output(root: &Path, args: &[&str]) -> Result<String> {
    let output = tokio::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .await
        .context("running git")?;
    if !output.status.success() {
        anyhow::bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Compose the `/diff` sidebar contents: unstaged, then staged, then
/// untracked changes. Untracked (new) files are invisible to plain `git
/// diff`, so without the third section a tree whose only changes are new
/// files reads as "clean" — the diff sidebar looks broken.
async fn git_diff_text(root: &Path) -> Result<String> {
    let unstaged = git_output(root, &["diff"]).await?;
    let staged = git_output(root, &["diff", "--staged"]).await?;
    let untracked = git_output(root, &["ls-files", "--others", "--exclude-standard"]).await?;
    let mut text = String::new();
    if !unstaged.trim().is_empty() {
        text.push_str(&unstaged);
    }
    if !staged.trim().is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str("# --- staged ---\n");
        text.push_str(&staged);
    }
    let mut untracked_text = String::new();
    for file in untracked.lines().filter(|l| !l.trim().is_empty()) {
        // Skip Wizard's own session state (.wizard/checkpoints, snapshots,
        // etc.) — it's an implementation detail, not the user's work, and
        // dumping it here makes the diff sidebar look broken.
        if is_wizard_state_path(file) {
            continue;
        }
        untracked_text.push_str(&git_diff_untracked(root, file).await);
    }
    if !untracked_text.trim().is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str("# --- untracked ---\n");
        text.push_str(&untracked_text);
    }
    if text.is_empty() {
        text = "(working tree clean)".to_string();
    }
    Ok(text)
}

/// Is this repo-relative path inside Wizard's own state dir (`.wizard/`)?
/// Such files (checkpoints, snapshots) are Wizard internals, not the user's
/// changes, so `/diff` omits them. Matches the dir at the repo root or in
/// any subdir, tolerating either path separator.
fn is_wizard_state_path(path: &str) -> bool {
    let path = path.replace('\\', "/");
    path == ".wizard" || path.starts_with(".wizard/") || path.contains("/.wizard/")
}

/// Render a single untracked file as a full addition by diffing it against
/// `/dev/null`. `git diff --no-index` exits 1 when the inputs differ (the
/// normal case here) and reads nothing from the index, so it stays
/// read-only; we take its stdout regardless of exit status and drop the
/// file silently if git can't read it.
async fn git_diff_untracked(root: &Path, file: &str) -> String {
    match tokio::process::Command::new("git")
        .args(["diff", "--no-index", "--no-color", "--", "/dev/null", file])
        .current_dir(root)
        .output()
        .await
    {
        Ok(output) => String::from_utf8_lossy(&output.stdout).into_owned(),
        Err(_) => String::new(),
    }
}

fn describe_evolve_outcome(outcome: &EvolveOutcome) -> String {
    match outcome {
        EvolveOutcome::SkillAdded { name, path } => {
            format!(
                "evolve: added skill '{name}' at {} — run /reload to activate",
                path.display()
            )
        }
        EvolveOutcome::McpServerRegistered { name } => {
            format!("evolve: registered MCP server '{name}' — run /reload to activate")
        }
        EvolveOutcome::ScriptedToolAdded { name, path } => {
            format!(
                "evolve: added scripted tool '{name}' at {} — run /reload to activate",
                path.display()
            )
        }
        EvolveOutcome::SubagentAdded { name } => {
            format!("evolve: added subagent '{name}' — run /reload to activate")
        }
        EvolveOutcome::DeepRebuilt { binary } => {
            format!(
                "evolve: deep rebuild succeeded ({}) — restart wizard to run the new binary",
                binary.display()
            )
        }
        EvolveOutcome::FellBackToRuntime { reason, outcome } => {
            format!(
                "evolve: fell back to runtime tier ({reason}); {}",
                describe_evolve_outcome(outcome)
            )
        }
        EvolveOutcome::Denied => "evolve: change denied".to_string(),
    }
}

const HELP_TEXT: &str = "available commands:\n  \
/help                       show this help\n  \
/clear                      clear the conversation\n  \
/model [tag]                pick a model interactively, or switch directly\n  \
/mode [genie|sovereign]     pick or switch personality mode\n  \
/genie · /sovereign         switch mode directly\n  \
/effort [low|med|high]      set reasoning effort (Grok 4.x, OpenAI o-series/gpt-5)\n  \
/plan                       toggle plan mode (read-only until a plan is approved)\n  \
/omakase                    toggle omakase: chef's-choice plan mode, the agent decides\n  \
/rewind [turn]              rewind files and conversation to before a turn\n  \
/resume                     reopen and continue a past session\n  \
/compact                    summarize older history into a progress note now\n  \
/agents                     browse subagents and delegate to one\n  \
/subagents                  monitor the subagents running in this session\n  \
/evolve [--deep] <desc>     self-extension (skill / MCP / scripted tool)\n  \
/publish [branch]           fork Wizard to your GitHub, get a one-line installer\n  \
/provider                   add or switch LLM providers (interactive picker)\n  \
/fusion [config]            toggle model fusion (panel debate → synthesis), or configure the panel\n  \
/server [status|start|stop] manage the local llama-server\n  \
/login xai                  sign in with your xAI account (OAuth, no API key)\n  \
/reload                     reload skills, scripted tools, and MCP servers\n  \
/diff                       toggle the git diff sidebar\n  \
/todos                      toggle the todo side panel\n  \
/dashboard                  session manager: all live wizard sessions on this machine\n  \
/cost                       show session token usage and cost\n  \
/memory                     show saved project memories\n  \
/status                     show session status (model, usage, todos, tasks)\n  \
/bashes                     list background tasks (id, status, command)\n  \
/goal [text]                show or set the standing mission goal\n  \
/settings                   open the settings menu (change config anytime)\n  \
/vim                        toggle vim-style modal editing of the input line\n  \
/doctor                     diagnose config, providers, MCP, hooks, state dirs\n  \
/quit                       exit\n\
keys:\n  \
Tab / →                     accept command completion\n  \
Shift+Tab                   toggle plan mode\n  \
↑ / ↓                       select suggestion · browse input history\n  \
PgUp/PgDn · wheel           scroll the transcript (stays put while streaming)\n  \
Esc · Ctrl-End              jump back to the live tail\n  \
drag                        select text — copied to the clipboard on release\n  \
click a tool card           expand / collapse its output\n  \
Ctrl-P                      model picker  ·  Ctrl-T toggle last tool card\n  \
Ctrl-A/E Home/End ←/→       move cursor   ·  Ctrl-W/U/K kill word/to start/to end\n  \
Ctrl-G                      edit the prompt in $EDITOR\n  \
Ctrl-C                      quit";

// ---------------------------------------------------------------------------
// Genie-mode entry point
// ---------------------------------------------------------------------------

/// True for backends that run on this machine (no API key, no cloud).
fn is_local_kind(kind: ProviderKind) -> bool {
    matches!(kind, ProviderKind::LlamaCpp | ProviderKind::Ollama)
}

/// Build `provider`'s client and prove it usable: for local llama.cpp this
/// spawns `llama-server` when possible (the terminal is still in normal mode
/// at startup, so spawn/load progress shows on a plain-terminal spinner),
/// then runs the provider's health probe.
async fn try_provider(provider: &ProviderConfig) -> Result<Arc<dyn LlmProvider>> {
    let client = provider
        .build()
        .with_context(|| format!("building provider '{}'", provider.name))?;
    if provider.kind == ProviderKind::LlamaCpp {
        let wait = crate::progress::ServerSpinner::start();
        let outcome = server::ensure_running(provider, &wait).await;
        wait.finish(outcome.is_ok());
        outcome?;
    }
    client
        .health()
        .await
        .with_context(|| format!("LLM health check failed for {}", client.label()))?;
    Ok(client)
}

/// Cloud providers synthesized from standard API-key env vars when the local
/// backend is unavailable and nothing usable is configured:
/// `(key env var, kind, base URL, model, provider name)`.
const BYOP_ENV_FALLBACKS: &[(&str, ProviderKind, &str, &str, &str)] = &[
    (
        "ANTHROPIC_API_KEY",
        ProviderKind::Anthropic,
        "https://api.anthropic.com",
        "claude-fable-5",
        "anthropic",
    ),
    (
        "OPENAI_API_KEY",
        ProviderKind::Openai,
        "https://api.openai.com/v1",
        "gpt-4o",
        "openai",
    ),
    (
        "XAI_API_KEY",
        ProviderKind::Xai,
        "https://api.x.ai/v1",
        "grok-4.5",
        "xai",
    ),
    (
        "OPENROUTER_API_KEY",
        ProviderKind::OpenRouter,
        "https://openrouter.ai/api/v1",
        "openrouter/auto",
        "openrouter",
    ),
];

/// Resolve a working LLM client at startup. The active provider is tried
/// first. A failing *local* backend (llama.cpp not installed, server not
/// running, no model file, …) is not fatal: Wizard falls back to
/// bring-your-own-provider — any configured cloud provider, then one
/// synthesized from a standard API-key env var, then (interactively) the
/// onboarding wizard. The chosen fallback becomes the active provider in the
/// in-memory config so the session's picker and status bar reflect it; only
/// onboarding persists anything to disk.
async fn startup_client(config: &mut Config) -> Result<Arc<dyn LlmProvider>> {
    let active = config.active();
    // Cloud provider: build the client (cheap — it just reads cached
    // credentials) and return immediately. The health probe is a network
    // round-trip that would block the first paint, so `run_tui` runs it in the
    // background and surfaces a failure as a notice. A *build* error is a config
    // error (e.g. malformed base URL), so it stays fatal. The local fallback
    // chain below only matters when a local backend is the active provider.
    if !is_local_kind(active.kind) {
        return active
            .build()
            .with_context(|| format!("building provider '{}'", active.name));
    }
    let local_err = match try_provider(&active).await {
        Ok(client) => return Ok(client),
        Err(err) => err,
    };
    println!("local model unavailable: {local_err:#}");

    // Any other configured cloud provider.
    for provider in config.providers.clone() {
        if is_local_kind(provider.kind) || provider.name == active.name {
            continue;
        }
        match try_provider(&provider).await {
            Ok(client) => {
                println!(
                    "falling back to provider '{}' ({})",
                    provider.name, provider.model
                );
                config.active_provider = Some(provider.name);
                return Ok(client);
            }
            Err(err) => println!("provider '{}' is also unavailable: {err:#}", provider.name),
        }
    }

    // A provider synthesized from a standard API-key env var.
    for &(key_env, kind, base_url, model, name) in BYOP_ENV_FALLBACKS {
        if !std::env::var(key_env).is_ok_and(|v| !v.trim().is_empty()) {
            continue;
        }
        let provider = ProviderConfig {
            name: name.to_string(),
            kind,
            base_url: base_url.to_string(),
            model: model.to_string(),
            api_key_env: Some(key_env.to_string()),
            gguf_path: None,
            usd_per_mtok_in: None,
            usd_per_mtok_out: None,
        };
        match try_provider(&provider).await {
            Ok(client) => {
                println!("falling back to {model} via ${key_env}");
                // Replace any same-named (failed) entry so active() resolves
                // to this one.
                config.providers.retain(|p| p.name != provider.name);
                config.active_provider = Some(provider.name.clone());
                config.providers.push(provider);
                return Ok(client);
            }
            Err(err) => println!("{name} via ${key_env} is also unavailable: {err:#}"),
        }
    }

    // Nothing usable: let the user bring their own provider interactively.
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        println!("no working provider — opening setup so you can pick one (Esc to cancel).");
        if let Some(new_config) = crate::onboarding::run().await? {
            let active = new_config.active();
            let client = try_provider(&active).await?;
            *config = new_config;
            return Ok(client);
        }
    }

    Err(local_err.context(
        "the local model is unavailable and no fallback provider is configured — \
         run `wizard --onboard` to set one up",
    ))
}

/// Genie-mode entry point: set up the terminal (raw mode + alternate
/// screen), build the agent stack (LLM provider, registry with scripted +
/// MCP tools, skills, session), pre-fill `cli.prompt` if given, and drive
/// the [`EventLoop`](crate::event::EventLoop) until quit. Restores the
/// terminal on exit and on panic. Returns the process exit code: 0 from the
/// TUI itself; the headless fallback propagates its outcome code.
pub async fn run_tui(mut config: Config, cli: Cli) -> Result<i32> {
    // No usable terminal: run headless when a task was given, otherwise we
    // cannot do anything sensible.
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        if cli.prompt.is_some() {
            return crate::agent::run_headless(config, cli).await;
        }
        anyhow::bail!("wizard needs a terminal for the TUI; pass -p \"task\" to run headless");
    }

    let mut client = startup_client(&mut config).await?;
    // A cloud provider's health probe was skipped at startup (it would block the
    // first paint); run_tui runs it in the background below. Local providers
    // already proved themselves in startup_client (and loaded the model).
    let active_is_cloud = !is_local_kind(config.active().kind);

    let project_root = std::env::current_dir().context("resolving project root")?;
    let mut skills = load_skill_roots();

    let mcp_path = Config::mcp_config_path()?;
    // Start with no MCP servers connected. Connecting them means spawning stdio
    // servers and running the `initialize` handshake (e.g. `npx @playwright/mcp`,
    // ~2s) — far too slow to block the first paint. The connect runs on a
    // background task once the TUI is up (see below); its tools merge into the
    // registry via `Event::McpConnected`. Built-in tools work immediately.
    // Shared with background rebuild tasks (model switch, crash recovery).
    let manager = Arc::new(Mutex::new(McpManager::empty()));

    let mut agent_slot: Option<Agent> = Some(
        build_agent(
            &client,
            &config,
            &skills,
            &project_root,
            &*manager.lock().await,
            if cli.resume {
                SessionTarget::Latest
            } else {
                SessionTarget::Fresh
            },
        )
        .await?,
    );
    // `--plan` / `plan_first = true`: the session starts in plan mode (the
    // App mirror is set from the same config in App::new below).
    if config.plan_first
        && let Some(agent) = agent_slot.as_mut()
    {
        agent.set_plan_mode(true);
    }
    let mut agent_task: Option<JoinHandle<Agent>> = None;

    // Genie-mode max_steps as configured, used when switching back from
    // sovereign in-session.
    let genie_max_steps = config.max_steps;

    // Identity for this session's dashboard heartbeat.
    let session_id = agent_slot
        .as_ref()
        .map(|agent| agent.session().id.clone())
        .unwrap_or_default();
    let session_name = cli
        .prompt
        .as_deref()
        .and_then(|prompt| prompt.lines().next())
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .map(|line| line.chars().take(48).collect::<String>())
        .unwrap_or_else(|| {
            // No prompt: name the session after its working directory.
            project_root
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "session".to_string())
        });

    let mut app = App::new(config);
    app.project_root = project_root.clone();
    app.custom_commands = crate::commands::load(&project_root);
    app.session_id = session_id.clone();
    app.session_name = session_name;
    // The rail kills background subagents through this, so it must be reachable
    // while a turn holds the agent — hence a cloned Arc, not the agent itself.
    app.subagents = agent_slot.as_ref().map(|agent| agent.subagent_registry());
    app.session_started_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Register this session so other sessions' /dashboard can see it.
    crate::session_registry::write(&app.session_record());
    // `wizard agents` opens straight into the dashboard.
    if matches!(cli.command, Some(crate::cli::Command::Agents)) {
        app.show_dashboard = true;
        app.refresh_sessions();
        app.refresh_peek();
    }
    if let Some(prompt) = cli.prompt.clone() {
        app.set_input(prompt);
    }
    // No startup notice: the welcome screen already shows the model, mode,
    // and help pointers until the first message arrives.

    // session_start hooks fire before the first draw; their activity (and
    // any failures) lands in the transcript as notices.
    {
        let (hook_tx, mut hook_rx) = mpsc::channel::<AgentEvent>(256);
        if let Some(agent) = agent_slot.as_mut() {
            agent.fire_session_start(&hook_tx).await;
        }
        drop(hook_tx);
        while let Some(event) = hook_rx.recv().await {
            app.handle_agent_event(event);
        }
    }

    // Ask the terminal how it can draw an image, while stdio is still the plain
    // terminal: the query writes escape sequences and reads the reply, which the
    // alternate screen and our own raw mode would both get in the way of. A
    // terminal that says nothing gets half-blocks, which every terminal can draw.
    *app.images.borrow_mut() = ImageCache::detect();
    tracing::debug!("terminal images: {:?}", app.images.borrow());

    let mut events = EventLoop::new(Duration::from_millis(100));
    let mut terminal = setup_terminal()?;
    let _guard = TerminalGuard;

    // Probe the cloud provider's health off the draw path so the network
    // round-trip doesn't delay launch. Only a failure is surfaced; success is
    // silent. The failure goes to `Event::ProviderHealthFailed` (not a plain
    // notice) so the main loop can show it where it's visible pre-conversation
    // — otherwise the welcome screen hides it until the first message fails.
    if active_is_cloud {
        let probe = client.clone();
        let notify = events.sender();
        tokio::spawn(async move {
            if let Err(err) = probe.health().await {
                let _ = notify
                    .send(Event::ProviderHealthFailed(format!("{err:#}")))
                    .await;
            }
        });
    }

    // Connect MCP servers off the draw path so a slow stdio server (npx, etc.)
    // can't delay launch. When the connect finishes, the main loop rebuilds the
    // registry from the now-populated manager (`Event::McpConnected`). The
    // indicator goes up unconditionally and comes down on every exit path
    // (no-servers early return, success, failure) so a message sent before the
    // tools arrive isn't a silent surprise.
    app.mcp_connecting = true;
    {
        let manager = Arc::clone(&manager);
        let mcp_path = mcp_path.clone();
        let notify = events.sender();
        tokio::spawn(async move {
            let mcp_config = match McpConfig::load(&mcp_path) {
                Ok(config) => config,
                Err(err) => {
                    tracing::warn!("loading {}: {err:#}", mcp_path.display());
                    // Tell the loop to clear the indicator (no servers will
                    // connect): nothing configured, nothing missing.
                    let _ = notify
                        .send(Event::McpConnected {
                            connected: 0,
                            configured: 0,
                        })
                        .await;
                    return;
                }
            };
            if mcp_config.servers.is_empty() {
                // Nothing to connect: keep the empty manager and skip the
                // registry rebuild entirely (the agent already has every tool).
                let _ = notify
                    .send(Event::McpConnected {
                        connected: 0,
                        configured: 0,
                    })
                    .await;
                return;
            }
            let configured = mcp_config.servers.len();
            {
                let mut manager = manager.lock().await;
                if let Err(err) = manager.reload(&mcp_config).await {
                    tracing::warn!("connecting MCP servers: {err:#}");
                }
            }
            let connected = manager.lock().await.connection_count();
            let _ = notify
                .send(Event::McpConnected {
                    connected,
                    configured,
                })
                .await;
        });
    }

    let mut last_heartbeat = Instant::now();

    loop {
        terminal.draw(|frame| crate::ui::draw(frame, &app))?;

        // Refresh this session's heartbeat so other dashboards see it live.
        if last_heartbeat.elapsed() >= Duration::from_secs(3) {
            session_registry::write(&app.session_record());
            last_heartbeat = Instant::now();
        }

        let Some(event) = events.next().await else {
            break;
        };

        // A background rebuild finished: restore the agent into the slot.
        if let Event::AgentRebuilt(rebuild) = event {
            let rebuild = *rebuild;
            app.rebuilding = None;
            let was_compacting = app.compacting;
            app.compacting = false;
            if let Some(model) = rebuild.model {
                app.config.model = model.clone();
                app.status.model = model;
            }
            if let Some(mut agent) = rebuild.agent {
                // A rebuilt agent starts with plan mode off; restore the
                // session's setting.
                if app.plan_mode {
                    agent.set_plan_mode(true);
                }
                // A rebuild brings a fresh tool context, so the old registry
                // handle is dead — re-point the rail at the live one.
                app.subagents = Some(agent.subagent_registry());
                // After /compact the history shrank: refresh the context
                // meter to the post-compact estimate (last_prompt was cleared
                // so context_tokens() falls back to a char/4 estimate of the
                // remaining history) instead of leaving the pre-compact size.
                if was_compacting {
                    app.status.context_tokens = agent.context_tokens();
                }
                agent_slot = Some(agent);
            }
            app.notice(rebuild.notice);
            // A `/model` in the queue triggered this rebuild and deferred the
            // rest of the queued commands; drain them now the agent is back.
            drain_agent_commands(
                &mut app,
                &mut client,
                &mut agent_slot,
                &manager,
                &mut skills,
                &project_root,
                &mcp_path,
                genie_max_steps,
                &events,
            )
            .await;
            continue;
        }

        // The background MCP connect finished: merge the servers' tools into the
        // live agent's registry. If a turn is running the agent is out of its
        // slot, so defer the merge until the turn returns it.
        if let Event::McpConnected {
            connected,
            configured,
        } = event
        {
            app.mcp_connecting = false;
            // Some configured servers came up but not all: surface the shortfall
            // as an `error:`-prefixed notice (bold/white, counts as conversation)
            // — the actionable counterpart to the now-silent success path.
            if connected < configured {
                app.notice(format!(
                    "error: {} of {configured} MCP servers failed to connect (see logs)",
                    configured - connected
                ));
            }
            if connected > 0 {
                if agent_slot.is_some() {
                    CommandContext {
                        app: &mut app,
                        client: &mut client,
                        agent_slot: &mut agent_slot,
                        manager: &manager,
                        skills: &mut skills,
                        project_root: &project_root,
                        mcp_path: &mcp_path,
                        genie_max_steps,
                        events: &events,
                    }
                    .merge_mcp_registry()
                    .await;
                } else {
                    app.mcp_merge_pending = true;
                }
            }
            continue;
        }

        // The deferred cloud-provider health probe failed: store the error so
        // it shows at launch (home screen + status bar) rather than only on the
        // first message.
        if let Event::ProviderHealthFailed(err) = event {
            app.provider_health_error = Some(err);
            continue;
        }

        // A background sign-in succeeded: add and switch to the provider. Owned
        // here because it mutates config and the agent slot.
        if let Event::ProviderActivated(cfg) = event {
            CommandContext {
                app: &mut app,
                client: &mut client,
                agent_slot: &mut agent_slot,
                manager: &manager,
                skills: &mut skills,
                project_root: &project_root,
                mcp_path: &mcp_path,
                genie_max_steps,
                events: &events,
            }
            .add_provider_config(
                *cfg,
                "signed in to xAI — provider added and active".to_string(),
            )
            .await;
            continue;
        }

        let turn_done = matches!(&event, Event::Agent(AgentEvent::Done { .. }));

        let action = app.handle_event(event)?;
        if let Some(action) = action {
            match action {
                AppAction::Submit(prepared) => match agent_slot.take() {
                    Some(mut agent) => {
                        app.status.busy = true;
                        app.status.step = 0;
                        app.streaming.clear();
                        app.streaming_thinking.clear();
                        app.turn_started = Some(Instant::now());
                        app.roll_spinner_verb();

                        // Bridge AgentEvent -> Event::Agent for the UI loop.
                        let (agent_tx, mut agent_rx) = mpsc::channel::<AgentEvent>(256);
                        let forward = events.sender();
                        tokio::spawn(async move {
                            while let Some(agent_event) = agent_rx.recv().await {
                                if forward.send(Event::Agent(agent_event)).await.is_err() {
                                    break;
                                }
                            }
                        });

                        let prompt = prepared.text;
                        let images = prepared.images;
                        agent_task = Some(tokio::spawn(async move {
                            let fallback = agent_tx.clone();
                            if let Err(err) =
                                agent.run_turn_with_images(&prompt, images, agent_tx).await
                            {
                                // run_turn normally ends with Done itself;
                                // on a hard error make sure the UI unblocks.
                                let _ = fallback
                                    .send(AgentEvent::Error(format!("turn failed: {err:#}")))
                                    .await;
                                let _ = fallback
                                    .send(AgentEvent::Done {
                                        reason: DoneReason::Stopped,
                                    })
                                    .await;
                            }
                            agent
                        }));
                    }
                    None => app.notice("the agent is busy — wait for the current turn to finish"),
                },
                AppAction::Command(command) => {
                    CommandContext {
                        app: &mut app,
                        client: &mut client,
                        agent_slot: &mut agent_slot,
                        manager: &manager,
                        skills: &mut skills,
                        project_root: &project_root,
                        mcp_path: &mcp_path,
                        genie_max_steps,
                        events: &events,
                    }
                    .run(command)
                    .await;
                }
                AppAction::Interrupt => {
                    // Cancel the running turn by aborting its task; the agent
                    // moved into it is lost, so rebuild from the last session
                    // (same path as crash recovery).
                    if let Some(handle) = agent_task.take() {
                        handle.abort();
                        app.flush_streaming();
                        app.status.busy = false;
                        app.status.step = 0;
                        app.turn_started = None;
                        app.notice("interrupted");
                        app.rebuilding = Some("restarting agent".to_string());
                        let client = client.clone();
                        let config = app.config.clone();
                        let skills = skills.clone();
                        let project_root = project_root.clone();
                        let manager = Arc::clone(&manager);
                        let notify = events.sender();
                        let session = SessionTarget::Id(app.session_id.clone());
                        tokio::spawn(async move {
                            let manager = manager.lock().await;
                            let rebuild = match build_agent(
                                &client,
                                &config,
                                &skills,
                                &project_root,
                                &manager,
                                session,
                            )
                            .await
                            {
                                Ok(agent) => AgentRebuild {
                                    agent: Some(agent),
                                    model: None,
                                    notice: "ready".to_string(),
                                },
                                Err(err) => AgentRebuild {
                                    agent: None,
                                    model: None,
                                    notice: format!(
                                        "could not restart the agent: {err:#} — /quit and relaunch"
                                    ),
                                },
                            };
                            let _ = notify.send(Event::AgentRebuilt(Box::new(rebuild))).await;
                        });
                    }
                }
                AppAction::CopySelection => {
                    // The drag finished: re-render and read the cells under the
                    // selection from the fresh frame. (After a completed
                    // `Terminal::draw` the swapped-in current buffer is reset,
                    // so reading `current_buffer_mut` here would find only
                    // blanks — clearing the selection the moment the button is
                    // released.) The highlight stays on screen until the next
                    // keystroke / click / scroll.
                    if let Some(selection) = app.selection {
                        let mut text = String::new();
                        terminal.draw(|frame| {
                            crate::ui::draw(frame, &app);
                            text = crate::ui::selection_text(frame.buffer_mut(), &selection);
                        })?;
                        if text.is_empty() {
                            app.selection = None;
                        } else if let Err(err) = copy_to_clipboard(&text) {
                            app.notice(format!("could not copy selection: {err:#}"));
                        }
                        // Success is silent: the persistent highlight is the
                        // feedback, and an unchanged transcript keeps the
                        // highlight aligned with the selected rows.
                    }
                }
            }
        }

        // The `/settings` "Open config file" row asks the main loop (the
        // terminal owner) to suspend the TUI and run an external editor.
        if app.pending_edit_config {
            app.pending_edit_config = false;
            edit_config_file(&mut app, &mut terminal);
        }

        // Ctrl-G: same suspend/restore dance, on the composer draft.
        if app.pending_edit_prompt {
            app.pending_edit_prompt = false;
            edit_prompt_in_editor(&mut app, &mut terminal);
        }

        // `/compact`: take the agent and summarize history off the event loop
        // so the TUI keeps animating the progress bar. The agent returns via
        // Event::AgentRebuilt, the same path as crash recovery.
        if app.pending_compact {
            app.pending_compact = false;
            match agent_slot.take() {
                Some(mut agent) => {
                    app.compacting = true;
                    let notify = events.sender();
                    tokio::spawn(async move {
                        let notice = agent.compact_now().await.describe();
                        let rebuild = AgentRebuild {
                            agent: Some(agent),
                            model: None,
                            notice,
                        };
                        let _ = notify.send(Event::AgentRebuilt(Box::new(rebuild))).await;
                    });
                }
                None => app.notice("the agent is busy — try again in a moment"),
            }
        }

        if turn_done && let Some(handle) = agent_task.take() {
            match handle.await {
                Ok(agent) => {
                    agent_slot = Some(agent);
                    // The provider just served a turn, so any earlier health
                    // warning was transient — drop it so it self-heals.
                    app.provider_health_error = None;
                    // MCP finished connecting mid-turn: merge its tools now that
                    // the agent is back in its slot.
                    if app.mcp_merge_pending {
                        app.mcp_merge_pending = false;
                        CommandContext {
                            app: &mut app,
                            client: &mut client,
                            agent_slot: &mut agent_slot,
                            manager: &manager,
                            skills: &mut skills,
                            project_root: &project_root,
                            mcp_path: &mcp_path,
                            genie_max_steps,
                            events: &events,
                        }
                        .merge_mcp_registry()
                        .await;
                    }
                }
                Err(err) => {
                    // The turn task panicked and took the agent with it.
                    // Rebuild off the event loop so the TUI stays responsive.
                    app.notice(format!("agent task crashed: {err}"));
                    app.rebuilding = Some("restarting agent".to_string());
                    let client = client.clone();
                    let config = app.config.clone();
                    let skills = skills.clone();
                    let project_root = project_root.clone();
                    let manager = Arc::clone(&manager);
                    let notify = events.sender();
                    let session = SessionTarget::Id(app.session_id.clone());
                    tokio::spawn(async move {
                        let manager = manager.lock().await;
                        let rebuild = match build_agent(
                            &client,
                            &config,
                            &skills,
                            &project_root,
                            &manager,
                            session,
                        )
                        .await
                        {
                            Ok(agent) => AgentRebuild {
                                agent: Some(agent),
                                model: None,
                                notice: "agent restarted from the last session".to_string(),
                            },
                            Err(err) => AgentRebuild {
                                agent: None,
                                model: None,
                                notice: format!(
                                    "could not restart the agent: {err:#} — /quit and relaunch"
                                ),
                            },
                        };
                        let _ = notify.send(Event::AgentRebuilt(Box::new(rebuild))).await;
                    });
                }
            }
        }

        // Dispatch any slash commands the agent queued via `run_command` during
        // the turn, now that it's back in its slot and can be reconfigured. A
        // crashed turn leaves the slot empty (a rebuild is in flight); the queue
        // then waits for that rebuild, or the next completed turn.
        drain_agent_commands(
            &mut app,
            &mut client,
            &mut agent_slot,
            &manager,
            &mut skills,
            &project_root,
            &mcp_path,
            genie_max_steps,
            &events,
        )
        .await;

        if app.should_quit {
            break;
        }
    }

    // session_end hooks: best-effort — skipped when quitting mid-turn took
    // the agent — with no event surfacing (the TUI is going away).
    if let Some(agent) = agent_slot.as_ref() {
        agent.fire_session_end(None).await;
    }

    // Drop this session's heartbeat so it leaves the dashboard immediately.
    session_registry::remove(&app.session_id);

    drop(_guard);
    restore_terminal_best_effort();
    Ok(0)
}

/// Everything a slash command may touch, borrowed from the main loop for
/// the duration of one dispatch.
struct CommandContext<'a> {
    app: &'a mut App,
    client: &'a mut Arc<dyn LlmProvider>,
    agent_slot: &'a mut Option<Agent>,
    manager: &'a Arc<Mutex<McpManager>>,
    skills: &'a mut Vec<Skill>,
    project_root: &'a Path,
    mcp_path: &'a Path,
    genie_max_steps: StepBudget,
    events: &'a EventLoop,
}

/// Dispatch the slash commands the agent queued via `run_command`, in order,
/// now that the turn has ended and the agent is back in its slot. A command
/// that starts a background rebuild (e.g. `/model`) empties the slot; draining
/// stops there and leaves the rest queued, so the `AgentRebuilt` handler drains
/// them once the agent returns. Called both after a turn completes and after a
/// rebuild restores the slot, so no queued command is silently dropped.
#[allow(clippy::too_many_arguments)]
async fn drain_agent_commands(
    app: &mut App,
    client: &mut Arc<dyn LlmProvider>,
    agent_slot: &mut Option<Agent>,
    manager: &Arc<Mutex<McpManager>>,
    skills: &mut Vec<Skill>,
    project_root: &Path,
    mcp_path: &Path,
    genie_max_steps: StepBudget,
    events: &EventLoop,
) {
    while agent_slot.is_some() && !app.pending_agent_commands.is_empty() {
        let line = app.pending_agent_commands.remove(0);
        let Some(Ok(command)) = SlashCommand::parse(&line) else {
            continue;
        };
        CommandContext {
            app: &mut *app,
            client: &mut *client,
            agent_slot: &mut *agent_slot,
            manager,
            skills: &mut *skills,
            project_root,
            mcp_path,
            genie_max_steps,
            events,
        }
        .run(command)
        .await;
    }
}

impl CommandContext<'_> {
    /// Execute one slash command against the running stack.
    async fn run(mut self, command: SlashCommand) {
        match command {
            SlashCommand::Help => self.app.notice(HELP_TEXT),
            SlashCommand::Quit => self.app.should_quit = true,
            SlashCommand::Diff => self.toggle_diff().await,
            SlashCommand::Todos => self.toggle_todos(),
            SlashCommand::Dashboard => self.toggle_dashboard(),
            SlashCommand::Subagents => self.toggle_subagents(),
            SlashCommand::Cost => self.cost(),
            SlashCommand::Memory => self.memory(),
            SlashCommand::Doctor => self.doctor().await,
            SlashCommand::Status => self.status(),
            SlashCommand::Bashes => self.bashes(),
            SlashCommand::Goal(None) => self.show_goal(),
            SlashCommand::Goal(Some(text)) => self.set_goal(text),
            SlashCommand::Clear => self.clear(),
            SlashCommand::Model(None) => self.open_model_picker().await,
            SlashCommand::Model(Some(tag)) => self.switch_model(tag),
            SlashCommand::Mode(None) => self.open_mode_picker(),
            SlashCommand::Mode(Some(mode)) => self.switch_mode(mode),
            SlashCommand::Effort(None) => self.open_effort_picker(),
            SlashCommand::Effort(Some(effort)) => self.set_effort(effort),
            SlashCommand::Plan => self.toggle_plan(),
            SlashCommand::Omakase => self.toggle_omakase(),
            SlashCommand::Rewind(None) => self.open_rewind_picker(),
            SlashCommand::Rewind(Some(turn)) => self.rewind(turn),
            SlashCommand::Resume(None) => self.app.open_resume_picker(),
            SlashCommand::Resume(Some(id)) => self.resume_session(id).await,
            SlashCommand::Compact => self.request_compact(),
            SlashCommand::Agents => self.open_agents_picker(),
            SlashCommand::Reload => self.reload().await,
            SlashCommand::Evolve { deep, description } => self.evolve(deep, description),
            SlashCommand::Publish { branch } => self.publish(branch),
            SlashCommand::Fusion(FusionAction::Toggle) => self.toggle_fusion().await,
            SlashCommand::Fusion(FusionAction::Config) => self.open_fusion_picker(),
            SlashCommand::Provider(action) => self.provider(action).await,
            SlashCommand::ProviderSetup {
                name,
                kind,
                base_url,
                model,
                api_key,
            } => {
                self.provider_setup(name, kind, base_url, model, api_key)
                    .await
            }
            SlashCommand::Server(action) => self.server(action).await,
            SlashCommand::Login(provider) => self.login(provider),
            SlashCommand::Settings => self.app.open_settings_picker(),
            SlashCommand::Vim => self.app.toggle_vim(),
            SlashCommand::ImportClaude(selection) => self.import_claude(selection).await,
        }
    }

    /// True (with a notice) when the agent cannot be touched right now —
    /// a turn is running or a background rebuild is in flight.
    fn agent_unavailable(&mut self, action: &str) -> bool {
        if self.app.status.busy {
            self.app
                .notice(format!("cannot {action} while a turn is running"));
            true
        } else if self.app.rebuilding.is_some() {
            self.app
                .notice(format!("cannot {action} while the agent is rebuilding"));
            true
        } else {
            false
        }
    }

    async fn toggle_diff(&mut self) {
        self.app.show_diff = !self.app.show_diff;
        if self.app.show_diff {
            self.app.diff_scroll = 0;
            self.app.diff_text = match git_diff_text(self.project_root).await {
                Ok(text) => text,
                Err(err) => format!("could not read git diff: {err:#}"),
            };
        }
    }

    /// `/todos`: toggle the todo side panel.
    fn toggle_todos(&mut self) {
        self.app.show_todos = !self.app.show_todos;
        if self.app.show_todos && self.app.todos.is_empty() {
            self.app
                .notice("todo list is empty — the agent fills it via the `todo` tool");
        }
    }

    /// `/dashboard`: toggle the machine-wide session manager. On open, refresh
    /// the live-session list from the registry; the event loop keeps it current
    /// while it's up.
    fn toggle_dashboard(&mut self) {
        self.app.show_dashboard = !self.app.show_dashboard;
        if self.app.show_dashboard {
            self.app.refresh_sessions();
            self.app.refresh_peek();
        }
    }

    /// `/subagents`: jump to the rail. The rail is always on screen while
    /// subagents exist, so this is a shortcut for ↓ — it takes you straight
    /// to the first running one.
    fn toggle_subagents(&mut self) {
        if self.app.attached.is_some() {
            self.app.detach_pane();
            return;
        }
        if !self.app.focus_rail() {
            self.app
                .notice("no subagents yet — the agent spawns them with `spawn_subagent`");
        }
    }

    /// `/cost`: session token totals, plus an estimate when the active
    /// provider has `usd_per_mtok_in` / `usd_per_mtok_out` configured.
    fn cost(&mut self) {
        let prompt = self.app.status.prompt_tokens;
        let completion = self.app.status.completion_tokens;
        let mut text = format!("session usage: {prompt} prompt + {completion} completion tokens");
        let provider = self.app.config.active();
        match crate::usage::cost_usd(
            prompt,
            completion,
            provider.usd_per_mtok_in,
            provider.usd_per_mtok_out,
        ) {
            Some(cost) => text.push_str(&format!(" · est. ${cost:.4}")),
            None => text.push_str(&format!(
                "\nset usd_per_mtok_in / usd_per_mtok_out on provider '{}' in \
                 ~/.wizard/config.toml for cost estimates",
                provider.name
            )),
        }
        self.app.notice(text);
    }

    /// `/memory`: list the saved project memories (name — description).
    fn memory(&mut self) {
        let store = match MemoryStore::open(self.project_root) {
            Ok(store) => store,
            Err(err) => {
                self.app
                    .notice(format!("could not open memory store: {err:#}"));
                return;
            }
        };
        match store.list() {
            Ok(entries) if entries.is_empty() => self
                .app
                .notice(format!("no memories saved yet ({})", store.dir().display())),
            Ok(entries) => {
                let mut text = format!("saved memories ({}):\n", store.dir().display());
                for entry in &entries {
                    text.push_str(&format!("  {} — {}\n", entry.name, entry.description));
                }
                self.app.notice(text.trim_end().to_string());
            }
            Err(err) => self.app.notice(format!("could not list memories: {err:#}")),
        }
    }

    /// `/doctor`: the same diagnostics as `wizard doctor`, in the
    /// transcript. Network probes are capped at 5s each, but a slow
    /// provider or MCP server still blocks the UI for that long.
    async fn doctor(&mut self) {
        let checks = crate::doctor::run_checks(self.project_root).await;
        self.app
            .notice(format!("doctor:\n{}", crate::doctor::render(&checks)));
    }

    /// `/status`: one snapshot of the session — model, provider, mode,
    /// session id, usage, todo progress, background tasks, plan mode.
    fn status(&mut self) {
        let provider = self.app.config.active();
        let effort = self
            .app
            .config
            .reasoning_effort
            .map(|e| e.to_string())
            .unwrap_or_else(|| "default".to_string());
        let mut text = format!(
            "model: {}\nprovider: {} ({:?} @ {})\nmode: {}\neffort: {effort}",
            self.app.status.model, provider.name, provider.kind, provider.base_url, self.app.mode,
        );
        match self.agent_slot.as_ref() {
            Some(agent) => {
                let (prompt, completion) = agent.usage().session_totals();
                text.push_str(&format!(
                    "\nsession: {}\nusage: {prompt} prompt + {completion} completion tokens",
                    agent.session().id,
                ));
                text.push_str(&format!(
                    "\nbackground tasks: {} running",
                    agent.running_tasks()
                ));
            }
            None => {
                // Mid-turn (or rebuilding): the status bar mirror is the
                // best available source.
                let (prompt, completion) = (
                    self.app.status.prompt_tokens,
                    self.app.status.completion_tokens,
                );
                text.push_str(&format!(
                    "\nsession: (turn running)\nusage: {prompt} prompt + {completion} completion tokens",
                ));
            }
        }
        let (done, total) = crate::tools::todo::progress(&self.app.todos);
        if total > 0 {
            text.push_str(&format!("\ntodos: {done}/{total} done"));
        } else {
            text.push_str("\ntodos: none");
        }
        text.push_str(&format!(
            "\nplan mode: {}",
            if self.app.omakase {
                "on (omakase — chef's choice)"
            } else if self.app.plan_mode {
                "on"
            } else {
                "off"
            }
        ));
        self.app.notice(text);
    }

    /// `/bashes`: list every background task this session has spawned
    /// (`execute` with `run_in_background`), running and finished, newest
    /// last — id, status, and the command line.
    fn bashes(&mut self) {
        let Some(agent) = self.agent_slot.as_ref() else {
            self.app
                .notice("background tasks: unavailable while a turn is running");
            return;
        };
        let tasks = agent.tasks();
        if tasks.is_empty() {
            self.app.notice("background tasks: none");
            return;
        }
        let mut text = String::from("background tasks:\n");
        for task in &tasks {
            text.push_str(&format!(
                "  #{} [{}] {}\n",
                task.id,
                task.status.describe(),
                task.command
            ));
        }
        self.app.notice(text.trim_end().to_string());
    }

    /// `/goal`: show the standing mission goal that drives sovereign /
    /// continuous mode, with its cycle count and a few recent progress notes.
    fn show_goal(&mut self) {
        match crate::agent::mission::Mission::load(self.project_root) {
            Err(err) => self.app.notice(format!("could not read mission: {err:#}")),
            Ok(None) => self.app.notice(
                "no standing goal set — use `/goal <text>` to set one \
                 (drives sovereign/continuous mode)",
            ),
            Ok(Some(m)) => {
                let mut text = format!(
                    "goal: {}\ncycles: {}  ·  updated {}",
                    m.goal,
                    m.cycles,
                    m.updated.format("%Y-%m-%d %H:%M UTC"),
                );
                if !m.notes.is_empty() {
                    text.push_str("\nrecent:");
                    let skip = m.notes.len().saturating_sub(5);
                    for note in &m.notes[skip..] {
                        text.push_str(&format!("\n  - {note}"));
                    }
                }
                self.app.notice(text);
            }
        }
    }

    /// `/goal <text>`: set (or replace) the standing mission goal,
    /// non-destructively preserving cycles and existing progress notes.
    fn set_goal(&mut self, text: String) {
        let text = text.trim().to_string();
        if text.is_empty() {
            self.app.notice("usage: /goal <text>");
            return;
        }
        let m = match crate::agent::mission::Mission::load(self.project_root) {
            Err(err) => {
                self.app.notice(format!("could not read mission: {err:#}"));
                return;
            }
            Ok(Some(mut m)) => {
                m.goal = text.clone();
                m.note(format!("goal changed to: {text}"));
                m
            }
            Ok(None) => crate::agent::mission::Mission::new(text.clone()),
        };
        if let Err(err) = m.save(self.project_root) {
            self.app.notice(format!("could not save mission: {err:#}"));
            return;
        }
        self.app.notice(format!("standing goal set:\n{text}"));
    }

    fn clear(&mut self) {
        if self.agent_unavailable("clear") {
            return;
        }
        if let Some(agent) = self.agent_slot.as_mut()
            && let Err(err) = agent.clear()
        {
            self.app
                .notice(format!("failed to rotate session: {err:#}"));
            return;
        }
        self.app.transcript.clear();
        self.app.streaming.clear();
        self.app.streaming_thinking.clear();
        self.app.scroll_to_bottom();
        // Mirror the agent's counter reset so the status bar drops the old
        // conversation's totals immediately (not after the next Usage event).
        self.app.status.prompt_tokens = 0;
        self.app.status.completion_tokens = 0;
        self.app.status.context_tokens = self
            .agent_slot
            .as_ref()
            .map(|agent| agent.context_tokens())
            .unwrap_or(0);
        self.app.notice("conversation cleared");
    }

    /// Open the interactive model picker with all installed models.
    async fn open_model_picker(&mut self) {
        if self.agent_unavailable("switch models") {
            return;
        }
        match self.client.list_models().await {
            Ok(models) if !models.is_empty() => {
                let current = self.app.status.model.clone();
                let items: Vec<PickerItem> = models
                    .into_iter()
                    .map(|model| PickerItem {
                        current: model == current
                            || model.split(':').next() == Some(current.as_str()),
                        detail: String::new(),
                        value: model,
                    })
                    .collect();
                let selected = items.iter().position(|item| item.current).unwrap_or(0);
                self.app.picker = Some(Picker {
                    kind: PickerKind::Model,
                    title: " select model ".to_string(),
                    items,
                    selected,
                });
            }
            Ok(_) => self
                .app
                .notice("no models installed — try `ollama pull <model>`"),
            Err(err) => self.app.notice(format!("could not list models: {err:#}")),
        }
    }

    /// Switch models off the event loop: the validation probe and any agent
    /// rebuild run in a background task and come back as
    /// [`Event::AgentRebuilt`], so the TUI never freezes.
    fn switch_model(&mut self, tag: String) {
        if self.agent_unavailable("switch models") {
            return;
        }
        let agent = self.agent_slot.take();
        self.app.rebuilding = Some(format!("switching to {tag}"));
        let client = self.client.clone();
        let config = self.app.config.clone();
        let skills = self.skills.clone();
        let project_root = self.project_root.to_path_buf();
        let manager = Arc::clone(self.manager);
        let notify = self.events.sender();
        tokio::spawn(async move {
            let rebuild =
                switch_model_task(agent, tag, &client, config, skills, project_root, manager).await;
            let _ = notify.send(Event::AgentRebuilt(Box::new(rebuild))).await;
        });
    }

    /// Open the interactive mode picker.
    fn open_mode_picker(&mut self) {
        if self.agent_unavailable("switch modes") {
            return;
        }
        let items = vec![
            PickerItem {
                value: "genie".to_string(),
                detail: "interactive — bypass permissions; acts without asking".to_string(),
                current: self.app.mode == Mode::Genie,
            },
            PickerItem {
                value: "sovereign".to_string(),
                detail: "autonomous — works continuously; self-directing".to_string(),
                current: self.app.mode == Mode::Sovereign,
            },
        ];
        let selected = items.iter().position(|item| item.current).unwrap_or(0);
        self.app.picker = Some(Picker {
            kind: PickerKind::Mode,
            title: " select mode ".to_string(),
            items,
            selected,
        });
    }

    /// `/agents`: open the subagent roster picker. Lists the built-in and
    /// user-defined subagents with their purpose, tool scope, and step budget.
    /// Selecting one pre-fills a delegation request (subagents are spawned by
    /// the model, so this isn't a direct command).
    fn open_agents_picker(&mut self) {
        let dir = Config::subagents_dir().unwrap_or_default();
        let configs = subagent::available_configs(&dir);
        if configs.is_empty() {
            self.app.notice("no subagents available");
            return;
        }
        let items: Vec<PickerItem> = configs
            .into_iter()
            .map(|config| {
                let scope = match &config.tool_scope {
                    None => "all tools".to_string(),
                    Some(names) => names.join(", "),
                };
                PickerItem {
                    detail: format!("{} · {scope} · {}", config.description, config.max_steps),
                    value: config.name,
                    current: false,
                }
            })
            .collect();
        self.app.picker = Some(Picker {
            kind: PickerKind::Subagent,
            title: " delegate to subagent ".to_string(),
            items,
            selected: 0,
        });
    }

    /// `/plan` (and Shift+Tab): toggle plan mode on the live agent.
    fn toggle_plan(&mut self) {
        if self.agent_unavailable("toggle plan mode") {
            return;
        }
        let on = !self.app.plan_mode;
        if let Some(agent) = self.agent_slot.as_mut() {
            agent.set_plan_mode(on);
        }
        self.app.plan_mode = on;
        // Plain plan mode and omakase are mutually exclusive flavors; turning
        // plan mode off leaves omakase too (mirrors the agent).
        if !on {
            self.app.omakase = false;
        }
        self.app.notice(if on {
            "plan mode on — the agent investigates read-only and presents a plan via \
             exit_plan for approval (/plan or Shift+Tab to leave)"
        } else {
            "plan mode off"
        });
    }

    /// `/omakase`: toggle chef's-choice mode on the live agent. Omakase is a
    /// flavor of plan mode — the agent explores read-only, then decides the
    /// approach itself and auto-approves its own plan (no interview, no review
    /// gate). Enabling it enables plan mode; disabling it drops back to plain
    /// plan mode.
    fn toggle_omakase(&mut self) {
        if self.agent_unavailable("toggle omakase") {
            return;
        }
        let on = !self.app.omakase;
        if let Some(agent) = self.agent_slot.as_mut() {
            agent.set_omakase(on);
        }
        self.app.omakase = on;
        if on {
            self.app.plan_mode = true;
            self.app.notice(
                "omakase on — chef's choice: the agent explores read-only, decides the \
                 approach itself, and executes its own plan (/omakase to leave)",
            );
        } else {
            self.app
                .notice("omakase off — back to plan mode (you review the plan)");
        }
    }

    /// `/rewind`: open the turn picker (newest first). Each row shows the
    /// turn number, the files its edits snapshotted, and the first line of
    /// the prompt that started it. Esc cancels.
    fn open_rewind_picker(&mut self) {
        if self.agent_unavailable("rewind") {
            return;
        }
        let Some(agent) = self.agent_slot.as_ref() else {
            self.app.notice("the agent is busy — try again in a moment");
            return;
        };
        let candidates = agent.rewind_candidates(20);
        if candidates.is_empty() {
            self.app.notice("nothing to rewind yet");
            return;
        }
        let items: Vec<PickerItem> = candidates
            .iter()
            .map(|candidate| {
                let files = candidate
                    .files
                    .iter()
                    .map(|path| {
                        path.file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_else(|| path.display().to_string())
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let detail = match (candidate.prompt.is_empty(), files.is_empty()) {
                    (false, false) => format!("{} · {files}", candidate.prompt),
                    (false, true) => candidate.prompt.clone(),
                    (true, false) => files,
                    (true, true) => String::new(),
                };
                PickerItem {
                    value: candidate.turn.to_string(),
                    detail,
                    current: false,
                }
            })
            .collect();
        self.app.picker = Some(Picker {
            kind: PickerKind::Rewind,
            title: " rewind to before turn ".to_string(),
            items,
            selected: 0,
        });
    }

    /// `/rewind <turn>` (or a picker selection): restore the files and drop
    /// the rewound turns from the session and the transcript.
    fn rewind(&mut self, turn: u64) {
        if self.agent_unavailable("rewind") {
            return;
        }
        let Some(agent) = self.agent_slot.as_mut() else {
            self.app.notice("the agent is busy — try again in a moment");
            return;
        };
        match agent.rewind_to(turn) {
            Ok(restored) => {
                // The rewound turns no longer exist: reset the transcript
                // view to match the truncated conversation.
                self.app.transcript.clear();
                self.app.streaming.clear();
                self.app.streaming_thinking.clear();
                self.app.scroll_to_bottom();
                let files = if restored.is_empty() {
                    "no files needed restoring".to_string()
                } else {
                    format!(
                        "restored {}",
                        restored
                            .iter()
                            .map(|path| path.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                self.app.notice(format!(
                    "rewound to before turn {turn} — {files}; conversation truncated"
                ));
            }
            Err(err) => self.app.notice(format!("rewind failed: {err:#}")),
        }
    }

    /// `/resume <id>` (or a picker selection): swap the live agent for one
    /// reopened on session `id` and replay its transcript. The agent must be
    /// idle (the slot is taken during a turn).
    async fn resume_session(&mut self, id: String) {
        if id == self.app.session_id {
            self.app.notice("already in this session");
            return;
        }
        if self.agent_unavailable("resume a session") {
            return;
        }
        if self.agent_slot.is_none() {
            self.app.notice("the agent is busy — try again in a moment");
            return;
        }
        let manager = self.manager.lock().await;
        let agent = build_agent(
            self.client,
            &self.app.config,
            self.skills,
            self.project_root,
            &manager,
            SessionTarget::Id(id.clone()),
        )
        .await;
        drop(manager);
        let mut agent = match agent {
            Ok(agent) => agent,
            Err(err) => {
                self.app
                    .notice(format!("could not resume session: {err:#}"));
                return;
            }
        };
        if self.app.plan_mode {
            agent.set_plan_mode(true);
        }
        // Replay the reopened conversation into the transcript view.
        let messages = agent.session().load_messages().unwrap_or_default();
        let resumed_id = agent.session().id.clone();
        let turns = messages
            .iter()
            .filter(|m| m.role == crate::llm::Role::User)
            .count();
        let name = messages
            .iter()
            .find(|m| m.role == crate::llm::Role::User)
            .and_then(|m| m.content.lines().next())
            .map(|line| line.trim().chars().take(48).collect::<String>())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| resumed_id.clone());
        self.app.load_transcript(messages);
        *self.agent_slot = Some(agent);

        // Hand this session's identity over to the new one: drop the old
        // heartbeat, adopt the resumed id, and re-register.
        crate::session_registry::remove(&self.app.session_id);
        self.app.session_id = resumed_id.clone();
        self.app.session_name = name;
        crate::session_registry::write(&self.app.session_record());
        self.app
            .notice(format!("resumed session {resumed_id} · {turns} turns"));
    }

    /// `/compact`: ask the main loop to run compaction in the background (it
    /// owns the agent slot). Guarded so it can't stack on a busy/rebuilding
    /// agent or a compaction already in flight.
    fn request_compact(&mut self) {
        if self.agent_unavailable("compact") {
            return;
        }
        if self.app.compacting {
            self.app.notice("already compacting");
            return;
        }
        if self.agent_slot.is_none() {
            self.app.notice("the agent is busy — try again in a moment");
            return;
        }
        self.app.pending_compact = true;
    }

    fn switch_mode(&mut self, mode: Mode) {
        if self.agent_unavailable("switch modes") {
            return;
        }
        if let Some(agent) = self.agent_slot.as_mut() {
            agent.set_mode(mode);
        }
        self.app.mode = mode;
        self.app.config.mode = mode;
        self.app.status.mode = mode;
        match mode {
            Mode::Sovereign => {
                self.app.config.max_steps = self.app.config.max_steps.for_mode(Mode::Sovereign);
            }
            Mode::Genie => {
                self.app.config.max_steps = self.genie_max_steps;
            }
        }
        self.app.status.max_steps = self.app.config.max_steps;
        // Persist so the mode survives a restart (consistent with /provider).
        self.persist_config();
        self.app.notice(format!("switched to {mode} mode"));
    }

    /// Open the interactive reasoning-effort picker (`/effort`).
    fn open_effort_picker(&mut self) {
        if self.agent_unavailable("change effort") {
            return;
        }
        let current = self.app.config.reasoning_effort;
        let rows = [
            (
                "high",
                "most reasoning — slowest, best on hard tasks",
                Some(ReasoningEffort::High),
            ),
            (
                "medium",
                "balanced reasoning",
                Some(ReasoningEffort::Medium),
            ),
            (
                "low",
                "least reasoning — fastest, cheapest",
                Some(ReasoningEffort::Low),
            ),
            (
                "default",
                "leave the provider default (e.g. Grok 4.5 → high)",
                None,
            ),
        ];
        let items: Vec<PickerItem> = rows
            .iter()
            .map(|(value, detail, effort)| PickerItem {
                value: (*value).to_string(),
                detail: (*detail).to_string(),
                current: *effort == current,
            })
            .collect();
        let selected = items.iter().position(|item| item.current).unwrap_or(0);
        self.app.picker = Some(Picker {
            kind: PickerKind::Effort,
            title: " reasoning effort ".to_string(),
            items,
            selected,
        });
    }

    /// Set the reasoning effort (`/effort <level>`): applies to the live agent
    /// and persists so it survives a restart. Only reaches providers whose
    /// models accept a `reasoning_effort` field; others ignore it.
    fn set_effort(&mut self, effort: Option<ReasoningEffort>) {
        if self.agent_unavailable("change effort") {
            return;
        }
        if let Some(agent) = self.agent_slot.as_mut() {
            agent.set_reasoning_effort(effort);
        }
        self.app.config.reasoning_effort = effort;
        self.persist_config();
        match effort {
            Some(effort) => self.app.notice(format!("reasoning effort: {effort}")),
            None => self
                .app
                .notice("reasoning effort: provider default".to_string()),
        }
    }

    async fn reload(&mut self) {
        if self.agent_unavailable("reload") {
            return;
        }
        *self.skills = load_skill_roots();
        self.app.custom_commands = crate::commands::load(self.project_root);
        let mut manager = self.manager.lock().await;
        match McpConfig::load(self.mcp_path) {
            Ok(mcp_config) => {
                if let Err(err) = manager.reload(&mcp_config).await {
                    self.app.notice(format!("MCP reload warning: {err:#}"));
                }
            }
            Err(err) => self
                .app
                .notice(format!("could not reload MCP config: {err:#}")),
        }
        // The rebuilt registry's subagent spawner keeps the session's hooks.
        let Some(hooks) = self
            .agent_slot
            .as_ref()
            .map(|agent| Arc::clone(agent.hooks()))
        else {
            return;
        };
        match build_registry(&manager, self.client, &hooks).await {
            Ok((registry, subagent_model)) => {
                let tool_count = registry.len();
                if let Some(agent) = self.agent_slot.as_mut() {
                    agent.set_registry(registry);
                    agent.bind_subagent_model(subagent_model);
                    agent.set_skills(self.skills.clone());
                }
                self.app.notice(format!(
                    "reloaded: {tool_count} tools, {} skills",
                    self.skills.len()
                ));
            }
            Err(err) => self.app.notice(format!("reload failed: {err:#}")),
        }
    }

    /// Merge the already-connected MCP servers' tools into the live agent's
    /// registry. Called after the startup background connect finishes — the
    /// slow part (spawning servers, `initialize`) is already done, so this just
    /// re-enumerates tools and swaps the registry. No-op if the agent is not in
    /// its slot (a turn is running); the main loop defers via `mcp_merge_pending`.
    async fn merge_mcp_registry(&mut self) {
        let Some(hooks) = self
            .agent_slot
            .as_ref()
            .map(|agent| Arc::clone(agent.hooks()))
        else {
            return;
        };
        let manager = self.manager.lock().await;
        match build_registry(&manager, self.client, &hooks).await {
            Ok((registry, subagent_model)) => {
                // Success is silent: tools simply start working and the
                // "connecting tools…" indicator disappears. A success notice
                // here is tool-flex narration and, emitted ~2s in, would float
                // above the user's first message as if it were a reply to it.
                if let Some(agent) = self.agent_slot.as_mut() {
                    agent.set_registry(registry);
                    agent.bind_subagent_model(subagent_model);
                }
            }
            Err(err) => self.app.notice(format!(
                "MCP connected but registry rebuild failed: {err:#}"
            )),
        }
    }

    /// Run a Claude Code import (dispatched from the `/settings` import
    /// picker), then reload custom commands + MCP servers live so the imported
    /// artifacts take effect without a restart.
    async fn import_claude(&mut self, selection: ImportSelection) {
        if self.agent_unavailable("import from Claude Code") {
            return;
        }
        let outcome = match import_claude::run_import(&selection) {
            Ok(outcome) => outcome,
            Err(err) => {
                self.app
                    .notice(format!("Claude Code import failed: {err:#}"));
                return;
            }
        };

        // Adopt the imported spinner verbs (replacing the active list).
        if !outcome.spinner_verbs.is_empty() {
            self.app.config.ui.spinner_verbs = outcome.spinner_verbs.clone();
            self.persist_config();
        }

        // Reload custom commands + MCP servers and rebuild the live tool
        // registry (mirrors `reload`) so imports are usable immediately.
        self.app.custom_commands = crate::commands::load(self.project_root);
        let mut manager = self.manager.lock().await;
        match McpConfig::load(self.mcp_path) {
            Ok(mcp_config) => {
                if let Err(err) = manager.reload(&mcp_config).await {
                    self.app.notice(format!("MCP reload warning: {err:#}"));
                }
            }
            Err(err) => self
                .app
                .notice(format!("could not reload MCP config: {err:#}")),
        }
        if let Some(hooks) = self
            .agent_slot
            .as_ref()
            .map(|agent| Arc::clone(agent.hooks()))
            && let Ok((registry, subagent_model)) =
                build_registry(&manager, self.client, &hooks).await
            && let Some(agent) = self.agent_slot.as_mut()
        {
            agent.set_registry(registry);
            agent.bind_subagent_model(subagent_model);
            agent.set_skills(self.skills.clone());
        }
        drop(manager);

        let summary = outcome.summary();
        self.app.notice(if summary.is_empty() {
            "nothing to import from Claude Code".to_string()
        } else {
            format!("imported from Claude Code:\n{summary}")
        });
    }

    fn evolve(&mut self, deep: bool, description: String) {
        let tier = if deep {
            EvolveTier::Deep
        } else {
            EvolveTier::Runtime
        };
        self.app.notice(format!(
            "evolving ({}): {description}",
            if deep { "deep" } else { "runtime" }
        ));
        // The explicit `/evolve` command is the user's consent; the outcome
        // notice reports exactly what was added.
        let request = EvolveRequest { description, tier };
        let mut evolver = Evolver::new(self.app.config.clone());
        let notify = self.events.sender();
        tokio::spawn(async move {
            let message = match evolver.run(request).await {
                Ok(outcome) => describe_evolve_outcome(&outcome),
                Err(err) => format!("evolve failed: {err:#}"),
            };
            let _ = notify.send(Event::Notice(message)).await;
        });
    }

    /// Fork Wizard to the user's GitHub and surface the one-liner install
    /// command. Runs in a background task so the TUI stays responsive.
    fn publish(&mut self, branch: Option<String>) {
        self.app.notice(format!(
            "publishing Wizard{}…",
            branch
                .as_deref()
                .map(|b| format!(" (branch: {b})"))
                .unwrap_or_default()
        ));
        let config = self.app.config.clone();
        let notify = self.events.sender();
        tokio::spawn(async move {
            let req = PublishRequest { branch };
            let message = match publish(&config, req, false).await {
                Ok(outcome) => format!(
                    "publish: forked to {}  (branch: {})\n\nInstall one-liner:\n{}",
                    outcome.fork_url, outcome.branch, outcome.install_one_liner
                ),
                Err(err) => format!("publish failed: {err:#}"),
            };
            let _ = notify.send(Event::Notice(message)).await;
        });
    }

    /// Persist `App.config` to disk, surfacing any error as a notice.
    fn persist_config(&mut self) {
        if let Err(err) = self.app.config.save() {
            self.app.notice(format!("could not save config: {err:#}"));
        }
    }

    /// Rebuild the live client + agent from the current active provider (after
    /// a `/provider use`/`add`). Runs synchronously; reports `summary` on
    /// success. Mirrors how the model picker probes the backend inline.
    async fn rebuild_active_provider(&mut self, summary: String) {
        let provider = self.app.config.active();
        let client = match provider.build() {
            Ok(client) => client,
            Err(err) => {
                self.app.notice(format!(
                    "could not build provider '{}': {err:#}",
                    provider.name
                ));
                return;
            }
        };
        *self.client = client;
        // A switch to llama.cpp may target a server that is not up yet:
        // kick off the auto-start in the background (the rebuild below
        // proceeds regardless; probes fall back until the model loads).
        if provider.kind == ProviderKind::LlamaCpp
            && server::probe(&provider.base_url).await == server::Health::Down
        {
            self.app.notice(format!(
                "llama-server at {} is not running — starting it…",
                provider.base_url
            ));
            self.start_server_task(provider.clone());
        }
        let model = self.app.config.active().model;
        self.rebuild_agent_with(model, summary, "switched provider")
            .await;
    }

    /// Rebuild the live agent against the current `client` (which the caller has
    /// already set), set the status-bar model label, and report `summary`.
    /// Shared by [`rebuild_active_provider`](Self::rebuild_active_provider) and
    /// the `/fusion` toggle. `context` names the action in the failure notice.
    async fn rebuild_agent_with(&mut self, model_label: String, summary: String, context: &str) {
        let manager = self.manager.lock().await;
        match build_agent(
            self.client,
            &self.app.config,
            self.skills,
            self.project_root,
            &manager,
            SessionTarget::Fresh,
        )
        .await
        {
            Ok(mut agent) => {
                // A rebuilt agent starts with plan mode off; restore the
                // session's setting.
                if self.app.plan_mode {
                    agent.set_plan_mode(true);
                }
                *self.agent_slot = Some(agent);
                self.app.status.model = model_label;
                self.app.notice(summary);
            }
            Err(err) => {
                *self.agent_slot = None;
                self.app.notice(format!(
                    "{context} but could not start the agent: {err:#} — /quit and relaunch"
                ));
            }
        }
    }

    /// Toggle `/fusion`: swap the active client to a
    /// [`FusionProvider`](crate::llm::fusion) (panel debate → synthesizer) when
    /// off, or back to the underlying single provider when on. Like a provider
    /// switch, this resets the session.
    async fn toggle_fusion(&mut self) {
        if self.agent_unavailable("toggle fusion") {
            return;
        }
        if self.app.fusion_active {
            self.app.fusion_active = false;
            self.rebuild_active_provider("fusion off — back to the single model".to_string())
                .await;
            return;
        }

        let fusion = match self.app.config.effective_fusion() {
            Some(fusion) => fusion,
            None => {
                self.app.notice(
                    "fusion needs at least one configured provider — add one with /provider, \
                     then /fusion config",
                );
                return;
            }
        };
        let provider = match self.app.config.build_fusion_from(&fusion) {
            Ok(provider) => provider,
            Err(err) => {
                self.app.notice(format!("could not start fusion: {err:#}"));
                return;
            }
        };
        let label = provider.label();
        *self.client = Arc::new(provider);
        self.app.fusion_active = true;
        self.rebuild_agent_with(
            label.clone(),
            format!("{label} — every turn now fuses the panel; /fusion to turn off"),
            "started fusion",
        )
        .await;
    }

    /// Open the `/fusion config` panel selector: pick which providers form the
    /// debate panel and which synthesizes.
    fn open_fusion_picker(&mut self) {
        self.app.open_fusion_picker();
    }

    /// Handle `/provider` subcommands: list, switch, add, or remove providers.
    async fn provider(&mut self, action: ProviderAction) {
        match action {
            ProviderAction::Menu => self.app.open_provider_picker(),
            ProviderAction::List => self.provider_list(),
            ProviderAction::Use(name) => self.provider_use(name).await,
            ProviderAction::Add {
                name,
                kind,
                base_url,
                model,
                api_key_env,
            } => {
                self.provider_add(name, kind, base_url, model, api_key_env)
                    .await
            }
            ProviderAction::Remove(name) => self.provider_remove(name),
        }
    }

    fn provider_list(&mut self) {
        if self.app.config.providers.is_empty() {
            let synth = self.app.config.active();
            self.app.notice(format!(
                "no providers configured — using the default: {} ({}) {} @ {}\n\
                 add one with /provider (interactive)",
                synth.name, synth.kind, synth.model, synth.base_url
            ));
            return;
        }
        let active = self.app.config.active().name;
        let mut lines = String::from("configured providers:");
        for provider in &self.app.config.providers {
            let marker = if provider.name == active { "* " } else { "  " };
            let key = provider
                .api_key_env
                .as_deref()
                .map(|env| format!(" [key: ${env}]"))
                .unwrap_or_default();
            lines.push_str(&format!(
                "\n{marker}{} ({}) {} @ {}{key}",
                provider.name, provider.kind, provider.model, provider.base_url
            ));
        }
        lines.push_str("\n(* = active)");
        self.app.notice(lines);
    }

    async fn provider_use(&mut self, name: String) {
        if self.agent_unavailable("switch providers") {
            return;
        }
        if !self.app.config.providers.iter().any(|p| p.name == name) {
            self.app
                .notice(format!("no provider named '{name}' — try /provider list"));
            return;
        }
        self.app.config.active_provider = Some(name.clone());
        self.persist_config();
        self.rebuild_active_provider(format!("switched to provider '{name}'"))
            .await;
    }

    async fn provider_add(
        &mut self,
        name: String,
        kind: ProviderKind,
        base_url: String,
        model: String,
        api_key_env: Option<String>,
    ) {
        if self.agent_unavailable("add a provider") {
            return;
        }
        let provider = ProviderConfig {
            name: name.clone(),
            kind,
            base_url,
            model,
            api_key_env: api_key_env.clone(),
            gguf_path: None,
            usd_per_mtok_in: None,
            usd_per_mtok_out: None,
        };
        let reminder = api_key_env
            .map(|env| format!(" — remember to `export {env}=<key>` for this provider"))
            .unwrap_or_default();
        self.add_provider_config(
            provider,
            format!("added and switched to provider '{name}'{reminder}"),
        )
        .await;
    }

    /// Add (or replace) `provider`, switch to it, persist config, and rebuild
    /// the live agent. Shared by the text `/provider add`, the interactive
    /// setup flow, and the xAI OAuth auto-add.
    async fn add_provider_config(&mut self, provider: ProviderConfig, summary: String) {
        let name = provider.name.clone();
        // Dedup by name: replace an existing entry with the same name.
        self.app.config.providers.retain(|p| p.name != name);
        self.app.config.providers.push(provider);
        self.app.config.active_provider = Some(name);
        self.persist_config();
        self.rebuild_active_provider(summary).await;
    }

    /// Finalize an interactive provider setup ([`SlashCommand::ProviderSetup`]):
    /// store the API key in `~/.wizard/credentials.toml` when present, then add
    /// and switch to the provider.
    async fn provider_setup(
        &mut self,
        name: String,
        kind: ProviderKind,
        base_url: String,
        model: String,
        api_key: Option<String>,
    ) {
        if self.agent_unavailable("add a provider") {
            return;
        }
        if let Some(key) = api_key.as_deref()
            && !key.is_empty()
            && let Err(err) = crate::credentials::store(&name, key)
        {
            self.app
                .notice(format!("could not save API key for '{name}': {err:#}"));
        }
        let provider = ProviderConfig {
            name: name.clone(),
            kind,
            base_url,
            model,
            api_key_env: None,
            gguf_path: None,
            usd_per_mtok_in: None,
            usd_per_mtok_out: None,
        };
        self.add_provider_config(provider, format!("added and switched to provider '{name}'"))
            .await;
    }

    fn provider_remove(&mut self, name: String) {
        if self.app.config.active().name == name {
            self.app.notice(format!(
                "'{name}' is the active provider — switch with /provider use <other> first"
            ));
            return;
        }
        let before = self.app.config.providers.len();
        self.app.config.providers.retain(|p| p.name != name);
        if self.app.config.providers.len() == before {
            self.app.notice(format!("no provider named '{name}'"));
            return;
        }
        self.persist_config();
        self.app.notice(format!("removed provider '{name}'"));
    }

    /// Handle `/server` subcommands: status, start, or stop the local
    /// llama-server.
    async fn server(&mut self, action: ServerAction) {
        match action {
            ServerAction::Status => self.server_status().await,
            ServerAction::Start => self.server_start().await,
            ServerAction::Stop => self.server_stop(),
        }
    }

    /// The active provider when it is llama.cpp; otherwise a notice that
    /// `/server` does not apply.
    fn llamacpp_provider(&mut self) -> Option<ProviderConfig> {
        let provider = self.app.config.active();
        if provider.kind == ProviderKind::LlamaCpp {
            Some(provider)
        } else {
            self.app.notice(format!(
                "the active provider '{}' is {} — /server only manages a local llama.cpp server",
                provider.name, provider.kind
            ));
            None
        }
    }

    async fn server_status(&mut self) {
        let Some(provider) = self.llamacpp_provider() else {
            return;
        };
        let spawned = server::spawned_pid()
            .map(|pid| format!(" (PID {pid}, started by wizard)"))
            .unwrap_or_default();
        let line = match server::probe(&provider.base_url).await {
            server::Health::Ready => {
                format!("llama-server at {}: ready{spawned}", provider.base_url)
            }
            server::Health::Loading => format!(
                "llama-server at {}: loading its model{spawned}",
                provider.base_url
            ),
            server::Health::Down => format!(
                "llama-server at {}: not running — start it with /server start",
                provider.base_url
            ),
        };
        self.app.notice(line);
    }

    async fn server_start(&mut self) {
        let Some(provider) = self.llamacpp_provider() else {
            return;
        };
        if server::probe(&provider.base_url).await == server::Health::Ready {
            self.app.notice(format!(
                "llama-server at {} is already running",
                provider.base_url
            ));
            return;
        }
        self.app
            .notice(format!("starting llama-server at {}…", provider.base_url));
        self.start_server_task(provider);
    }

    fn server_stop(&mut self) {
        let message = match server::stop() {
            Ok(server::StopOutcome::Stopped(pid)) => format!("stopped llama-server (PID {pid})"),
            Ok(server::StopOutcome::NotRecorded) => {
                "wizard has not started a llama-server — nothing to stop".to_string()
            }
            Ok(server::StopOutcome::NotRunning(pid)) => {
                format!("llama-server (PID {pid}) already exited")
            }
            Ok(server::StopOutcome::NotOurs { pid, name }) => {
                format!("refusing to stop PID {pid}: it is '{name}', not llama-server")
            }
            Err(err) => format!("could not stop llama-server: {err:#}"),
        };
        self.app.notice(message);
    }

    /// `/login <provider>`: run an OAuth sign-in in the background, streaming
    /// progress (including the URL to open) into the transcript as notices.
    fn login(&mut self, provider: String) {
        if provider != "xai" {
            self.app.notice(format!(
                "unknown login provider '{provider}' (supported: xai)"
            ));
            return;
        }
        let notify = self.events.sender();
        self.app
            .notice("starting the xAI sign-in; your browser should open shortly");
        tokio::spawn(async move {
            let progress = {
                let notify = notify.clone();
                move |line: &str| {
                    // The progress callback is sync; relay each line through
                    // its own send task.
                    let notify = notify.clone();
                    let line = line.to_string();
                    tokio::spawn(async move {
                        let _ = notify.send(Event::Notice(line)).await;
                    });
                }
            };
            match crate::llm::xai_oauth::login(progress).await {
                Ok(()) => {
                    // Auto-add the OAuth provider and switch to it; the main
                    // loop owns the config + agent slot.
                    let provider = ProviderConfig {
                        name: "xai-oauth".to_string(),
                        kind: ProviderKind::XaiOauth,
                        base_url: crate::llm::xai_oauth::DEFAULT_BASE_URL.to_string(),
                        model: crate::llm::xai_oauth::DEFAULT_MODEL.to_string(),
                        api_key_env: None,
                        gguf_path: None,
                        usd_per_mtok_in: None,
                        usd_per_mtok_out: None,
                    };
                    let _ = notify
                        .send(Event::ProviderActivated(Box::new(provider)))
                        .await;
                }
                Err(err) => {
                    let _ = notify
                        .send(Event::Notice(format!("xAI sign-in failed: {err:#}")))
                        .await;
                }
            }
        });
    }

    /// Background half of `/server start` (and the post-switch auto-start):
    /// ensure a llama-server is running for `provider`, streaming progress
    /// into the transcript as notices.
    fn start_server_task(&self, provider: ProviderConfig) {
        let notify = self.events.sender();
        tokio::spawn(async move {
            let progress = NoticeProgress {
                notify: notify.clone(),
            };
            let message = match server::ensure_running(&provider, &progress).await {
                Ok(()) => format!("llama-server at {} is ready", provider.base_url),
                Err(err) => format!("llama-server: {err:#}"),
            };
            let _ = notify.send(Event::Notice(message)).await;
        });
    }
}

/// [`crate::server::Progress`] adapter for the TUI's `/server start`: relays
/// status lines and download milestones into the transcript as notices (the
/// callback is sync, so each line is sent from its own task). Byte progress
/// is throttled to whole-percent steps, the way the plain-terminal download
/// bar fills, so a multi-GB pull does not flood the transcript.
struct NoticeProgress {
    notify: mpsc::Sender<Event>,
}

impl NoticeProgress {
    fn notice(notify: &mpsc::Sender<Event>, line: String) {
        let notify = notify.clone();
        tokio::spawn(async move {
            let _ = notify.send(Event::Notice(line)).await;
        });
    }
}

impl server::Progress for NoticeProgress {
    fn status(&self, line: &str) {
        Self::notice(&self.notify, line.to_string());
    }

    fn bytes(&self, label: &str, total: Option<u64>) -> Box<dyn server::ByteProgress> {
        Box::new(NoticeBytes {
            notify: self.notify.clone(),
            label: label.to_string(),
            total: total.filter(|total| *total > 0),
            written: std::sync::atomic::AtomicU64::new(0),
            last_percent: std::sync::atomic::AtomicU64::new(0),
        })
    }
}

/// Byte-progress guard for [`NoticeProgress`]: emits a transcript notice on
/// each whole-percent advance and a closing milestone on finish.
struct NoticeBytes {
    notify: mpsc::Sender<Event>,
    label: String,
    total: Option<u64>,
    written: std::sync::atomic::AtomicU64,
    last_percent: std::sync::atomic::AtomicU64,
}

impl server::ByteProgress for NoticeBytes {
    fn inc(&self, n: u64) {
        use std::sync::atomic::Ordering;
        let written = self.written.fetch_add(n, Ordering::Relaxed) + n;
        if let Some(total) = self.total {
            let percent = written * 100 / total;
            if percent > self.last_percent.swap(percent, Ordering::Relaxed) {
                NoticeProgress::notice(
                    &self.notify,
                    format!(
                        "{} — {percent}% of {:.1} GB",
                        self.label,
                        total as f64 / 1e9
                    ),
                );
            }
        }
    }

    fn finish(self: Box<Self>, msg: &str) {
        if !msg.is_empty() {
            NoticeProgress::notice(&self.notify, msg.to_string());
        }
    }
}

/// Background half of `/model <tag>`: validate the tag against the
/// installed models, probe native tool support, then either retag the live
/// agent (context preserved) or build a fresh one.
async fn switch_model_task(
    agent: Option<Agent>,
    tag: String,
    client: &Arc<dyn LlmProvider>,
    mut config: Config,
    skills: Vec<Skill>,
    project_root: PathBuf,
    manager: Arc<Mutex<McpManager>>,
) -> AgentRebuild {
    if let Ok(models) = client.list_models().await {
        let known = models
            .iter()
            .any(|m| *m == tag || m.split(':').next() == Some(tag.as_str()));
        if !known {
            // Hand the untouched agent straight back.
            return AgentRebuild {
                agent,
                model: None,
                notice: format!("model '{tag}' is not installed (try `ollama pull {tag}`)"),
            };
        }
    }
    let native_tools = match client.supports_native_tools(&tag).await {
        Ok(supported) => supported,
        Err(err) => {
            tracing::warn!("probing tool support for {tag}: {err:#}");
            false
        }
    };
    match agent {
        Some(mut agent) => {
            agent.set_model(tag.clone(), native_tools);
            AgentRebuild {
                agent: Some(agent),
                model: Some(tag.clone()),
                notice: format!("switched to model {tag} (context preserved)"),
            }
        }
        None => {
            config.model = tag.clone();
            let manager = manager.lock().await;
            match build_agent(
                client,
                &config,
                &skills,
                &project_root,
                &manager,
                SessionTarget::Fresh,
            )
            .await
            {
                Ok(agent) => AgentRebuild {
                    agent: Some(agent),
                    model: Some(tag.clone()),
                    notice: format!("switched to model {tag}"),
                },
                Err(err) => AgentRebuild {
                    agent: None,
                    model: None,
                    notice: format!("failed to switch model: {err:#}"),
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        App::new(Config::default())
    }

    fn press(app: &mut App, code: KeyCode) -> Option<AppAction> {
        app.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
            .expect("key handled")
    }

    fn press_ctrl(app: &mut App, c: char) -> Option<AppAction> {
        app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
            .expect("key handled")
    }

    /// An app with `n` subagent runs on the rail, all still running.
    fn app_with_panes(n: u64) -> App {
        let mut app = app();
        for i in 0..n {
            app.handle_agent_event(AgentEvent::SubagentRunStarted {
                run: i,
                bg: Some(i as u32),
                name: format!("agent{i}"),
                task: format!("task {i}"),
            });
        }
        app
    }

    fn press_mod(app: &mut App, code: KeyCode, mods: KeyModifiers) -> Option<AppAction> {
        app.handle_key(KeyEvent::new(code, mods))
            .expect("key handled")
    }

    fn type_str(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, KeyCode::Char(c));
        }
    }

    /// Untracked (new) files are invisible to plain `git diff`, so `/diff`
    /// must surface them itself — otherwise a tree whose only change is a new
    /// file reads as "(working tree clean)".
    #[tokio::test]
    async fn diff_text_includes_untracked_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .output()
                .expect("git");
        };
        run(&["init"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(root.join("brand_new.txt"), "fresh content\n").expect("write");

        let text = git_diff_text(root).await.expect("diff text");
        assert!(
            text.contains("brand_new.txt") && text.contains("fresh content"),
            "untracked file missing from /diff output:\n{text}"
        );
        assert!(text.contains("# --- untracked ---"));
    }

    /// Wizard's own `.wizard/` session state (checkpoints, snapshots) is an
    /// implementation detail — it must never show up in `/diff`, or the
    /// sidebar fills with internal noise and looks broken.
    #[tokio::test]
    async fn diff_text_omits_wizard_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .output()
                .expect("git");
        };
        run(&["init"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "t"]);
        std::fs::create_dir_all(root.join(".wizard/checkpoints/1")).expect("mkdir");
        std::fs::write(root.join(".wizard/checkpoints/1/0.snap"), "internal\n").expect("write");
        std::fs::write(root.join("real_change.txt"), "user content\n").expect("write");

        let text = git_diff_text(root).await.expect("diff text");
        assert!(
            text.contains("real_change.txt"),
            "real untracked change missing:\n{text}"
        );
        assert!(
            !text.contains(".wizard/checkpoints"),
            "wizard internal state leaked into /diff:\n{text}"
        );
    }

    #[test]
    fn is_wizard_state_path_matches_state_dir_only() {
        assert!(is_wizard_state_path(".wizard/checkpoints/1/0.snap"));
        assert!(is_wizard_state_path("sub/.wizard/x"));
        assert!(is_wizard_state_path(".wizard"));
        assert!(!is_wizard_state_path("src/wizard.rs"));
        assert!(!is_wizard_state_path(".wizardrc"));
    }

    /// The diff sidebar paginates with PgUp/PgDn (offset from the top) and Esc
    /// closes it — without this a diff taller than the pane is unreadable.
    #[test]
    fn diff_sidebar_pages_and_closes() {
        let mut app = app();
        app.show_diff = true;
        assert_eq!(app.diff_scroll, 0);

        press(&mut app, KeyCode::PageDown);
        assert_eq!(app.diff_scroll, 10, "PgDn scrolls the diff down");
        press(&mut app, KeyCode::PageUp);
        assert_eq!(app.diff_scroll, 0, "PgUp scrolls back up");
        // PgUp at the top stays clamped (no underflow).
        press(&mut app, KeyCode::PageUp);
        assert_eq!(app.diff_scroll, 0);

        // While the diff owns paging, the transcript scroll is untouched.
        assert_eq!(app.scroll, 0);

        app.diff_scroll = 30;
        press(&mut app, KeyCode::Esc);
        assert!(!app.show_diff, "Esc closes the diff sidebar");
        assert_eq!(app.diff_scroll, 0, "closing resets the diff scroll");
    }

    #[test]
    fn launch_state_fields_default_inert() {
        let app = app();
        assert!(!app.mcp_connecting, "tools indicator starts hidden");
        assert!(
            app.provider_health_error.is_none(),
            "no provider error until the probe fails"
        );
    }

    #[test]
    fn welcome_stays_up_for_empty_and_notice_only_transcripts() {
        let mut app = app();
        // Fresh launch: nothing typed, welcome screen.
        assert!(!app.has_conversation());

        // Early system notices (provider health, partial MCP failure) land
        // before the first message; they alone must not dismiss the welcome.
        app.notice("error: 1 of 2 MCP servers failed to connect (see logs)");
        app.notice("just a status line");
        assert!(
            !app.has_conversation(),
            "notices alone should not count as conversation"
        );
    }

    #[test]
    fn slash_command_dismisses_the_welcome_screen() {
        let mut app = app();
        assert!(app.welcome_visible());

        // Startup notices land before anything is submitted; they alone must
        // leave the welcome screen up.
        app.notice("error: 1 of 2 MCP servers failed to connect (see logs)");
        assert!(app.welcome_visible());

        // A slash command dispatches without adding transcript entries, but
        // it still begins the session.
        type_str(&mut app, "/effort high");
        press(&mut app, KeyCode::Enter);
        assert!(
            !app.welcome_visible(),
            "a slash command dismisses the welcome screen"
        );
    }

    #[test]
    fn welcome_dismisses_once_real_entries_appear() {
        for entry in [
            TranscriptEntry::User("hi".to_string()),
            TranscriptEntry::Assistant("hello".to_string()),
            TranscriptEntry::ToolCard {
                name: "read".to_string(),
                args: serde_json::json!({}),
                output: None,
                is_error: false,
                collapsed: false,
            },
        ] {
            let mut app = app();
            app.transcript.push(entry);
            assert!(
                app.has_conversation(),
                "a User/Assistant/ToolCard entry begins the conversation"
            );
        }
    }

    #[test]
    fn spinner_verb_starts_from_the_default_list() {
        let app = app();
        assert!(
            crate::config::UiConfig::DEFAULT_SPINNER_VERBS.contains(&app.spinner_verb.as_str())
        );
    }

    #[test]
    fn spinner_verb_is_deterministic_and_stable_within_a_busy_period() {
        let config = Config {
            ui: crate::config::UiConfig {
                spinner_verbs: vec![
                    "Pondering".to_string(),
                    "Musing".to_string(),
                    "Noodling".to_string(),
                ],
                vim: false,
            },
            ..Config::default()
        };
        let mut a = App::new(config.clone());
        let mut b = App::new(config);
        a.tick = 17;
        b.tick = 17;
        a.roll_spinner_verb();
        b.roll_spinner_verb();
        // Same tick and roll count -> same verb.
        assert_eq!(a.spinner_verb, b.spinner_verb);
        // Ticks advancing mid-turn must not change the verb until a re-roll.
        let during = a.spinner_verb.clone();
        a.tick += 5;
        assert_eq!(a.spinner_verb, during);
    }

    #[test]
    fn spinner_verb_rerolls_across_busy_periods() {
        let mut app = app();
        let mut seen = std::collections::HashSet::new();
        for turn in 0..40u64 {
            app.tick = turn * 13;
            app.roll_spinner_verb();
            seen.insert(app.spinner_verb.clone());
        }
        assert!(seen.len() > 1, "verb never varied across busy periods");
    }

    #[test]
    fn slash_filters_suggestions_by_prefix() {
        let mut app = app();
        type_str(&mut app, "/mo");
        let names: Vec<&str> = app.suggestions.iter().map(|s| s.name.as_str()).collect();
        // Prefix matches first, then substring matches ("me*mo*ry").
        assert_eq!(names, ["model", "mode", "memory"]);
        assert_eq!(app.input_mode, InputMode::Command);
    }

    #[test]
    fn suggestions_hide_once_args_are_typed() {
        let mut app = app();
        type_str(&mut app, "/evolve add");
        assert!(app.suggestions.is_empty());
    }

    #[test]
    fn arrow_keys_cycle_suggestions_with_wraparound() {
        let mut app = app();
        type_str(&mut app, "/mo");
        assert_eq!(app.suggestion_index, 0);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.suggestion_index, 1);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.suggestion_index, 2);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.suggestion_index, 0);
        press(&mut app, KeyCode::Up);
        assert_eq!(app.suggestion_index, 2);
    }

    #[test]
    fn tab_completes_the_selected_suggestion() {
        let mut app = app();
        // "/re" would be ambiguous between /rewind and /reload.
        type_str(&mut app, "/rel");
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.input, "/reload");
        assert_eq!(app.cursor, "/reload".chars().count());
    }

    #[test]
    fn tab_completion_appends_space_for_commands_taking_args() {
        let mut app = app();
        type_str(&mut app, "/ev");
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.input, "/evolve ");
    }

    #[test]
    fn enter_completes_and_runs_argless_commands() {
        let mut app = app();
        type_str(&mut app, "/d");
        let action = press(&mut app, KeyCode::Enter);
        assert!(matches!(
            action,
            Some(AppAction::Command(SlashCommand::Diff))
        ));
        assert!(app.input.is_empty());
    }

    #[test]
    fn enter_on_partial_arg_command_completes_and_waits() {
        let mut app = app();
        type_str(&mut app, "/ev");
        let action = press(&mut app, KeyCode::Enter);
        assert!(action.is_none());
        assert_eq!(app.input, "/evolve ");
    }

    #[test]
    fn exactly_typed_command_wins_over_longer_completion() {
        // "model" prefix-matches the typed "mode"; Enter must still run
        // /mode itself, not complete to /model.
        let mut app = app();
        type_str(&mut app, "/mode");
        assert_eq!(app.suggestions[0].name, "mode");
        let action = press(&mut app, KeyCode::Enter);
        assert!(matches!(
            action,
            Some(AppAction::Command(SlashCommand::Mode(None)))
        ));
    }

    fn custom(name: &str, template: &str, description: Option<&str>) -> CustomCommand {
        CustomCommand {
            name: name.to_string(),
            description: description.map(str::to_string),
            template: template.to_string(),
            path: PathBuf::new(),
        }
    }

    #[test]
    fn custom_commands_appear_in_suggestions_after_builtins() {
        let mut app = app();
        app.custom_commands = vec![custom(
            "models-report",
            "Report on $ARGUMENTS",
            Some("report"),
        )];
        type_str(&mut app, "/mo");
        let names: Vec<&str> = app.suggestions.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["model", "mode", "models-report", "memory"]);
        let spec = &app.suggestions[2];
        assert_eq!(spec.description, "report");
        assert!(spec.takes_args);
    }

    #[test]
    fn typed_custom_command_submits_the_expanded_prompt() {
        let mut app = app();
        app.custom_commands = vec![custom("review", "Review $1 with care.", None)];
        type_str(&mut app, "/review src/app.rs");
        let action = press(&mut app, KeyCode::Enter);
        let Some(AppAction::Submit(prepared)) = action else {
            panic!("expected a submit, got {action:?}");
        };
        assert_eq!(prepared.text, "Review src/app.rs with care.");
        // The transcript shows what the user actually typed.
        assert!(matches!(
            app.transcript.last(),
            Some(TranscriptEntry::User(text)) if text == "/review src/app.rs"
        ));
    }

    #[test]
    fn unknown_slash_command_passes_through_as_a_prompt() {
        let mut app = app();
        type_str(&mut app, "/frobnicate the build");
        let action = press(&mut app, KeyCode::Enter);
        assert!(matches!(
            action,
            Some(AppAction::Submit(prepared)) if prepared.text == "/frobnicate the build"
        ));
    }

    #[test]
    fn builtin_command_with_bad_args_keeps_its_usage_notice() {
        let mut app = app();
        type_str(&mut app, "/mode warlock");
        let action = press(&mut app, KeyCode::Enter);
        assert!(action.is_none());
        assert!(matches!(
            app.transcript.last(),
            Some(TranscriptEntry::Notice(text)) if text.contains("unknown mode")
        ));
    }

    #[test]
    fn submit_expands_at_file_references() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("ctx.txt"), "the context\n").unwrap();
        let mut app = app();
        app.project_root = tmp.path().to_path_buf();
        type_str(&mut app, "use @ctx.txt here");
        let action = press(&mut app, KeyCode::Enter);
        let Some(AppAction::Submit(prepared)) = action else {
            panic!("expected a submit, got {action:?}");
        };
        assert!(
            prepared.text.contains("the context"),
            "got: {}",
            prepared.text
        );
        // The transcript keeps the compact form.
        assert!(matches!(
            app.transcript.last(),
            Some(TranscriptEntry::User(text)) if text == "use @ctx.txt here"
        ));
    }

    #[test]
    fn submit_attaches_image_at_refs() {
        let tmp = tempfile::tempdir().unwrap();
        // Minimal 1x1 PNG.
        let png = [
            0x89u8, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00,
            0x00, 0x90, 0x77, 0x53, 0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, 0x54, 0x08,
            0xd7, 0x63, 0xf8, 0xcf, 0xc0, 0x00, 0x00, 0x00, 0x03, 0x00, 0x01, 0x00, 0x05, 0xfe,
            0xd4, 0xef, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ];
        std::fs::write(tmp.path().join("shot.png"), png).unwrap();
        let mut app = app();
        app.project_root = tmp.path().to_path_buf();
        type_str(&mut app, "look at @shot.png");
        let action = press(&mut app, KeyCode::Enter);
        let Some(AppAction::Submit(prepared)) = action else {
            panic!("expected a submit, got {action:?}");
        };
        assert!(
            prepared.text.contains("[image: shot.png]"),
            "got: {}",
            prepared.text
        );
        assert_eq!(prepared.images.len(), 1);
        assert!(prepared.images[0].ends_with("shot.png"));
    }

    #[test]
    fn tab_completes_at_paths_from_the_directory_listing() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("readme.md"), "x").unwrap();
        std::fs::create_dir(tmp.path().join("reach")).unwrap();
        let mut app = app();
        app.project_root = tmp.path().to_path_buf();

        // Common prefix of readme.md / reach.
        type_str(&mut app, "see @re");
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.input, "see @rea");

        // Unique file completes fully.
        type_str(&mut app, "d");
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.input, "see @readme.md");
    }

    #[test]
    fn tab_completes_unique_directory_with_a_trailing_slash() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("sources")).unwrap();
        std::fs::write(tmp.path().join("sources").join("inner.rs"), "x").unwrap();
        let mut app = app();
        app.project_root = tmp.path().to_path_buf();
        type_str(&mut app, "@so");
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.input, "@sources/");
        type_str(&mut app, "in");
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.input, "@sources/inner.rs");
    }

    #[test]
    fn genie_and_sovereign_parse_as_mode_switches() {
        assert_eq!(
            SlashCommand::parse("/genie"),
            Some(Ok(SlashCommand::Mode(Some(Mode::Genie))))
        );
        assert_eq!(
            SlashCommand::parse("/sovereign"),
            Some(Ok(SlashCommand::Mode(Some(Mode::Sovereign))))
        );
    }

    #[test]
    fn effort_parses_levels_default_and_bare() {
        assert_eq!(
            SlashCommand::parse("/effort"),
            Some(Ok(SlashCommand::Effort(None))),
            "bare /effort opens the picker"
        );
        assert_eq!(
            SlashCommand::parse("/effort low"),
            Some(Ok(SlashCommand::Effort(Some(Some(ReasoningEffort::Low)))))
        );
        assert_eq!(
            SlashCommand::parse("/effort HIGH"),
            Some(Ok(SlashCommand::Effort(Some(Some(ReasoningEffort::High))))),
            "level is case-insensitive"
        );
        assert_eq!(
            SlashCommand::parse("/effort default"),
            Some(Ok(SlashCommand::Effort(Some(None)))),
            "default clears back to the provider default"
        );
        assert!(
            matches!(SlashCommand::parse("/effort turbo"), Some(Err(_))),
            "unknown level is an error"
        );
    }

    #[test]
    fn goal_parses_show_and_set() {
        assert_eq!(
            SlashCommand::parse("/goal"),
            Some(Ok(SlashCommand::Goal(None)))
        );
        assert_eq!(
            SlashCommand::parse("/goal ship the thing"),
            Some(Ok(SlashCommand::Goal(Some("ship the thing".into()))))
        );
    }

    #[test]
    fn server_subcommands_parse() {
        assert_eq!(
            SlashCommand::parse("/server"),
            Some(Ok(SlashCommand::Server(ServerAction::Status)))
        );
        assert_eq!(
            SlashCommand::parse("/server status"),
            Some(Ok(SlashCommand::Server(ServerAction::Status)))
        );
        assert_eq!(
            SlashCommand::parse("/server start"),
            Some(Ok(SlashCommand::Server(ServerAction::Start)))
        );
        assert_eq!(
            SlashCommand::parse("/server stop"),
            Some(Ok(SlashCommand::Server(ServerAction::Stop)))
        );
        let parsed = SlashCommand::parse("/server restart").expect("is a slash command");
        let message = parsed.expect_err("unknown subcommand");
        assert!(message.contains("status|start|stop"), "got: {message}");
    }

    #[test]
    fn provider_add_accepts_xai_kinds() {
        let parsed =
            SlashCommand::parse("/provider add xai xai https://api.x.ai/v1 grok-4.3 XAI_API_KEY")
                .expect("is a slash command")
                .expect("parses");
        assert_eq!(
            parsed,
            SlashCommand::Provider(ProviderAction::Add {
                name: "xai".to_string(),
                kind: ProviderKind::Xai,
                base_url: "https://api.x.ai/v1".to_string(),
                model: "grok-4.3".to_string(),
                api_key_env: Some("XAI_API_KEY".to_string()),
            })
        );

        let parsed =
            SlashCommand::parse("/provider add grok xaioauth https://api.x.ai/v1 grok-4.3")
                .expect("is a slash command")
                .expect("parses");
        assert_eq!(
            parsed,
            SlashCommand::Provider(ProviderAction::Add {
                name: "grok".to_string(),
                kind: ProviderKind::XaiOauth,
                base_url: "https://api.x.ai/v1".to_string(),
                model: "grok-4.3".to_string(),
                api_key_env: None,
            })
        );

        // The error for an unknown kind names the xai kinds too.
        let parsed = SlashCommand::parse("/provider add x bogus https://e.com m")
            .expect("is a slash command");
        let message = parsed.expect_err("unknown kind");
        assert!(message.contains("xai|xaioauth"), "got: {message}");
    }

    #[test]
    fn provider_add_accepts_openrouter_kind() {
        let parsed = SlashCommand::parse(
            "/provider add openrouter openrouter https://openrouter.ai/api/v1 openrouter/auto OPENROUTER_API_KEY",
        )
        .expect("is a slash command")
        .expect("parses");
        assert_eq!(
            parsed,
            SlashCommand::Provider(ProviderAction::Add {
                name: "openrouter".to_string(),
                kind: ProviderKind::OpenRouter,
                base_url: "https://openrouter.ai/api/v1".to_string(),
                model: "openrouter/auto".to_string(),
                api_key_env: Some("OPENROUTER_API_KEY".to_string()),
            })
        );

        // The error for an unknown kind names openrouter too.
        let parsed = SlashCommand::parse("/provider add x bogus https://e.com m")
            .expect("is a slash command");
        let message = parsed.expect_err("unknown kind");
        assert!(message.contains("openrouter"), "got: {message}");
    }

    #[test]
    fn provider_add_accepts_cloudflare_kind() {
        let parsed = SlashCommand::parse(
            "/provider add cf cloudflare https://api.cloudflare.com/client/v4/accounts/acc/ai/v1 @cf/zai-org/glm-5.2 CLOUDFLARE_API_TOKEN",
        )
        .expect("is a slash command")
        .expect("parses");
        assert_eq!(
            parsed,
            SlashCommand::Provider(ProviderAction::Add {
                name: "cf".to_string(),
                kind: ProviderKind::Cloudflare,
                base_url: "https://api.cloudflare.com/client/v4/accounts/acc/ai/v1".to_string(),
                model: "@cf/zai-org/glm-5.2".to_string(),
                api_key_env: Some("CLOUDFLARE_API_TOKEN".to_string()),
            })
        );
    }

    #[test]
    fn provider_no_args_opens_the_menu_and_list_still_lists() {
        // Bare `/provider` opens the interactive picker; `/provider list` keeps
        // the scripting/text behavior.
        assert_eq!(
            SlashCommand::parse("/provider"),
            Some(Ok(SlashCommand::Provider(ProviderAction::Menu)))
        );
        assert_eq!(
            SlashCommand::parse("/provider list"),
            Some(Ok(SlashCommand::Provider(ProviderAction::List)))
        );
    }

    #[test]
    fn login_parses_with_a_provider_argument() {
        assert_eq!(
            SlashCommand::parse("/login xai"),
            Some(Ok(SlashCommand::Login("xai".to_string())))
        );
        let parsed = SlashCommand::parse("/login").expect("is a slash command");
        let message = parsed.expect_err("missing provider");
        assert!(message.contains("/login xai"), "got: {message}");
    }

    #[test]
    fn plan_parses_as_a_toggle() {
        assert_eq!(SlashCommand::parse("/plan"), Some(Ok(SlashCommand::Plan)));
    }

    #[test]
    fn todos_and_cost_parse() {
        assert_eq!(SlashCommand::parse("/todos"), Some(Ok(SlashCommand::Todos)));
        assert_eq!(SlashCommand::parse("/cost"), Some(Ok(SlashCommand::Cost)));
    }

    #[test]
    fn fusion_parses_toggle_config_and_rejects_unknown() {
        assert_eq!(
            SlashCommand::parse("/fusion"),
            Some(Ok(SlashCommand::Fusion(FusionAction::Toggle)))
        );
        assert_eq!(
            SlashCommand::parse("/fusion config"),
            Some(Ok(SlashCommand::Fusion(FusionAction::Config)))
        );
        assert!(matches!(SlashCommand::parse("/fusion bogus"), Some(Err(_))));
    }

    #[test]
    fn rewind_parses_with_and_without_a_turn() {
        assert_eq!(
            SlashCommand::parse("/rewind"),
            Some(Ok(SlashCommand::Rewind(None)))
        );
        assert_eq!(
            SlashCommand::parse("/rewind 7"),
            Some(Ok(SlashCommand::Rewind(Some(7))))
        );
        let parsed = SlashCommand::parse("/rewind soon").expect("is a slash command");
        let message = parsed.expect_err("non-numeric turn");
        assert!(message.contains("/rewind [turn]"), "got: {message}");
    }

    #[test]
    fn resume_parses_with_and_without_an_id() {
        assert_eq!(
            SlashCommand::parse("/resume"),
            Some(Ok(SlashCommand::Resume(None)))
        );
        assert_eq!(
            SlashCommand::parse("/resume 2026-06-09T09-30-00"),
            Some(Ok(SlashCommand::Resume(Some(
                "2026-06-09T09-30-00".to_string()
            ))))
        );
    }

    #[test]
    fn compact_parses() {
        assert_eq!(
            SlashCommand::parse("/compact"),
            Some(Ok(SlashCommand::Compact))
        );
    }

    #[test]
    fn resume_picker_selection_becomes_a_resume_command() {
        let mut app = app();
        app.picker = Some(Picker {
            kind: PickerKind::Resume,
            title: " resume session ".to_string(),
            items: vec![PickerItem {
                value: "2026-06-09T09-30-00".to_string(),
                detail: "add resume · 4 msgs".to_string(),
                current: false,
            }],
            selected: 0,
        });
        let action = press(&mut app, KeyCode::Enter);
        assert!(matches!(
            action,
            Some(AppAction::Command(SlashCommand::Resume(Some(id)))) if id == "2026-06-09T09-30-00"
        ));
        assert!(app.picker.is_none(), "the picker closed");
    }

    #[test]
    fn load_transcript_replays_messages_and_pairs_tool_results() {
        use crate::llm::{ChatMessage, FunctionCall, ToolCall};
        let mut app = app();
        let mut assistant = ChatMessage::assistant("reading it");
        assistant.tool_calls.push(ToolCall {
            function: FunctionCall {
                name: "read_file".to_string(),
                arguments: serde_json::json!({ "path": "x.rs" }),
            },
        });
        app.load_transcript(vec![
            ChatMessage::system("ignored system prompt"),
            ChatMessage::user("read x.rs"),
            assistant,
            ChatMessage::tool_result("read_file", "fn main() {}"),
        ]);
        // System dropped; user + assistant + one filled tool card remain.
        assert!(matches!(
            app.transcript.first(),
            Some(TranscriptEntry::User(text)) if text == "read x.rs"
        ));
        assert!(matches!(
            app.transcript.get(1),
            Some(TranscriptEntry::Assistant(text)) if text == "reading it"
        ));
        assert!(matches!(
            app.transcript.get(2),
            Some(TranscriptEntry::ToolCard { name, output: Some(out), .. })
                if name == "read_file" && out == "fn main() {}"
        ));
        assert_eq!(app.transcript.len(), 3);
    }

    #[test]
    fn rewind_picker_selection_becomes_a_rewind_command() {
        let mut app = app();
        app.picker = Some(Picker {
            kind: PickerKind::Rewind,
            title: " rewind to before turn ".to_string(),
            items: vec![
                PickerItem {
                    value: "9".to_string(),
                    detail: "fix tests · notes.txt".to_string(),
                    current: false,
                },
                PickerItem {
                    value: "8".to_string(),
                    detail: String::new(),
                    current: false,
                },
            ],
            selected: 0,
        });
        press(&mut app, KeyCode::Down);
        let action = press(&mut app, KeyCode::Enter);
        assert!(matches!(
            action,
            Some(AppAction::Command(SlashCommand::Rewind(Some(8))))
        ));
        assert!(app.picker.is_none(), "the picker closed");
    }

    #[test]
    fn rewind_picker_esc_cancels() {
        let mut app = app();
        app.picker = Some(Picker {
            kind: PickerKind::Rewind,
            title: " rewind to before turn ".to_string(),
            items: vec![PickerItem {
                value: "3".to_string(),
                detail: String::new(),
                current: false,
            }],
            selected: 0,
        });
        let action = press(&mut app, KeyCode::Esc);
        assert!(action.is_none());
        assert!(app.picker.is_none(), "Esc closed the picker");
    }

    #[test]
    fn agents_and_subagents_parse_to_distinct_commands() {
        // /agents opens the roster picker; /subagents opens the in-session
        // monitor — they are no longer aliases.
        assert!(matches!(
            SlashCommand::parse("/agents"),
            Some(Ok(SlashCommand::Agents))
        ));
        assert!(matches!(
            SlashCommand::parse("/subagents"),
            Some(Ok(SlashCommand::Subagents))
        ));
    }

    #[test]
    fn subagent_picker_selection_prefills_a_delegation_request() {
        let mut app = app();
        app.picker = Some(Picker {
            kind: PickerKind::Subagent,
            title: " delegate to subagent ".to_string(),
            items: vec![
                PickerItem {
                    value: "worker".to_string(),
                    detail: "general-purpose".to_string(),
                    current: false,
                },
                PickerItem {
                    value: "reviewer".to_string(),
                    detail: "code review".to_string(),
                    current: false,
                },
            ],
            selected: 0,
        });
        press(&mut app, KeyCode::Down);
        let action = press(&mut app, KeyCode::Enter);
        // Subagents are model-invoked, so Enter pre-fills input instead of
        // emitting a command.
        assert!(action.is_none());
        assert!(app.picker.is_none(), "the picker closed");
        assert_eq!(app.input, "Use the reviewer subagent to ");
        assert_eq!(app.cursor, app.input.chars().count());
    }

    #[test]
    fn ctrl_c_idle_arms_then_exits() {
        let mut app = app();
        assert!(press_ctrl(&mut app, 'c').is_none());
        assert!(app.ctrl_c_armed);
        assert!(!app.should_quit, "first press only arms");
        assert!(press_ctrl(&mut app, 'c').is_none());
        assert!(app.should_quit, "second press exits");
    }

    #[test]
    fn ctrl_c_busy_interrupts_then_exits() {
        let mut app = app();
        app.status.busy = true;
        // First press while busy interrupts the turn, doesn't quit.
        assert!(matches!(
            press_ctrl(&mut app, 'c'),
            Some(AppAction::Interrupt)
        ));
        assert!(!app.should_quit);
        // Armed now: a second press exits even while busy.
        assert!(press_ctrl(&mut app, 'c').is_none());
        assert!(app.should_quit);
    }

    #[test]
    fn any_other_key_disarms_ctrl_c() {
        let mut app = app();
        press_ctrl(&mut app, 'c');
        assert!(app.ctrl_c_armed);
        press(&mut app, KeyCode::Char('x'));
        assert!(!app.ctrl_c_armed);
        // So the next Ctrl-C re-arms rather than quitting.
        assert!(press_ctrl(&mut app, 'c').is_none());
        assert!(!app.should_quit);
    }

    #[test]
    fn dashboard_command_parses() {
        assert!(matches!(
            SlashCommand::parse("/dashboard"),
            Some(Ok(SlashCommand::Dashboard))
        ));
    }

    #[test]
    fn dashboard_navigates_and_esc_closes() {
        use crate::session_registry::{SessionRecord, SessionState};
        let mut app = app();
        let make = |id: &str, state: SessionState| SessionRecord {
            id: id.to_string(),
            name: id.to_string(),
            cwd: "/tmp".to_string(),
            model: "m".to_string(),
            mode: "genie".to_string(),
            state,
            activity: String::new(),
            pid: 1,
            started_unix: 0,
            updated_unix: 0,
        };
        app.sessions = vec![
            make("a", SessionState::Working),
            make("b", SessionState::Idle),
        ];
        app.show_dashboard = true;

        // ↓ moves the selection and wraps; ↑ wraps back.
        press(&mut app, KeyCode::Down);
        assert_eq!(app.dashboard_selected, 1);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.dashboard_selected, 0, "wraps to the top");
        press(&mut app, KeyCode::Up);
        assert_eq!(app.dashboard_selected, 1, "wraps to the bottom");

        // Esc closes the modal.
        press(&mut app, KeyCode::Esc);
        assert!(!app.show_dashboard);
    }

    #[test]
    fn dashboard_input_composes_and_esc_clears_then_closes() {
        let mut app = app();
        app.show_dashboard = true;
        press(&mut app, KeyCode::Char('h'));
        press(&mut app, KeyCode::Char('i'));
        assert_eq!(app.dashboard_input, "hi");
        press(&mut app, KeyCode::Backspace);
        assert_eq!(app.dashboard_input, "h");
        // Esc with text clears it but keeps the modal open.
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.dashboard_input, "");
        assert!(app.show_dashboard);
        // Esc again, now empty, closes the modal.
        press(&mut app, KeyCode::Esc);
        assert!(!app.show_dashboard);
    }

    #[test]
    fn session_record_reflects_state() {
        let mut app = app();
        app.session_id = "sess-1".to_string();
        app.session_name = "fix bug".to_string();
        assert_eq!(app.session_record().state, SessionState::Idle);
        app.status.busy = true;
        assert_eq!(app.session_record().state, SessionState::Working);
    }

    #[test]
    fn todo_update_mirrors_the_list_and_auto_shows_the_panel_once() {
        use crate::tools::todo::{TodoItem, TodoStatus};
        let mut app = app();
        assert!(!app.show_todos);

        let items = vec![TodoItem {
            content: "first".to_string(),
            status: TodoStatus::InProgress,
        }];
        app.handle_agent_event(AgentEvent::TodoUpdated(items.clone()));
        assert_eq!(app.todos, items);
        assert!(app.show_todos, "first update auto-shows the panel");

        // The user hides it; later updates respect that.
        app.show_todos = false;
        app.handle_agent_event(AgentEvent::TodoUpdated(items.clone()));
        assert!(!app.show_todos, "auto-show happens only once");
        assert_eq!(app.todos, items, "the list still updates");
    }

    #[test]
    fn esc_dismisses_the_todo_panel_after_the_diff_sidebar() {
        let mut app = app();
        app.show_todos = true;
        press(&mut app, KeyCode::Esc);
        assert!(!app.show_todos, "Esc dismisses the todo panel");

        // With both sidebars open, Esc closes the diff first, todos second,
        // and only then falls through to the input.
        app.show_todos = true;
        app.show_diff = true;
        app.input = "draft".to_string();
        press(&mut app, KeyCode::Esc);
        assert!(!app.show_diff, "diff closes first");
        assert!(app.show_todos, "todos stay open until the next Esc");
        press(&mut app, KeyCode::Esc);
        assert!(!app.show_todos);
        assert_eq!(app.input, "draft", "input untouched while panels close");
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.input, "", "Esc finally clears the input");

        // Vim Normal mode keeps the same escape hatch.
        let mut app = vim_app();
        press(&mut app, KeyCode::Esc); // insert -> normal
        app.show_todos = true;
        press(&mut app, KeyCode::Esc);
        assert!(!app.show_todos, "Normal-mode Esc dismisses the todo panel");
    }

    #[test]
    fn usage_events_drive_session_totals_and_the_context_meter() {
        let mut app = app();
        app.handle_agent_event(AgentEvent::Usage {
            prompt_tokens: 100,
            completion_tokens: 20,
        });
        app.handle_agent_event(AgentEvent::Usage {
            prompt_tokens: 50,
            completion_tokens: 5,
        });
        // Session lifetime still accumulates for /cost.
        assert_eq!(app.status.prompt_tokens, 150);
        assert_eq!(app.status.completion_tokens, 25);
        // Context meter tracks the most recent prompt size, not the sum.
        assert_eq!(app.status.context_tokens, 50);

        // Auto-compaction replaces the meter without touching lifetime totals.
        app.handle_agent_event(AgentEvent::ContextSize { tokens: 12 });
        assert_eq!(app.status.context_tokens, 12);
        assert_eq!(app.status.prompt_tokens, 150);
        assert_eq!(app.status.completion_tokens, 25);
    }

    #[test]
    fn background_task_events_drive_the_live_status_bar_counter() {
        let mut app = app();
        assert_eq!(app.status.background_tasks, 0);

        app.handle_agent_event(AgentEvent::TaskStarted {
            id: 1,
            command: "sleep 5".to_string(),
        });
        assert_eq!(
            app.status.background_tasks, 1,
            "marker appears while running"
        );

        app.handle_agent_event(AgentEvent::TaskStarted {
            id: 2,
            command: "ping -c 1 example.com".to_string(),
        });
        assert_eq!(app.status.background_tasks, 2);

        app.handle_agent_event(AgentEvent::TaskFinished {
            id: 1,
            command: "sleep 5".to_string(),
            status: crate::tools::tasks::TaskStatus::Done(0),
        });
        assert_eq!(
            app.status.background_tasks, 1,
            "counter drops back down as tasks finish"
        );
        assert!(matches!(
            app.transcript.last(),
            Some(TranscriptEntry::Notice(text))
                if text.contains("background task #1 finished")
        ));

        app.handle_agent_event(AgentEvent::TaskFinished {
            id: 2,
            command: "ping -c 1 example.com".to_string(),
            status: crate::tools::tasks::TaskStatus::Done(0),
        });
        assert_eq!(
            app.status.background_tasks, 0,
            "marker clears once all finish"
        );
    }

    #[test]
    fn failed_tool_cards_start_collapsed() {
        let mut app = app();
        app.handle_agent_event(AgentEvent::ToolStarted {
            name: "web_fetch".to_string(),
            args: serde_json::json!({"url": "https://example.com"}),
        });
        app.handle_agent_event(AgentEvent::ToolFinished {
            name: "web_fetch".to_string(),
            output: crate::tools::ToolOutput::error("HTTP 403 Forbidden\n<!DOCTYPE html>\n..."),
        });
        assert!(
            matches!(
                app.transcript.last(),
                Some(TranscriptEntry::ToolCard {
                    is_error: true,
                    collapsed: true,
                    ..
                })
            ),
            "errors show only the ✗ card line until expanded via Ctrl-T"
        );

        // Short successful outputs still arrive expanded.
        app.handle_agent_event(AgentEvent::ToolStarted {
            name: "read_file".to_string(),
            args: serde_json::json!({"path": "a.txt"}),
        });
        app.handle_agent_event(AgentEvent::ToolFinished {
            name: "read_file".to_string(),
            output: crate::tools::ToolOutput::ok("one line"),
        });
        assert!(matches!(
            app.transcript.last(),
            Some(TranscriptEntry::ToolCard {
                is_error: false,
                collapsed: false,
                ..
            })
        ));
    }

    #[test]
    fn stream_retry_discards_the_partial_streamed_text() {
        let mut app = app();
        app.handle_agent_event(AgentEvent::TextDelta("half an ans".to_string()));
        app.handle_agent_event(AgentEvent::StreamRetrying);
        app.handle_agent_event(AgentEvent::Error(
            "LLM unavailable (stream stalled); sleeping 5s then retrying (attempt 1)".to_string(),
        ));
        assert!(
            app.streaming.is_empty(),
            "the doomed attempt's partial text is dropped, not flushed"
        );
        assert!(
            !app
                .transcript
                .iter()
                .any(|entry| matches!(entry, TranscriptEntry::Assistant(text) if text.contains("half an ans"))),
            "no assistant entry made of the discarded partial"
        );

        // The retry streams the full answer; only that lands.
        app.handle_agent_event(AgentEvent::TextDelta("the full answer".to_string()));
        assert_eq!(app.streaming, "the full answer");
    }

    #[test]
    fn long_outputs_start_collapsed_by_lines_or_length() {
        assert!(!collapse_long("short output"));
        assert!(!collapse_long(&"line\n".repeat(6)));
        assert!(collapse_long(&"line\n".repeat(7)), "more than six lines");
        assert!(
            collapse_long(&"x".repeat(601)),
            "a giant single line wraps to fill the screen just the same"
        );
    }

    fn click(app: &mut App, column: u16, row: u16) {
        use crossterm::event::MouseEvent;
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            let _ = app.handle_event(Event::Mouse(MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            }));
        }
    }

    #[test]
    fn clicking_a_tool_card_header_toggles_its_output() {
        let mut app = app();
        app.handle_agent_event(AgentEvent::ToolStarted {
            name: "execute".to_string(),
            args: serde_json::json!({"command": "ls"}),
        });
        app.handle_agent_event(AgentEvent::ToolFinished {
            name: "execute".to_string(),
            output: crate::tools::ToolOutput::ok("a\nb\nc\nd\ne\nf\ng\nh"),
        });
        let index = app.transcript.len() - 1;
        assert!(matches!(
            app.transcript.last(),
            Some(TranscriptEntry::ToolCard {
                collapsed: true,
                ..
            })
        ));

        // Render a frame so the click hit map is populated.
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| crate::ui::draw(frame, &app)).unwrap();
        let (row, hit_index) = *app
            .card_hits
            .borrow()
            .first()
            .expect("the card header should be clickable");
        assert_eq!(hit_index, index);

        // A plain click on the header expands the card...
        click(&mut app, 2, row);
        assert!(matches!(
            app.transcript.get(index),
            Some(TranscriptEntry::ToolCard {
                collapsed: false,
                ..
            })
        ));

        // ...and a second click (at its possibly-shifted row) collapses it.
        terminal.draw(|frame| crate::ui::draw(frame, &app)).unwrap();
        let row = app.card_hits.borrow().first().map(|(y, _)| *y).unwrap();
        click(&mut app, 2, row);
        assert!(matches!(
            app.transcript.get(index),
            Some(TranscriptEntry::ToolCard {
                collapsed: true,
                ..
            })
        ));
    }

    /// A real PNG on disk, as the image store would have left it: a solid red
    /// square, so any cell that drew it is unmistakable.
    fn red_png(dir: &Path) -> ImageRef {
        let path = dir.join("red.png");
        image::RgbaImage::from_pixel(48, 48, image::Rgba([255, 0, 0, 255]))
            .save(&path)
            .expect("wrote the png");
        ImageRef {
            path,
            mime: "image/png".to_string(),
            bytes: std::fs::metadata(dir.join("red.png")).unwrap().len() as usize,
        }
    }

    /// Every cell of a drawn frame, row by row: what is on screen.
    fn screen(app: &App, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| crate::ui::draw(frame, app)).unwrap();
        terminal.backend().buffer().clone()
    }

    /// Rows holding image pixels. The UI is deliberately monochrome — white,
    /// grays, and no hues anywhere (see [`crate::ui`]) — so a cell painted in
    /// 24-bit colour is an image cell and nothing else. That makes this both the
    /// "it drew" check and the "it left nothing behind" check.
    fn pixel_rows(buf: &ratatui::buffer::Buffer) -> Vec<u16> {
        use ratatui::style::Color;
        (0..buf.area.height)
            .filter(|&y| {
                (0..buf.area.width).any(|x| {
                    let cell = buf.cell((x, y)).unwrap();
                    matches!(cell.fg, Color::Rgb(..)) || matches!(cell.bg, Color::Rgb(..))
                })
            })
            .collect()
    }

    fn row_text(buf: &ratatui::buffer::Buffer, y: u16) -> String {
        (0..buf.area.width)
            .map(|x| buf.cell((x, y)).unwrap().symbol())
            .collect()
    }

    #[test]
    fn an_image_from_the_model_and_one_from_a_tool_both_render_with_their_file() {
        let dir = tempfile::tempdir().unwrap();
        let image = red_png(dir.path());
        let mut app = app();
        app.welcome_dismissed = true;

        app.handle_agent_event(AgentEvent::TextDelta("here it is".to_string()));
        app.handle_agent_event(AgentEvent::Images {
            source: ImageSource::Assistant,
            images: vec![image.clone()],
        });
        app.handle_agent_event(AgentEvent::ToolStarted {
            name: "render".to_string(),
            args: serde_json::json!({}),
        });
        app.handle_agent_event(AgentEvent::ToolFinished {
            name: "render".to_string(),
            output: crate::tools::ToolOutput::ok("drawn"),
        });
        app.handle_agent_event(AgentEvent::Images {
            source: ImageSource::Tool("render".to_string()),
            images: vec![image.clone()],
        });
        assert_eq!(
            app.transcript
                .iter()
                .filter(|entry| matches!(entry, TranscriptEntry::Image { .. }))
                .count(),
            2,
        );

        let buf = screen(&app, 80, 40);
        let text: String = (0..buf.area.height)
            .map(|y| row_text(&buf, y))
            .collect::<Vec<_>>()
            .join("\n");

        // Both images were drawn, in pixels.
        assert_eq!(
            pixel_rows(&buf).len(),
            6,
            "two three-row image blocks, drawn in pixels:\n{text}"
        );
        // Each is named by what made it, and each names its file — untruncated,
        // on a line of its own, so it can be copied out and opened.
        assert!(text.contains("image · image/png"), "{text}");
        assert!(text.contains("image from `render` · image/png"), "{text}");
        let path = image.path.display().to_string();
        assert_eq!(
            (0..buf.area.height)
                .filter(|&y| row_text(&buf, y).trim() == path)
                .count(),
            2,
            "each image's path stands alone on its own row:\n{text}"
        );
    }

    #[test]
    fn a_scrolled_image_is_clipped_to_the_transcript_and_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let image = red_png(dir.path());
        let mut app = app();
        app.welcome_dismissed = true;
        app.handle_agent_event(AgentEvent::Images {
            source: ImageSource::Assistant,
            images: vec![image],
        });
        // Enough text after it to push the image off the top of a short screen.
        for line in 0..12 {
            app.handle_agent_event(AgentEvent::Notice(format!("line {line}")));
        }

        // Pinned to the bottom, the image is above the viewport: no pixels.
        let (width, height) = (60u16, 12u16);
        let buf = screen(&app, width, height);
        assert!(pixel_rows(&buf).is_empty(), "the image is scrolled away");

        // Scroll it back into view a row at a time. However the block straddles
        // the edge of the viewport, its pixels stay inside the transcript body —
        // never in the composer, the rail or the status bar below it.
        let body = crate::ui::regions(&app, ratatui::layout::Rect::new(0, 0, width, height))[0];
        let mut ever_drawn = false;
        for _ in 0..20 {
            app.scroll_transcript(1);
            let buf = screen(&app, width, height);
            let rows = pixel_rows(&buf);
            ever_drawn |= !rows.is_empty();
            for row in rows {
                assert!(
                    row < body.bottom(),
                    "row {row} has pixels below the transcript body (which ends at {})",
                    body.bottom()
                );
            }
        }
        assert!(
            ever_drawn,
            "scrolling back never brought the image into view"
        );

        // And back at the bottom, the screen is exactly what it was before the
        // scroll — no pixels left over anywhere.
        app.scroll_to_bottom();
        assert!(pixel_rows(&screen(&app, width, height)).is_empty());
    }

    #[test]
    fn a_subagents_image_renders_inside_that_runs_pane() {
        let dir = tempfile::tempdir().unwrap();
        let image = red_png(dir.path());
        let mut app = app();
        app.welcome_dismissed = true;
        app.handle_agent_event(AgentEvent::SubagentRunStarted {
            run: 1,
            bg: None,
            name: "researcher".to_string(),
            task: "look".to_string(),
        });
        app.handle_agent_event(AgentEvent::SubagentRunImages {
            run: 1,
            source: ImageSource::Tool("screenshot".to_string()),
            images: vec![image],
        });

        // The run's image is its own: the main chat, which the subagent has said
        // nothing to yet, shows no pixels.
        assert!(pixel_rows(&screen(&app, 80, 40)).is_empty());

        // Open the pane and it is there, on the tool that took it.
        app.attached = Some(0);
        let buf = screen(&app, 80, 40);
        assert!(!pixel_rows(&buf).is_empty(), "the pane draws the image");
        let text: String = (0..buf.area.height)
            .map(|y| row_text(&buf, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("image from `screenshot`"), "{text}");
    }

    #[test]
    fn a_resumed_session_replays_the_images_it_left_on_disk() {
        use crate::llm::{ChatMessage, Image};
        let png = Image::new("iVBOR", "image/png").at_path(PathBuf::from("/img/a.png"));

        let mut app = app();
        let mut assistant = ChatMessage::assistant("done");
        assistant.images.push(png.clone());
        app.load_transcript(vec![
            ChatMessage::user("draw"),
            assistant,
            ChatMessage::tool_result("render", "ok"),
            // The images `render` returned, riding back to the model. Not a
            // prompt — the agent wrote it, not the user.
            ChatMessage::user_with_images("Image(s) returned by `render`:", vec![png]),
        ]);

        let images: Vec<&TranscriptEntry> = app
            .transcript
            .iter()
            .filter(|entry| matches!(entry, TranscriptEntry::Image { .. }))
            .collect();
        assert!(
            matches!(
                images.as_slice(),
                [
                    TranscriptEntry::Image {
                        source: ImageSource::Assistant,
                        ..
                    },
                    TranscriptEntry::Image {
                        source: ImageSource::Tool(tool),
                        image,
                    },
                ] if tool == "render" && image.path == Path::new("/img/a.png")
            ),
            "both directions came back, attributed: {images:?}"
        );
        assert!(
            !app.transcript
                .iter()
                .any(|entry| matches!(entry, TranscriptEntry::User(text) if text.contains("Image(s) returned"))),
            "the carrier message is not replayed as something the user said"
        );
    }

    #[test]
    fn backtab_toggles_plan_mode() {
        let mut app = app();
        let action = press(&mut app, KeyCode::BackTab);
        assert!(matches!(
            action,
            Some(AppAction::Command(SlashCommand::Plan))
        ));
    }

    #[test]
    fn backtab_in_a_picker_still_navigates() {
        let mut app = app();
        app.picker = Some(Picker {
            kind: PickerKind::Mode,
            title: " select mode ".to_string(),
            items: vec![
                PickerItem {
                    value: "genie".to_string(),
                    detail: String::new(),
                    current: true,
                },
                PickerItem {
                    value: "sovereign".to_string(),
                    detail: String::new(),
                    current: false,
                },
            ],
            selected: 0,
        });
        let action = press(&mut app, KeyCode::BackTab);
        assert!(action.is_none(), "the picker captured the key");
        assert_eq!(app.picker.as_ref().expect("open").selected, 1);
    }

    /// Open a plan review via the agent event, returning the verdict
    /// receiver.
    fn open_review(app: &mut App, plan: &str) -> tokio::sync::oneshot::Receiver<PlanVerdict> {
        let (respond, rx) = tokio::sync::oneshot::channel();
        app.handle_agent_event(AgentEvent::PlanReady {
            plan: plan.to_string(),
            respond,
        });
        rx
    }

    #[test]
    fn plan_ready_opens_a_review_and_y_approves() {
        let mut app = app();
        let mut rx = open_review(&mut app, "# the plan");
        let review = app.plan_review.as_ref().expect("review open");
        assert_eq!(review.plan, "# the plan");
        assert!(app.plan_mode, "a pending plan implies plan mode");

        // Review keys never leak into the input line.
        press(&mut app, KeyCode::Char('y'));
        assert!(app.input.is_empty());
        assert!(app.plan_review.is_none(), "review closed");
        assert!(!app.plan_mode, "approval clears the plan-mode mirror");
        assert_eq!(rx.try_recv(), Ok(PlanVerdict::approve()));
    }

    #[test]
    fn plan_review_enter_also_approves() {
        let mut app = app();
        let mut rx = open_review(&mut app, "# p");
        let action = press(&mut app, KeyCode::Enter);
        assert!(action.is_none());
        assert_eq!(rx.try_recv(), Ok(PlanVerdict::approve()));
    }

    #[test]
    fn plan_review_rejection_collects_feedback() {
        let mut app = app();
        let mut rx = open_review(&mut app, "# p");

        press(&mut app, KeyCode::Char('n'));
        let review = app.plan_review.as_ref().expect("still open");
        assert_eq!(review.feedback.as_deref(), Some(""));

        type_str(&mut app, "add testz");
        press(&mut app, KeyCode::Backspace);
        type_str(&mut app, "s first");
        assert!(app.input.is_empty(), "feedback typing never hits the input");
        press(&mut app, KeyCode::Enter);

        assert!(app.plan_review.is_none(), "review closed");
        assert!(app.plan_mode, "rejection keeps plan mode on");
        assert_eq!(rx.try_recv(), Ok(PlanVerdict::reject("add tests first")));
    }

    #[test]
    fn plan_review_esc_leaves_feedback_entry() {
        let mut app = app();
        let mut rx = open_review(&mut app, "# p");
        press(&mut app, KeyCode::Char('n'));
        type_str(&mut app, "half a thought");
        press(&mut app, KeyCode::Esc);
        let review = app.plan_review.as_ref().expect("still open");
        assert!(review.feedback.is_none(), "back to the review state");
        assert!(rx.try_recv().is_err(), "no verdict sent yet");
        // 'n' again starts fresh feedback.
        press(&mut app, KeyCode::Char('n'));
        assert_eq!(
            app.plan_review.as_ref().expect("open").feedback.as_deref(),
            Some("")
        );
    }

    /// Open an interview via the agent event, returning the answers receiver.
    fn open_interview(
        app: &mut App,
        questions: Vec<InterviewQuestion>,
    ) -> tokio::sync::oneshot::Receiver<Option<Vec<String>>> {
        let (respond, rx) = tokio::sync::oneshot::channel();
        app.handle_agent_event(AgentEvent::Interview { questions, respond });
        rx
    }

    fn question(q: &str, options: &[&str]) -> InterviewQuestion {
        InterviewQuestion {
            question: q.to_string(),
            options: options.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn omakase_parses_and_round_trips() {
        assert_eq!(
            SlashCommand::parse("/omakase"),
            Some(Ok(SlashCommand::Omakase))
        );
    }

    /// Parse `input` and return the agent-runnable verdict, asserting it is a
    /// well-formed command first.
    fn runnable(input: &str) -> Result<(), String> {
        match SlashCommand::parse(input) {
            Some(Ok(command)) => command.agent_runnable(),
            other => panic!("{input} did not parse to a command: {other:?}"),
        }
    }

    #[test]
    fn agent_runnable_allows_self_config_and_info_commands() {
        for input in [
            "/effort high",
            "/model claude-sonnet-5",
            "/mode sovereign",
            "/goal ship it",
            "/goal",
            "/status",
            "/diff",
            "/compact",
            "/reload",
            "/settings",
            "/fusion",
        ] {
            assert!(runnable(input).is_ok(), "{input} should be runnable");
        }
    }

    #[test]
    fn command_requested_event_queues_for_post_turn_dispatch() {
        let mut app = app();
        assert!(app.pending_agent_commands.is_empty());
        app.handle_agent_event(AgentEvent::CommandRequested("/effort high".into()));
        assert_eq!(app.pending_agent_commands, vec!["/effort high".to_string()]);
        // A second request accumulates rather than replacing.
        app.handle_agent_event(AgentEvent::CommandRequested("/compact".into()));
        assert_eq!(
            app.pending_agent_commands,
            vec!["/effort high".to_string(), "/compact".to_string()]
        );
    }

    #[test]
    fn agent_runnable_refuses_pickers_and_dangerous_commands() {
        for input in [
            "/effort",   // interactive picker without an argument
            "/model",    // interactive picker without an argument
            "/mode",     // interactive picker without an argument
            "/quit",     // ends the session
            "/clear",    // wipes the conversation
            "/rewind 2", // restores checkpoints
            "/resume",   // switches sessions
            "/login xai",
            "/provider list",
            "/publish",
            "/agents",
            "/fusion config",
        ] {
            assert!(runnable(input).is_err(), "{input} should be refused");
        }
    }

    #[test]
    fn interview_collects_answers_and_advances() {
        let mut app = app();
        let mut rx = open_interview(
            &mut app,
            vec![
                question("which db?", &["sqlite", "postgres"]),
                question("any auth?", &[]),
            ],
        );
        assert!(app.interview.is_some(), "interview modal open");

        // Pick option 2 for the first question, then accept it with Enter.
        press(&mut app, KeyCode::Char('2'));
        assert_eq!(
            app.interview.as_ref().expect("open").input,
            "postgres",
            "digit fills the matching option"
        );
        assert!(
            app.input.is_empty(),
            "interview keys never hit the input line"
        );
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.interview.as_ref().expect("still open").current, 1);

        // Free-text the second answer.
        type_str(&mut app, "yes, oauth");
        press(&mut app, KeyCode::Enter);

        assert!(
            app.interview.is_none(),
            "interview closed after the last answer"
        );
        assert_eq!(
            rx.try_recv(),
            Ok(Some(vec!["postgres".to_string(), "yes, oauth".to_string()]))
        );
    }

    #[test]
    fn interview_esc_dismisses_with_no_answers() {
        let mut app = app();
        let mut rx = open_interview(&mut app, vec![question("which db?", &[])]);
        press(&mut app, KeyCode::Esc);
        assert!(app.interview.is_none(), "dismissed");
        assert_eq!(rx.try_recv(), Ok(None), "decline sent to the tool");
    }

    #[test]
    fn empty_interview_declines_immediately() {
        let mut app = app();
        let mut rx = open_interview(&mut app, vec![]);
        assert!(app.interview.is_none(), "nothing to ask");
        assert_eq!(rx.try_recv(), Ok(None));
    }

    #[test]
    fn omakase_proceeding_clears_flags_and_shows_the_plan() {
        let mut app = app();
        app.plan_mode = true;
        app.omakase = true;
        app.handle_agent_event(AgentEvent::OmakaseProceeding {
            plan: "# chef plan".to_string(),
        });
        assert!(!app.plan_mode, "chef's choice leaves plan mode");
        assert!(!app.omakase, "omakase cleared once proceeding");
        let shown = app.transcript.iter().any(|e| {
            matches!(
                e,
                TranscriptEntry::ToolCard { output: Some(p), .. } if p == "# chef plan"
            )
        });
        assert!(shown, "the chosen plan is surfaced in the transcript");
    }

    #[test]
    fn cursor_editing_inserts_mid_line() {
        let mut app = app();
        type_str(&mut app, "helo");
        press(&mut app, KeyCode::Left);
        press(&mut app, KeyCode::Char('l'));
        assert_eq!(app.input, "hello");
        press(&mut app, KeyCode::Home);
        press(&mut app, KeyCode::Delete);
        assert_eq!(app.input, "ello");
        press(&mut app, KeyCode::End);
        press(&mut app, KeyCode::Backspace);
        assert_eq!(app.input, "ell");
    }

    #[test]
    fn history_recall_restores_draft() {
        let mut app = app();
        type_str(&mut app, "first message");
        press(&mut app, KeyCode::Enter);
        type_str(&mut app, "second message");
        press(&mut app, KeyCode::Enter);

        type_str(&mut app, "draft");
        press(&mut app, KeyCode::Up);
        assert_eq!(app.input, "second message");
        press(&mut app, KeyCode::Up);
        assert_eq!(app.input, "first message");
        press(&mut app, KeyCode::Down);
        assert_eq!(app.input, "second message");
        press(&mut app, KeyCode::Down);
        assert_eq!(app.input, "draft");
    }

    #[test]
    fn picker_navigation_wraps_and_enter_selects() {
        let mut app = app();
        app.picker = Some(Picker {
            kind: PickerKind::Model,
            title: " select model ".to_string(),
            items: vec![
                PickerItem {
                    value: "qwen3.6:27b".to_string(),
                    detail: String::new(),
                    current: true,
                },
                PickerItem {
                    value: "llama4:8b".to_string(),
                    detail: String::new(),
                    current: false,
                },
            ],
            selected: 0,
        });

        press(&mut app, KeyCode::Up);
        assert_eq!(app.picker.as_ref().expect("open").selected, 1);
        let action = press(&mut app, KeyCode::Enter);
        match action {
            Some(AppAction::Command(SlashCommand::Model(Some(tag)))) => {
                assert_eq!(tag, "llama4:8b");
            }
            other => panic!("expected model switch, got {other:?}"),
        }
        assert!(app.picker.is_none());
    }

    #[test]
    fn picker_escape_cancels() {
        let mut app = app();
        app.picker = Some(Picker {
            kind: PickerKind::Mode,
            title: " select mode ".to_string(),
            items: vec![PickerItem {
                value: "genie".to_string(),
                detail: String::new(),
                current: true,
            }],
            selected: 0,
        });
        press(&mut app, KeyCode::Esc);
        assert!(app.picker.is_none());
    }

    #[test]
    fn ctrl_w_kills_previous_word() {
        let mut app = app();
        type_str(&mut app, "fix the parser bug");
        app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL))
            .expect("key handled");
        assert_eq!(app.input, "fix the parser ");
    }

    #[test]
    fn history_recall_of_slash_command_keeps_browsing_history() {
        let mut app = app();
        type_str(&mut app, "older message");
        press(&mut app, KeyCode::Enter);
        type_str(&mut app, "/model");
        press(&mut app, KeyCode::Enter);

        press(&mut app, KeyCode::Up);
        assert_eq!(app.input, "/model");
        // The recalled slash command repopulates suggestions; ↑ must keep
        // walking history instead of cycling them.
        press(&mut app, KeyCode::Up);
        assert_eq!(app.input, "older message");
    }

    #[test]
    fn unbound_ctrl_chords_do_not_insert_literal_chars() {
        let mut app = app();
        type_str(&mut app, "abc");
        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL))
            .expect("key handled");
        assert_eq!(app.input, "abc");
    }

    #[test]
    fn ctrl_g_requests_external_prompt_edit() {
        let mut app = app();
        type_str(&mut app, "draft in progress");
        let action = press_ctrl(&mut app, 'g');
        assert!(action.is_none());
        assert!(app.pending_edit_prompt);
        // The buffer is only replaced after the editor exits cleanly.
        assert_eq!(app.input, "draft in progress");
    }

    #[test]
    fn ctrl_g_is_inert_during_masked_key_entry() {
        // An API key being typed must never be staged into a temp file.
        let mut app = app();
        app.web_key_backend = Some("brave".to_string());
        type_str(&mut app, "sk-secret");
        press_ctrl(&mut app, 'g');
        assert!(!app.pending_edit_prompt);
        assert_eq!(app.input, "sk-secret", "chord must not insert a literal g");
    }

    #[test]
    fn editor_text_replaces_input_with_cursor_at_end() {
        let mut app = app();
        type_str(&mut app, "old draft");
        app.set_input_from_editor("hello\nworld\n".to_string());
        // Exactly one trailing newline (the editor's) is trimmed.
        assert_eq!(app.input, "hello\nworld");
        assert_eq!(app.cursor, app.input.chars().count());
    }

    #[test]
    fn editor_text_trims_at_most_one_line_ending() {
        let mut app = app();
        app.set_input_from_editor("two\n\n".to_string());
        assert_eq!(app.input, "two\n");
        app.set_input_from_editor("crlf\r\n".to_string());
        assert_eq!(app.input, "crlf");
    }

    #[test]
    fn busy_submit_is_not_recorded_in_history() {
        let mut app = app();
        app.status.busy = true;
        type_str(&mut app, "queued message");
        let action = press(&mut app, KeyCode::Enter);
        assert!(action.is_none());
        assert!(app.history.is_empty());
    }

    #[test]
    fn ctrl_u_kills_to_line_start_keeping_tail() {
        let mut app = app();
        type_str(&mut app, "hello world");
        for _ in 0..6 {
            press(&mut app, KeyCode::Left);
        }
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL))
            .expect("key handled");
        assert_eq!(app.input, " world");
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn submit_rejected_while_agent_rebuilds() {
        let mut app = app();
        app.rebuilding = Some("switching to qwen3:0.6b".to_string());
        type_str(&mut app, "hello");
        let action = press(&mut app, KeyCode::Enter);
        assert!(action.is_none());
        assert!(app.history.is_empty());
        assert!(matches!(
            app.transcript.last(),
            Some(TranscriptEntry::Notice(_))
        ));
    }

    #[test]
    fn ctrl_p_is_a_noop_while_busy() {
        let mut app = app();
        app.status.busy = true;
        let action = app
            .handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL))
            .expect("key handled");
        assert!(action.is_none());
        assert!(app.picker.is_none());
    }

    // --- vim modal editing ---

    fn vim_app() -> App {
        let mut app = app();
        app.toggle_vim();
        assert!(app.vim.enabled);
        assert_eq!(app.vim.mode, VimMode::Insert);
        app
    }

    #[test]
    fn esc_enters_normal_x_deletes_and_i_returns_to_insert() {
        let mut app = vim_app();
        type_str(&mut app, "hello");
        assert_eq!(app.vim.mode, VimMode::Insert);
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.vim.mode, VimMode::Normal);
        // Leaving insert nudges the cursor left onto the last char ('o').
        assert_eq!(app.cursor, 4);
        // In normal mode 'x' deletes the char under the cursor, not insert 'x'.
        press(&mut app, KeyCode::Char('x'));
        assert_eq!(app.input, "hell");
        press(&mut app, KeyCode::Char('i'));
        assert_eq!(app.vim.mode, VimMode::Insert);
    }

    #[test]
    fn word_motions_and_dw_in_normal_mode() {
        let mut app = vim_app();
        type_str(&mut app, "foo bar baz");
        press(&mut app, KeyCode::Esc); // normal, cursor on last 'z'
        press(&mut app, KeyCode::Char('0')); // start of line
        assert_eq!(app.cursor, 0);
        press(&mut app, KeyCode::Char('w')); // -> "bar"
        assert_eq!(app.cursor, 4);
        // dw deletes the word + trailing space.
        press(&mut app, KeyCode::Char('d'));
        press(&mut app, KeyCode::Char('w'));
        assert_eq!(app.input, "foo baz");
    }

    #[test]
    fn insert_transitions_append() {
        let mut app = vim_app();
        type_str(&mut app, "ab");
        press(&mut app, KeyCode::Esc); // normal, cursor on 'b' (index 1)
        press(&mut app, KeyCode::Char('0')); // index 0 ('a')
        press(&mut app, KeyCode::Char('a')); // insert after 'a'
        assert_eq!(app.vim.mode, VimMode::Insert);
        press(&mut app, KeyCode::Char('X'));
        assert_eq!(app.input, "aXb");
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Char('A')); // append at end
        type_str(&mut app, "Z");
        assert_eq!(app.input, "aXbZ");
    }

    #[test]
    fn dd_clears_line_and_u_undoes() {
        let mut app = vim_app();
        type_str(&mut app, "scratch");
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Char('d'));
        press(&mut app, KeyCode::Char('d'));
        assert_eq!(app.input, "");
        press(&mut app, KeyCode::Char('u'));
        assert_eq!(app.input, "scratch");
    }

    #[test]
    fn count_prefix_repeats_motion() {
        let mut app = vim_app();
        type_str(&mut app, "abcdef");
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Char('0'));
        press(&mut app, KeyCode::Char('3'));
        press(&mut app, KeyCode::Char('l')); // 3 right -> index 3
        assert_eq!(app.cursor, 3);
        press(&mut app, KeyCode::Char('2'));
        press(&mut app, KeyCode::Char('x')); // delete 2 chars
        assert_eq!(app.input, "abcf");
    }

    #[test]
    fn delete_then_paste_register() {
        let mut app = vim_app();
        type_str(&mut app, "ab");
        press(&mut app, KeyCode::Esc); // cursor on 'b' (index 1)
        press(&mut app, KeyCode::Char('0')); // index 0
        press(&mut app, KeyCode::Char('x')); // delete 'a' -> register "a", input "b"
        assert_eq!(app.input, "b");
        press(&mut app, KeyCode::Char('p')); // paste after 'b'
        assert_eq!(app.input, "ba");
    }

    #[test]
    fn enter_submits_in_normal_mode() {
        let mut app = vim_app();
        type_str(&mut app, "/help");
        press(&mut app, KeyCode::Esc);
        let action = press(&mut app, KeyCode::Enter);
        assert!(matches!(
            action,
            Some(AppAction::Command(SlashCommand::Help))
        ));
        assert_eq!(app.input, "");
    }

    #[test]
    fn disabled_vim_inserts_hjkl_literally() {
        let mut app = app(); // vim off
        type_str(&mut app, "hjkl");
        press(&mut app, KeyCode::Esc); // plain clear, not a mode switch
        assert_eq!(app.input, "");
    }

    // --- Shift/Alt+Enter newline ---

    #[test]
    fn shift_enter_inserts_newline_without_submitting() {
        let mut app = app();
        type_str(&mut app, "line one");
        let action = press_mod(&mut app, KeyCode::Enter, KeyModifiers::SHIFT);
        assert!(action.is_none());
        type_str(&mut app, "line two");
        assert_eq!(app.input, "line one\nline two");
        // Nothing was submitted.
        assert!(!app.has_conversation());
    }

    #[test]
    fn alt_enter_also_inserts_newline() {
        let mut app = app();
        type_str(&mut app, "a");
        press_mod(&mut app, KeyCode::Enter, KeyModifiers::ALT);
        type_str(&mut app, "b");
        assert_eq!(app.input, "a\nb");
    }

    #[test]
    fn plain_enter_submits_multiline_input() {
        let mut app = app();
        type_str(&mut app, "first");
        press_mod(&mut app, KeyCode::Enter, KeyModifiers::SHIFT);
        type_str(&mut app, "second");
        let action = press(&mut app, KeyCode::Enter);
        match action {
            Some(AppAction::Submit(prepared)) => {
                assert!(
                    prepared.text.contains("first")
                        && prepared.text.contains('\n')
                        && prepared.text.contains("second")
                );
            }
            other => panic!("expected a submit action, got {other:?}"),
        }
        assert_eq!(app.input, "");
    }

    #[test]
    fn shift_enter_inserts_newline_in_vim_normal_mode() {
        let mut app = vim_app();
        type_str(&mut app, "xy");
        press(&mut app, KeyCode::Esc); // NORMAL, cursor on the last char
        let action = press_mod(&mut app, KeyCode::Enter, KeyModifiers::SHIFT);
        // A break is inserted (never submits); the cursor sits on a char, so it
        // lands before it rather than at the very end.
        assert!(action.is_none());
        assert!(app.input.contains('\n'));
        assert_eq!(app.input.chars().filter(|c| !c.is_whitespace()).count(), 2);
    }

    // ---- Subagent rail ---------------------------------------------------

    #[test]
    fn subagent_run_events_build_a_pane() {
        let mut app = app_with_panes(1);
        assert_eq!(app.panes.len(), 1);
        assert_eq!(app.panes[0].name, "agent0");
        assert_eq!(app.panes[0].status, PaneStatus::Running);

        app.handle_agent_event(AgentEvent::SubagentRunToolStarted {
            run: 0,
            name: "read_file".to_string(),
            args: serde_json::json!({"path": "src/app.rs"}),
        });
        app.handle_agent_event(AgentEvent::SubagentRunText {
            run: 0,
            text: "found it".to_string(),
        });

        // The subagent's work lands in *its* pane, not the main transcript.
        assert_eq!(app.panes[0].transcript.len(), 2);
        assert!(app.transcript.is_empty());
        // …and it is flagged as unread, since the user is not watching it.
        assert_eq!(app.panes[0].unread, 2);
    }

    #[test]
    fn concurrent_runs_of_one_subagent_stay_in_separate_panes() {
        let mut app = app();
        for run in [7, 9] {
            app.handle_agent_event(AgentEvent::SubagentRunStarted {
                run,
                bg: None,
                name: "worker".to_string(),
                task: format!("task {run}"),
            });
        }
        app.handle_agent_event(AgentEvent::SubagentRunText {
            run: 9,
            text: "from the second".to_string(),
        });

        assert_eq!(app.panes.len(), 2);
        assert!(app.panes[0].transcript.is_empty());
        assert_eq!(app.panes[1].transcript.len(), 1);
    }

    #[test]
    fn tool_output_lands_on_the_panes_open_card() {
        let mut app = app_with_panes(1);
        app.handle_agent_event(AgentEvent::SubagentRunToolStarted {
            run: 0,
            name: "read_file".to_string(),
            args: Value::Null,
        });
        app.handle_agent_event(AgentEvent::SubagentRunToolFinished {
            run: 0,
            name: "read_file".to_string(),
            output: crate::tools::ToolOutput::ok("contents"),
        });

        assert_eq!(app.panes[0].transcript.len(), 1);
        let TranscriptEntry::ToolCard { output, .. } = &app.panes[0].transcript[0] else {
            panic!("expected a tool card");
        };
        assert_eq!(output.as_deref(), Some("contents"));
    }

    #[test]
    fn down_from_the_composer_focuses_the_rail_then_enter_attaches() {
        let mut app = app_with_panes(2);
        assert_eq!(app.rail_focus, None);

        press(&mut app, KeyCode::Down);
        assert_eq!(app.rail_focus, Some(0));

        press(&mut app, KeyCode::Down);
        assert_eq!(app.rail_focus, Some(1));
        // Clamped at the bottom rather than wrapping — you cannot fall off.
        press(&mut app, KeyCode::Down);
        assert_eq!(app.rail_focus, Some(1));

        press(&mut app, KeyCode::Enter);
        assert_eq!(app.attached, Some(1));
        assert_eq!(
            app.attached_pane().map(|pane| pane.name.as_str()),
            Some("agent1")
        );

        // Esc backs out to the main chat, all the way to the composer.
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.attached, None);
        assert_eq!(app.rail_focus, None);
    }

    #[test]
    fn up_off_the_top_of_the_rail_returns_to_the_composer() {
        let mut app = app_with_panes(2);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.rail_focus, Some(0));

        press(&mut app, KeyCode::Up);
        assert_eq!(app.rail_focus, None);

        // Focus really is back in the composer: typing goes to the input.
        press(&mut app, KeyCode::Char('h'));
        assert_eq!(app.input, "h");
    }

    #[test]
    fn down_still_walks_history_when_there_are_no_subagents() {
        let mut app = app();
        app.history.push("earlier".to_string());
        press(&mut app, KeyCode::Up);
        assert_eq!(app.input, "earlier");
        press(&mut app, KeyCode::Down);
        assert_eq!(app.rail_focus, None);
        assert!(app.input.is_empty());
    }

    #[test]
    fn typing_on_the_rail_hands_focus_back_to_the_composer() {
        let mut app = app_with_panes(1);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.rail_focus, Some(0));

        // The keystroke must not be swallowed by the rail.
        press(&mut app, KeyCode::Char('x'));
        assert_eq!(app.rail_focus, None);
        assert_eq!(app.input, "x");
    }

    #[test]
    fn attaching_clears_the_unread_badge_and_live_entries_stay_read() {
        let mut app = app_with_panes(1);
        app.handle_agent_event(AgentEvent::SubagentRunText {
            run: 0,
            text: "one".to_string(),
        });
        assert_eq!(app.panes[0].unread, 1);

        app.attach_pane(0);
        assert_eq!(app.panes[0].unread, 0);

        // While you are watching, new work is not "unread".
        app.handle_agent_event(AgentEvent::SubagentRunText {
            run: 0,
            text: "two".to_string(),
        });
        assert_eq!(app.panes[0].unread, 0);
    }

    #[test]
    fn run_done_retires_the_pane() {
        let mut app = app_with_panes(1);
        app.handle_agent_event(AgentEvent::SubagentRunDone {
            run: 0,
            completed: true,
            output: "report".to_string(),
            steps_used: 3,
            error: None,
        });
        assert_eq!(app.panes[0].status, PaneStatus::Done);
        assert!(app.panes[0].finished.is_some());
        assert_eq!(app.running_panes(), 0);
    }

    #[test]
    fn the_final_report_lands_in_the_pane() {
        let mut app = app_with_panes(1);
        // The report is the step that made no tool call, so the sub-loop ends
        // on it and never streams it as text — it only arrives on the Done
        // event. The pane must still show it.
        app.handle_agent_event(AgentEvent::SubagentRunDone {
            run: 0,
            completed: true,
            output: "the auth flow starts in login.rs".to_string(),
            steps_used: 2,
            error: None,
        });
        let TranscriptEntry::Assistant(text) = app.panes[0].transcript.last().unwrap() else {
            panic!("expected the report as an assistant message");
        };
        assert_eq!(text, "the auth flow starts in login.rs");
        assert_eq!(app.panes[0].activity(), "the auth flow starts in login.rs");
    }

    #[test]
    fn the_report_is_not_duplicated_when_it_also_streamed() {
        let mut app = app_with_panes(1);
        app.handle_agent_event(AgentEvent::SubagentRunText {
            run: 0,
            text: "all done".to_string(),
        });
        app.handle_agent_event(AgentEvent::SubagentRunDone {
            run: 0,
            completed: true,
            output: "all done".to_string(),
            steps_used: 1,
            error: None,
        });
        assert_eq!(app.panes[0].transcript.len(), 1);
    }

    #[test]
    fn a_failed_run_shows_its_error_in_the_pane() {
        let mut app = app_with_panes(1);
        app.handle_agent_event(AgentEvent::SubagentRunDone {
            run: 0,
            completed: false,
            output: String::new(),
            steps_used: 1,
            error: Some("provider is down".to_string()),
        });
        assert_eq!(app.panes[0].status, PaneStatus::Failed);
        let TranscriptEntry::Notice(text) = &app.panes[0].transcript[0] else {
            panic!("expected a notice");
        };
        assert!(text.contains("provider is down"));
    }

    #[test]
    fn focus_rail_prefers_a_running_pane_over_a_finished_one() {
        let mut app = app_with_panes(2);
        app.handle_agent_event(AgentEvent::SubagentRunDone {
            run: 0,
            completed: true,
            output: "done".to_string(),
            steps_used: 1,
            error: None,
        });
        // agent0 has finished; ↓ should land on the one still working.
        press(&mut app, KeyCode::Down);
        assert_eq!(app.rail_focus, Some(1));
    }

    #[test]
    fn arrows_walk_from_one_pane_straight_into_the_next() {
        let mut app = app_with_panes(3);
        app.attach_pane(0);

        press(&mut app, KeyCode::Down);
        assert_eq!(app.attached, Some(1));
        press(&mut app, KeyCode::Down);
        assert_eq!(app.attached, Some(2));
        // Wraps rather than dead-ending at the last run.
        press(&mut app, KeyCode::Down);
        assert_eq!(app.attached, Some(0));

        press(&mut app, KeyCode::Up);
        assert_eq!(app.attached, Some(2));
        // Browsing runs never scrolls the one you passed through.
        assert!(app.panes.iter().all(|pane| pane.scroll == 0));
    }

    #[test]
    fn shift_arrows_scroll_the_pane_you_are_reading() {
        let mut app = app_with_panes(3);
        app.attach_pane(1);
        // Pretend the last frame had room to scroll (renderer fills this).
        app.panes[1].max_scroll.set(100);

        press_mod(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
        press_mod(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
        assert_eq!(app.attached, Some(1), "shift+↑ must not change pane");
        assert!(!app.panes[1].scroll_follow, "scrolling up leaves the tail");
        assert_eq!(
            app.panes[1].scroll, 98,
            "top-anchored: two lines up from max"
        );

        press_mod(&mut app, KeyCode::Down, KeyModifiers::SHIFT);
        assert_eq!(app.panes[1].scroll, 99);
        assert!(!app.panes[1].scroll_follow);
    }

    #[test]
    fn arrows_in_a_pane_scroll_it_instead_of_recalling_history() {
        let mut app = app_with_panes(1);
        app.history.push("an earlier prompt".to_string());
        app.attach_pane(0);
        app.panes[0].max_scroll.set(100);

        // The bug: ↑/↓ fell through to the composer and walked the main chat's
        // history while the user was plainly looking at a subagent.
        press(&mut app, KeyCode::Up);
        assert!(app.input.is_empty(), "↑ must not recall history in a pane");
        assert!(!app.panes[0].scroll_follow);
        assert_eq!(app.panes[0].scroll, 99);

        press(&mut app, KeyCode::Up);
        assert_eq!(app.panes[0].scroll, 98);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.panes[0].scroll, 99);
        // Pinned at the live tail; it cannot scroll past the bottom.
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        assert!(app.panes[0].scroll_follow, "reaching the bottom re-follows");
        assert_eq!(app.panes[0].scroll, 0);
        assert!(app.input.is_empty());
        assert_eq!(app.attached, Some(0));
    }

    #[test]
    fn transcript_stays_put_while_streaming_after_scroll_up() {
        let mut app = app();
        // Viewport is full and we are following the live tail.
        app.transcript_max_scroll.set(50);
        assert!(app.scroll_follow);

        // User scrolls up to re-read earlier output.
        app.scroll_transcript(10);
        assert!(!app.scroll_follow);
        assert_eq!(app.scroll, 40);

        // Content grows (renderer would bump max_scroll); the top-anchored
        // offset must not change — that is the whole stick-to-bottom contract.
        app.transcript_max_scroll.set(80);
        assert_eq!(app.scroll, 40, "scroll offset holds while content grows");
        assert!(!app.scroll_follow);

        // Scrolling down to the (new) bottom re-enables follow.
        app.scroll_transcript(-100);
        assert!(app.scroll_follow);
        assert_eq!(app.scroll, 0);

        // Ctrl-End is the explicit jump-to-tail chord.
        app.scroll_transcript(5);
        assert!(!app.scroll_follow);
        app.scroll_to_bottom();
        assert!(app.scroll_follow);
        assert_eq!(app.scroll, 0);
    }

    #[test]
    fn wheel_and_page_keys_drive_stick_to_bottom() {
        let mut app = app();
        app.transcript_max_scroll.set(30);

        press(&mut app, KeyCode::PageUp);
        assert!(!app.scroll_follow);
        assert_eq!(app.scroll, 20);

        // One PgDn of 10 lands exactly on the bottom and re-enables follow.
        press(&mut app, KeyCode::PageDown);
        assert!(
            app.scroll_follow,
            "PgDn onto the bottom should re-enable follow"
        );
        assert_eq!(app.scroll, 0);

        // Esc while scrolled away jumps to the tail (instead of clearing input).
        app.scroll_transcript(5);
        assert!(!app.scroll_follow);
        press(&mut app, KeyCode::Esc);
        assert!(app.scroll_follow);

        // Ctrl-End does the same.
        app.scroll_transcript(5);
        press_mod(&mut app, KeyCode::End, KeyModifiers::CONTROL);
        assert!(app.scroll_follow);
    }

    #[test]
    fn esc_from_a_pane_lands_in_the_composer_in_one_press() {
        let mut app = app_with_panes(2);
        app.attach_pane(1);

        press(&mut app, KeyCode::Esc);
        assert_eq!(app.attached, None);
        // Focus is all the way back in the composer, not parked on the rail —
        // one Esc, and you are typing again.
        assert_eq!(app.rail_focus, None);
        press(&mut app, KeyCode::Char('h'));
        assert_eq!(app.input, "h");
    }

    #[test]
    fn a_finished_run_retires_off_the_rail() {
        let mut app = app_with_panes(2);
        app.handle_agent_event(AgentEvent::SubagentRunDone {
            run: 0,
            completed: true,
            output: "report".to_string(),
            steps_used: 1,
            error: None,
        });
        // It lingers first, so you actually see it land.
        app.retire_finished_panes();
        assert_eq!(app.panes.len(), 2);

        // Once its linger is up it drops off, leaving the rail showing live work.
        app.panes[0].finished = Some(Instant::now() - PANE_LINGER - Duration::from_secs(1));
        app.retire_finished_panes();
        assert_eq!(app.panes.len(), 1);
        assert_eq!(app.panes[0].name, "agent1");
        assert_eq!(app.running_panes(), 1);
    }

    #[test]
    fn the_pane_you_are_watching_never_retires_under_you() {
        let mut app = app_with_panes(1);
        app.attach_pane(0);
        app.handle_agent_event(AgentEvent::SubagentRunDone {
            run: 0,
            completed: true,
            output: "report".to_string(),
            steps_used: 1,
            error: None,
        });
        app.panes[0].finished = Some(Instant::now() - PANE_LINGER - Duration::from_secs(1));

        // Long past its linger, but you are reading it.
        app.retire_finished_panes();
        assert_eq!(app.panes.len(), 1);
        assert_eq!(app.attached, Some(0));

        // Esc lets it go, and lands you back in the composer.
        press(&mut app, KeyCode::Esc);
        assert!(app.panes.is_empty());
        assert_eq!(app.attached, None);
        assert_eq!(app.rail_focus, None);
    }

    #[test]
    fn retiring_keeps_the_selection_on_the_run_it_pointed_at() {
        let mut app = app_with_panes(3);
        // Focus the third run, then retire the first.
        app.rail_focus = Some(2);
        app.handle_agent_event(AgentEvent::SubagentRunDone {
            run: 0,
            completed: true,
            output: "done".to_string(),
            steps_used: 1,
            error: None,
        });
        app.panes[0].finished = Some(Instant::now() - PANE_LINGER - Duration::from_secs(1));
        app.retire_finished_panes();

        // Indices shifted, but the selection still points at the same run.
        assert_eq!(app.panes.len(), 2);
        assert_eq!(app.rail_focus, Some(1));
        assert_eq!(app.panes[1].name, "agent2");
    }

    #[test]
    fn a_background_report_survives_its_pane_retiring() {
        let mut app = app_with_panes(1);
        // The card the model got back when it delegated: a placeholder.
        app.transcript.push(TranscriptEntry::ToolCard {
            name: "spawn_subagent".to_string(),
            args: serde_json::json!({"subagent": "agent0", "task": "task 0"}),
            output: Some("Delegated to subagent 'agent0' (#0)".to_string()),
            is_error: false,
            collapsed: false,
        });
        app.handle_agent_event(AgentEvent::SubagentRunDone {
            run: 0,
            completed: true,
            output: "the auth flow starts in login.rs".to_string(),
            steps_used: 4,
            error: None,
        });
        app.panes[0].finished = Some(Instant::now() - PANE_LINGER - Duration::from_secs(1));
        app.retire_finished_panes();
        assert!(app.panes.is_empty());

        // The pane is gone, but the run is still readable in the main chat.
        let TranscriptEntry::ToolCard { output, .. } = &app.transcript[0] else {
            panic!("expected the spawn card");
        };
        assert_eq!(output.as_deref(), Some("the auth flow starts in login.rs"));
    }

    #[test]
    fn the_composer_stays_live_while_attached() {
        let mut app = app_with_panes(1);
        app.attach_pane(0);
        // Better than a modal: you can keep driving the main conversation
        // while you watch a subagent work.
        press(&mut app, KeyCode::Char('h'));
        press(&mut app, KeyCode::Char('i'));
        assert_eq!(app.input, "hi");
        assert_eq!(app.attached, Some(0));
    }

    #[test]
    fn activity_reports_the_tool_in_flight_then_the_last_message() {
        let mut app = app_with_panes(1);
        // Nothing yet: fall back to the task.
        assert_eq!(app.panes[0].activity(), "task 0");

        app.handle_agent_event(AgentEvent::SubagentRunToolStarted {
            run: 0,
            name: "grep".to_string(),
            args: Value::Null,
        });
        assert_eq!(app.panes[0].activity(), "grep");

        app.handle_agent_event(AgentEvent::SubagentRunToolFinished {
            run: 0,
            name: "grep".to_string(),
            output: crate::tools::ToolOutput::ok("hit"),
        });
        app.handle_agent_event(AgentEvent::SubagentRunText {
            run: 0,
            text: "narrowing it down".to_string(),
        });
        assert_eq!(app.panes[0].activity(), "narrowing it down");
    }
}
