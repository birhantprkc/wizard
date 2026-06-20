//! TUI state machine: application state, slash commands, and the genie-mode
//! main loop. Rendering lives in [`crate::ui`]; raw events in
//! [`crate::event`].

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use serde_json::Value;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;

use crate::agent::{Agent, AgentEvent, DoneReason, PlanVerdict, session::Session, subagent};
use crate::cli::Cli;
use crate::commands::CustomCommand;
use crate::config::{Config, Mode, ProviderConfig, ProviderKind};
use crate::event::{Event, EventLoop};
use crate::evolve::{EvolveOutcome, EvolveRequest, EvolveTier, Evolver, PublishRequest, publish};
use crate::hooks::HookEngine;
use crate::import_claude::{self, ImportSelection};
use crate::llm::provider::LlmProvider;
use crate::mcp::{McpConfig, McpManager};
use crate::memory::MemoryStore;
use crate::server;
use crate::session_registry::{self, SessionRecord, SessionState};
use crate::skills::Skill;
use crate::tools::registry::ToolRegistry;
use crate::tools::todo::TodoItem;

/// One rendered entry in the chat transcript.
#[derive(Debug)]
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
    /// System notice (mode switch, reload result, errors).
    Notice(String),
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
    /// `/rewind [turn]` — restore file checkpoints and truncate history.
    /// `None` opens the turn picker; `Some` rewinds to before that turn.
    Rewind(Option<u64>),
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
    /// `/publish [branch]` — fork Wizard and get a one-line installer.
    Publish {
        branch: Option<String>,
    },
    /// `/provider ...` — add, remove, or switch LLM providers.
    Provider(ProviderAction),
    /// `/server ...` — status / start / stop the local llama-server.
    Server(ServerAction),
    /// `/login <provider>`: OAuth sign-in for providers that support it
    /// (currently `xai`).
    Login(String),
    /// `/settings` — open the in-app settings menu (a reusable picker).
    Settings,
    /// Import the selected artifacts from Claude Code (`~/.claude/`). Not a
    /// typed command; dispatched from the `/settings` import picker, which is
    /// why it carries the [`ImportSelection`].
    ImportClaude(ImportSelection),
    Quit,
}

/// What a `/provider` subcommand does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderAction {
    /// `/provider` / `/provider list` — show configured providers.
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
        None | Some("list") => ProviderAction::List,
        Some("use") => match args.get(1) {
            Some(name) => ProviderAction::Use((*name).to_string()),
            None => return Err("usage: /provider use <name>".to_string()),
        },
        Some("add") => {
            if args.len() < 5 {
                return Err(
                    "usage: /provider add <name> <llamacpp|ollama|openai|anthropic|openrouter|xai|xaioauth> <base_url> <model> [API_KEY_ENV]"
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
                other => {
                    return Err(format!(
                        "unknown provider kind '{other}' (llamacpp|ollama|openai|anthropic|openrouter|xai|xaioauth)"
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
            "rewind" => match args.first() {
                None => Ok(Self::Rewind(None)),
                Some(arg) => arg
                    .parse::<u64>()
                    .map(|turn| Self::Rewind(Some(turn)))
                    .map_err(|_| "usage: /rewind [turn]".to_string()),
            },
            "agents" => Ok(Self::Agents),
            "subagents" => Ok(Self::Subagents),
            "diff" => Ok(Self::Diff),
            "todos" => Ok(Self::Todos),
            "dashboard" => Ok(Self::Dashboard),
            "cost" => Ok(Self::Cost),
            "memory" => Ok(Self::Memory),
            "doctor" => Ok(Self::Doctor),
            "status" => Ok(Self::Status),
            "publish" => Ok(Self::Publish {
                branch: args.first().map(|s| s.to_string()),
            }),
            "provider" => parse_provider(&args),
            "server" => parse_server(&args),
            "login" => match args.first() {
                Some(provider) => Ok(Self::Login((*provider).to_string())),
                None => Err("usage: /login xai".to_string()),
            },
            "settings" => Ok(Self::Settings),
            "quit" | "q" | "exit" => Ok(Self::Quit),
            other => Err(format!("unknown command '/{other}' — try /help")),
        };
        Some(parsed)
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
        name: "plan",
        args: "",
        description: "toggle plan mode: read-only until a plan is approved",
        takes_args: false,
    },
    CommandSpec {
        name: "rewind",
        args: "[turn]",
        description: "rewind files and conversation to before a turn",
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
        args: "[list|use|add|remove]",
        description: "add, remove, or switch LLM providers",
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
        name: "settings",
        args: "",
        description: "open the settings menu (change config anytime)",
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
    /// A turn to rewind to (item values are turn ids).
    Rewind,
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

/// Status bar contents.
#[derive(Debug, Default)]
pub struct StatusLine {
    pub model: String,
    pub mode: Mode,
    /// Current step within the running turn (0 when idle).
    pub step: u32,
    pub max_steps: u32,
    /// True while a turn is streaming.
    pub busy: bool,
    /// Session prompt-token total (from [`AgentEvent::Usage`]).
    pub prompt_tokens: u64,
    /// Session completion-token total.
    pub completion_tokens: u64,
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
    pub transcript: Vec<TranscriptEntry>,
    /// Partial assistant text of the in-flight turn (moved into the
    /// transcript when the turn ends).
    pub streaming: String,
    /// Partial model reasoning of the in-flight turn, rendered dimmed and
    /// flushed to the transcript alongside `streaming`.
    pub streaming_thinking: String,
    pub status: StatusLine,
    /// Git diff sidebar visibility and cached contents.
    pub show_diff: bool,
    pub diff_text: String,
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
    /// In-session subagent monitor visibility (toggled by `/subagents`).
    pub show_subagents: bool,
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
    /// Transcript scroll offset from the bottom (0 = pinned to latest).
    pub scroll: u16,
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
    /// Whether plan mode is active (mirrors the agent's flag for the status
    /// bar; toggled by `/plan` and Shift+Tab).
    pub plan_mode: bool,
    /// Open plan-review modal (the turn is paused inside `exit_plan` until
    /// it resolves), if any.
    pub plan_review: Option<PlanReview>,
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
}

impl App {
    pub fn new(config: Config) -> Self {
        let mode = config.mode;
        let plan_mode = config.plan_first;
        let spinner_verb = config.ui.spinner_verb(0).to_string();
        let status = StatusLine {
            model: config.active().model,
            mode,
            step: 0,
            max_steps: config.max_steps,
            busy: false,
            prompt_tokens: 0,
            completion_tokens: 0,
        };
        Self {
            config,
            mode,
            input: String::new(),
            cursor: 0,
            input_mode: InputMode::default(),
            transcript: Vec::new(),
            streaming: String::new(),
            streaming_thinking: String::new(),
            status,
            show_diff: false,
            diff_text: String::new(),
            show_todos: false,
            todos: Vec::new(),
            todos_seen: false,
            show_dashboard: false,
            show_subagents: false,
            session_id: String::new(),
            session_name: String::new(),
            session_started_unix: 0,
            sessions: Vec::new(),
            ctrl_c_armed: false,
            dashboard_selected: 0,
            dashboard_input: String::new(),
            peek_lines: Vec::new(),
            scroll: 0,
            should_quit: false,
            tick: 0,
            suggestions: Vec::new(),
            suggestion_index: 0,
            custom_commands: Vec::new(),
            project_root: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            picker: None,
            plan_mode,
            plan_review: None,
            history: Vec::new(),
            history_pos: None,
            history_draft: String::new(),
            turn_started: None,
            rebuilding: None,
            spinner_verb,
            verb_rolls: 0,
            pending_edit_config: false,
        }
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
            ("import", "Import from Claude Code".to_string(), import_detail),
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
            "plan_first" => self.config.plan_first = !self.config.plan_first,
            "continuous" => self.config.continuous = !self.config.continuous,
            "plan_each_cycle" => self.config.plan_each_cycle = !self.config.plan_each_cycle,
            "rollback" => {
                self.config.rollback_failed_cycles = !self.config.rollback_failed_cycles;
            }
            "web_allow_local" => self.config.web.allow_local = !self.config.web.allow_local,
            "fleet_synthesize" => self.config.fleet.synthesize = !self.config.fleet.synthesize,
            "web_backend" => {
                self.config.web.search_backend = cycle_backend(&self.config.web.search_backend);
            }
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

    /// Current state for this session's heartbeat: needs-input when paused on a
    /// plan review, working while a turn streams, otherwise idle.
    fn session_state(&self) -> SessionState {
        if self.plan_review.is_some() {
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
        self.input
            .char_indices()
            .nth(self.cursor)
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

    fn insert_char(&mut self, c: char) {
        let index = self.byte_index();
        self.input.insert(index, c);
        self.cursor += 1;
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

    /// Dispatch one event from the merged stream. Returns the user action
    /// the main loop must perform (start a turn, run a slash command, ...).
    pub fn handle_event(&mut self, event: Event) -> Result<Option<AppAction>> {
        match event {
            Event::Key(key) => self.handle_key(key),
            Event::Mouse(mouse) => {
                match mouse.kind {
                    MouseEventKind::ScrollUp => {
                        self.scroll = self.scroll.saturating_add(3);
                    }
                    MouseEventKind::ScrollDown => {
                        self.scroll = self.scroll.saturating_sub(3);
                    }
                    _ => {}
                }
                Ok(None)
            }
            Event::Paste(text) => {
                self.insert_str(&text);
                self.sync_input_mode();
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
            // Owned by the main loop (it holds the agent slot); never
            // reaches here.
            Event::AgentRebuilt(_) => Ok(None),
        }
    }

    /// Keyboard handling for the current [`InputMode`]. Priority: global
    /// chords, open picker, then line editing.
    pub fn handle_key(&mut self, key: KeyEvent) -> Result<Option<AppAction>> {
        if key.kind == KeyEventKind::Release {
            return Ok(None);
        }

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

        // The subagent monitor is modal too: Esc / Enter / q close it; other
        // keys are swallowed. It refreshes from live App state each frame.
        if self.show_subagents {
            if matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q')) {
                self.show_subagents = false;
            }
            return Ok(None);
        }

        // An open plan review captures all keys: the turn is paused inside
        // exit_plan until a verdict is sent.
        if self.plan_review.is_some() {
            self.handle_plan_review_key(key);
            return Ok(None);
        }

        // An open picker captures navigation keys.
        if let Some(picker) = self.picker.as_mut() {
            match key.code {
                KeyCode::Up | KeyCode::BackTab => {
                    picker.selected = if picker.selected == 0 {
                        picker.items.len().saturating_sub(1)
                    } else {
                        picker.selected - 1
                    };
                }
                KeyCode::Down | KeyCode::Tab => {
                    picker.selected = if picker.selected + 1 >= picker.items.len() {
                        0
                    } else {
                        picker.selected + 1
                    };
                }
                // Space toggles a checkbox row in the Claude-import multi-select.
                KeyCode::Char(' ') if picker.kind == PickerKind::ClaudeImport => {
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
                        PickerKind::Rewind => {
                            // Item values are always turn ids we formatted.
                            let Ok(turn) = item.value.parse::<u64>() else {
                                return Ok(None);
                            };
                            AppAction::Command(SlashCommand::Rewind(Some(turn)))
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
                            let flags: Vec<bool> =
                                picker.items.iter().map(|i| i.current).collect();
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
                    };
                    return Ok(Some(action));
                }
                _ => {}
            }
            return Ok(None);
        }

        let suggesting = !self.suggestions.is_empty();
        let action = match key.code {
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
            // Shift+Tab toggles plan mode (same as /plan).
            KeyCode::BackTab => Some(AppAction::Command(SlashCommand::Plan)),
            KeyCode::Esc => {
                if self.scroll > 0 {
                    self.scroll = 0;
                } else {
                    self.clear_input();
                }
                None
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_add(10);
                None
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_sub(10);
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
                } else {
                    self.history_next();
                }
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
    /// expansion) to the agent.
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
        let expanded =
            crate::commands::preprocess(&input, &self.custom_commands, &self.project_root);
        self.push_history(&input);
        self.clear_input();
        self.transcript.push(TranscriptEntry::User(input));
        self.scroll = 0;
        Some(AppAction::Submit(expanded))
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
            KeyCode::Up => review.scroll = review.scroll.saturating_sub(1),
            KeyCode::Down => review.scroll = review.scroll.saturating_add(1),
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
                        // Long, successful outputs start collapsed; errors
                        // stay expanded so they are visible.
                        *collapsed = !output.is_error && output.content.lines().count() > 6;
                        *slot = Some(output.content);
                    }
                    None => {
                        // No matching running card (e.g. denied before start
                        // was emitted) — record the result standalone.
                        self.transcript.push(TranscriptEntry::ToolCard {
                            name,
                            args: Value::Null,
                            output: Some(output.content),
                            is_error: output.is_error,
                            collapsed: false,
                        });
                    }
                }
            }
            AgentEvent::StepCompleted { step } => {
                self.status.step = step;
            }
            AgentEvent::Error(message) => {
                self.flush_streaming();
                self.notice(format!("error: {message}"));
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
            AgentEvent::Usage {
                prompt_tokens,
                completion_tokens,
            } => {
                self.status.prompt_tokens += prompt_tokens;
                self.status.completion_tokens += completion_tokens;
            }
            // TaskStarted is mirrored to the gateway's JSON stream (see
            // output.rs); the TUI surfaces only the finish notice.
            AgentEvent::TaskStarted { .. } => {}
            AgentEvent::TaskFinished {
                id,
                command,
                status,
            } => {
                self.notice(format!(
                    "background task #{id} finished ({}): {command}",
                    status.describe()
                ));
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
            AgentEvent::Done { reason } => {
                self.flush_streaming();
                self.status.busy = false;
                self.turn_started = None;
                match reason {
                    DoneReason::Completed => {}
                    DoneReason::MaxSteps => self.notice(format!(
                        "step budget reached ({} steps) — send another message to continue",
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
    /// Start an agent turn with this user message.
    Submit(String),
    /// Execute a parsed slash command.
    Command(SlashCommand),
    /// Interrupt the running turn (Ctrl-C): abort the turn task and rebuild
    /// the agent from the last session.
    Interrupt,
}

// ---------------------------------------------------------------------------
// Terminal lifecycle
// ---------------------------------------------------------------------------

type Tui = Terminal<CrosstermBackend<std::io::Stdout>>;

fn setup_terminal() -> Result<Tui> {
    crossterm::terminal::enable_raw_mode().context("enabling raw mode")?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(
        stdout,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableMouseCapture,
        crossterm::event::EnableBracketedPaste,
    )
    .context("entering alternate screen")?;
    Terminal::new(CrosstermBackend::new(stdout)).context("creating terminal")
}

/// Suspend the TUI, open `$VISUAL`/`$EDITOR` on `~/.wizard/config.toml`, then
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
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_default();
    if editor.trim().is_empty() {
        app.notice(format!(
            "no $EDITOR set — edit {} by hand, then /reload",
            path.display()
        ));
        return;
    }

    // Leave the alternate screen so the editor draws on the real terminal.
    if let Err(err) = restore_terminal() {
        app.notice(format!("could not suspend the TUI: {err:#}"));
        return;
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
        }
        Err(err) => {
            app.notice(format!("could not restore the TUI: {err:#} — /quit and relaunch"));
            return;
        }
    }

    match status {
        Ok(status) if status.success() => match Config::load() {
            Ok(config) => {
                app.config = config;
                app.mode = app.config.mode;
                app.status.mode = app.config.mode;
                app.status.model = app.config.active().model;
                app.notice(
                    "config reloaded — restart for provider/model changes to take effect",
                );
            }
            Err(err) => app.notice(format!("config not reloaded (parse error): {err:#}")),
        },
        Ok(_) => app.notice("editor exited without success — config not reloaded"),
        Err(err) => app.notice(format!("could not launch editor: {err:#}")),
    }
}

fn restore_terminal() -> Result<()> {
    crossterm::execute!(
        std::io::stdout(),
        crossterm::event::DisableBracketedPaste,
        crossterm::event::DisableMouseCapture,
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
async fn build_registry(
    manager: &McpManager,
    client: &Arc<dyn LlmProvider>,
    hooks: &Arc<HookEngine>,
) -> Result<ToolRegistry> {
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

    let subagents_dir = Config::subagents_dir()?;
    let subagent_configs = subagent::available_configs(&subagents_dir);
    let base = Arc::new(base);
    let mut registry = subagent::scoped_registry(&base, None);
    registry.register(Arc::new(subagent::SpawnSubagentTool::new(
        subagent_configs,
        Arc::clone(client),
        Arc::clone(&base),
        Arc::clone(hooks),
    )));
    Ok(registry)
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

/// Build a fully wired [`Agent`]. `resume` reopens the latest session file
/// instead of starting a new one.
async fn build_agent(
    client: &Arc<dyn LlmProvider>,
    config: &Config,
    skills: &[Skill],
    project_root: &Path,
    manager: &McpManager,
    resume: bool,
) -> Result<Agent> {
    // Session first: the hook engine carries its id in every payload.
    let sessions_dir = Config::sessions_dir()?;
    let session = if resume {
        match Session::open_latest(&sessions_dir)? {
            Some(session) => session,
            None => Session::create(&sessions_dir)?,
        }
    } else {
        Session::create(&sessions_dir)?
    };
    let hooks = Arc::new(HookEngine::new(
        crate::hooks::load(project_root),
        project_root.to_path_buf(),
        session.id.clone(),
    ));
    let mut registry = build_registry(manager, client, &hooks).await?;
    attach_config_tools(&mut registry, config);
    let model = config.active().model;
    let native_tools = match client.supports_native_tools(&model).await {
        Ok(supported) => supported,
        Err(err) => {
            tracing::warn!("probing tool support for {model}: {err:#}");
            false
        }
    };
    Agent::new(
        Arc::clone(client),
        registry,
        config.clone(),
        skills.to_vec(),
        project_root.to_path_buf(),
        session,
        native_tools,
        hooks,
    )
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

/// Compose the `/diff` sidebar contents: unstaged then staged changes.
async fn git_diff_text(root: &Path) -> Result<String> {
    let unstaged = git_output(root, &["diff"]).await?;
    let staged = git_output(root, &["diff", "--staged"]).await?;
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
    if text.is_empty() {
        text = "(working tree clean)".to_string();
    }
    Ok(text)
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
/plan                       toggle plan mode (read-only until a plan is approved)\n  \
/rewind [turn]              rewind files and conversation to before a turn\n  \
/agents                     browse subagents and delegate to one\n  \
/subagents                  monitor the subagents running in this session\n  \
/evolve [--deep] <desc>     self-extension (skill / MCP / scripted tool)\n  \
/publish [branch]           fork Wizard to your GitHub, get a one-line installer\n  \
/provider [list|use|...]    add, remove, or switch LLM providers (llamacpp/ollama/openai/anthropic/openrouter/xai/xaioauth)\n  \
/server [status|start|stop] manage the local llama-server\n  \
/login xai                  sign in with your xAI account (OAuth, no API key)\n  \
/reload                     reload skills, scripted tools, and MCP servers\n  \
/diff                       toggle the git diff sidebar\n  \
/todos                      toggle the todo side panel\n  \
/dashboard                  session manager: all live wizard sessions on this machine\n  \
/cost                       show session token usage and cost\n  \
/memory                     show saved project memories\n  \
/status                     show session status (model, usage, todos, tasks)\n  \
/settings                   open the settings menu (change config anytime)\n  \
/doctor                     diagnose config, providers, MCP, hooks, state dirs\n  \
/quit                       exit\n\
keys:\n  \
Tab / →                     accept command completion\n  \
Shift+Tab                   toggle plan mode\n  \
↑ / ↓                       select suggestion · browse input history\n  \
PgUp/PgDn · mouse wheel     scroll the transcript\n  \
Ctrl-P                      model picker  ·  Ctrl-T toggle last tool card\n  \
Ctrl-A/E Home/End ←/→       move cursor   ·  Ctrl-W/U/K kill word/to start/to end\n  \
Ctrl-C                      quit";

// ---------------------------------------------------------------------------
// Genie-mode entry point
// ---------------------------------------------------------------------------

/// True for backends that run on this machine (no API key, no cloud).
fn is_local_kind(kind: ProviderKind) -> bool {
    matches!(kind, ProviderKind::LlamaCpp | ProviderKind::Ollama)
}

/// Rotate the `web_search` backend for the settings menu's cycle row:
/// duckduckgo → brave → tavily → duckduckgo.
fn cycle_backend(current: &str) -> String {
    match current {
        "duckduckgo" => "brave",
        "brave" => "tavily",
        _ => "duckduckgo",
    }
    .to_string()
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
        let outcome = server::ensure_running(provider, &|line: &str| wait.update(line)).await;
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
        "grok-4.3",
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
    let local_err = match try_provider(&active).await {
        Ok(client) => return Ok(client),
        Err(err) if is_local_kind(active.kind) => err,
        Err(err) => return Err(err),
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

    let project_root = std::env::current_dir().context("resolving project root")?;
    let mut skills = load_skill_roots();

    let mcp_path = Config::mcp_config_path()?;
    let mcp_config = match McpConfig::load(&mcp_path) {
        Ok(config) => config,
        Err(err) => {
            tracing::warn!("loading {}: {err:#}", mcp_path.display());
            McpConfig::default()
        }
    };
    // Shared with background rebuild tasks (model switch, crash recovery).
    let manager = Arc::new(Mutex::new(
        match McpManager::connect_all(&mcp_config).await {
            Ok(manager) => manager,
            Err(err) => {
                tracing::warn!("connecting MCP servers: {err:#}");
                McpManager::empty()
            }
        },
    ));

    let mut agent_slot: Option<Agent> = Some(
        build_agent(
            &client,
            &config,
            &skills,
            &project_root,
            &*manager.lock().await,
            cli.resume,
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

    let mut events = EventLoop::new(Duration::from_millis(100));
    let mut terminal = setup_terminal()?;
    let _guard = TerminalGuard;

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
                agent_slot = Some(agent);
            }
            app.notice(rebuild.notice);
            continue;
        }

        let turn_done = matches!(&event, Event::Agent(AgentEvent::Done { .. }));

        let action = app.handle_event(event)?;
        if let Some(action) = action {
            match action {
                AppAction::Submit(input) => match agent_slot.take() {
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

                        agent_task = Some(tokio::spawn(async move {
                            let fallback = agent_tx.clone();
                            if let Err(err) = agent.run_turn(&input, agent_tx).await {
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
                        tokio::spawn(async move {
                            let manager = manager.lock().await;
                            let rebuild = match build_agent(
                                &client,
                                &config,
                                &skills,
                                &project_root,
                                &manager,
                                true,
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
            }
        }

        // The `/settings` "Open config file" row asks the main loop (the
        // terminal owner) to suspend the TUI and run an external editor.
        if app.pending_edit_config {
            app.pending_edit_config = false;
            edit_config_file(&mut app, &mut terminal);
        }

        if turn_done && let Some(handle) = agent_task.take() {
            match handle.await {
                Ok(agent) => agent_slot = Some(agent),
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
                    tokio::spawn(async move {
                        let manager = manager.lock().await;
                        let rebuild = match build_agent(
                            &client,
                            &config,
                            &skills,
                            &project_root,
                            &manager,
                            true,
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
    genie_max_steps: u32,
    events: &'a EventLoop,
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
            SlashCommand::Clear => self.clear(),
            SlashCommand::Model(None) => self.open_model_picker().await,
            SlashCommand::Model(Some(tag)) => self.switch_model(tag),
            SlashCommand::Mode(None) => self.open_mode_picker(),
            SlashCommand::Mode(Some(mode)) => self.switch_mode(mode),
            SlashCommand::Plan => self.toggle_plan(),
            SlashCommand::Rewind(None) => self.open_rewind_picker(),
            SlashCommand::Rewind(Some(turn)) => self.rewind(turn),
            SlashCommand::Agents => self.open_agents_picker(),
            SlashCommand::Reload => self.reload().await,
            SlashCommand::Evolve { deep, description } => self.evolve(deep, description),
            SlashCommand::Publish { branch } => self.publish(branch),
            SlashCommand::Provider(action) => self.provider(action).await,
            SlashCommand::Server(action) => self.server(action).await,
            SlashCommand::Login(provider) => self.login(provider),
            SlashCommand::Settings => self.app.open_settings_picker(),
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
            self.app.show_subagents = false;
            self.app.refresh_sessions();
            self.app.refresh_peek();
        }
    }

    /// `/subagents`: toggle the in-session subagent monitor.
    fn toggle_subagents(&mut self) {
        self.app.show_subagents = !self.app.show_subagents;
        // Mutually exclusive with the dashboard so only one modal is up.
        if self.app.show_subagents {
            self.app.show_dashboard = false;
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
        let mut text = format!(
            "model: {}\nprovider: {} ({:?} @ {})\nmode: {}",
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
            if self.app.plan_mode { "on" } else { "off" }
        ));
        self.app.notice(text);
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
        self.app.scroll = 0;
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
                    detail: format!(
                        "{} · {scope} · {} steps",
                        config.description, config.max_steps
                    ),
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
        self.app.notice(if on {
            "plan mode on — the agent investigates read-only and presents a plan via \
             exit_plan for approval (/plan or Shift+Tab to leave)"
        } else {
            "plan mode off"
        });
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
                self.app.scroll = 0;
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
                self.app.config.max_steps = self
                    .app
                    .config
                    .max_steps
                    .max(Mode::Sovereign.default_max_steps());
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
            Ok(registry) => {
                let tool_count = registry.len();
                if let Some(agent) = self.agent_slot.as_mut() {
                    agent.set_registry(registry);
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
                self.app.notice(format!("Claude Code import failed: {err:#}"));
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
            && let Ok(registry) = build_registry(&manager, self.client, &hooks).await
            && let Some(agent) = self.agent_slot.as_mut()
        {
            agent.set_registry(registry);
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
        let manager = self.manager.lock().await;
        match build_agent(
            self.client,
            &self.app.config,
            self.skills,
            self.project_root,
            &manager,
            false,
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
                self.app.status.model = self.app.config.active().model;
                self.app.notice(summary);
            }
            Err(err) => {
                *self.agent_slot = None;
                self.app.notice(format!(
                    "switched provider but could not start the agent: {err:#} — /quit and relaunch"
                ));
            }
        }
    }

    /// Handle `/provider` subcommands: list, switch, add, or remove providers.
    async fn provider(&mut self, action: ProviderAction) {
        match action {
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
                 add one with: /provider add <name> <llamacpp|ollama|openai|anthropic|openrouter|xai|xaioauth> <base_url> <model> [API_KEY_ENV]",
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
        // Dedup by name: replace an existing entry with the same name.
        self.app.config.providers.retain(|p| p.name != name);
        self.app.config.providers.push(provider);
        self.app.config.active_provider = Some(name.clone());
        self.persist_config();
        let reminder = api_key_env
            .map(|env| format!(" — remember to `export {env}=<key>` for this provider"))
            .unwrap_or_default();
        self.rebuild_active_provider(format!("added and switched to provider '{name}'{reminder}"))
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
            let message = match crate::llm::xai_oauth::login(progress).await {
                Ok(()) => "signed in to xAI; add the provider with \
                           /provider add xai xaioauth https://api.x.ai/v1 grok-4.3"
                    .to_string(),
                Err(err) => format!("xAI sign-in failed: {err:#}"),
            };
            let _ = notify.send(Event::Notice(message)).await;
        });
    }

    /// Background half of `/server start` (and the post-switch auto-start):
    /// ensure a llama-server is running for `provider`, streaming progress
    /// into the transcript as notices.
    fn start_server_task(&self, provider: ProviderConfig) {
        let notify = self.events.sender();
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
            let message = match server::ensure_running(&provider, &progress).await {
                Ok(()) => format!("llama-server at {} is ready", provider.base_url),
                Err(err) => format!("llama-server: {err:#}"),
            };
            let _ = notify.send(Event::Notice(message)).await;
        });
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
            match build_agent(client, &config, &skills, &project_root, &manager, false).await {
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

    fn type_str(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, KeyCode::Char(c));
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
        let Some(AppAction::Submit(prompt)) = action else {
            panic!("expected a submit, got {action:?}");
        };
        assert_eq!(prompt, "Review src/app.rs with care.");
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
            Some(AppAction::Submit(prompt)) if prompt == "/frobnicate the build"
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
        let Some(AppAction::Submit(prompt)) = action else {
            panic!("expected a submit, got {action:?}");
        };
        assert!(prompt.contains("the context"), "got: {prompt}");
        // The transcript keeps the compact form.
        assert!(matches!(
            app.transcript.last(),
            Some(TranscriptEntry::User(text)) if text == "use @ctx.txt here"
        ));
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
    fn usage_events_accumulate_session_totals_in_the_status_bar() {
        let mut app = app();
        app.handle_agent_event(AgentEvent::Usage {
            prompt_tokens: 100,
            completion_tokens: 20,
        });
        app.handle_agent_event(AgentEvent::Usage {
            prompt_tokens: 50,
            completion_tokens: 5,
        });
        assert_eq!(app.status.prompt_tokens, 150);
        assert_eq!(app.status.completion_tokens, 25);
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
}
