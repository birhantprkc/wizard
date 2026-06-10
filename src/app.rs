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
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::agent::{Agent, AgentEvent, DoneReason, session::Session, subagent};
use crate::cli::Cli;
use crate::config::{Config, Mode, ProviderConfig, ProviderKind};
use crate::event::{Event, EventLoop};
use crate::evolve::{EvolveOutcome, EvolveRequest, EvolveTier, Evolver, PublishRequest, publish};
use crate::llm::ToolCall;
use crate::llm::provider::LlmProvider;
use crate::mcp::{McpConfig, McpManager};
use crate::server;
use crate::skills::Skill;
use crate::tools::registry::ToolRegistry;

/// One rendered entry in the chat transcript.
#[derive(Debug)]
pub enum TranscriptEntry {
    User(String),
    Assistant(String),
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

/// A gated tool call waiting for the user's y/n.
#[derive(Debug)]
pub struct PendingApproval {
    pub call: ToolCall,
    /// Send `true` to approve, `false` to deny. Dropping denies.
    pub respond: oneshot::Sender<bool>,
}

/// What the input line is currently doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InputMode {
    /// Composing a chat message.
    #[default]
    Chat,
    /// Composing a `/slash` command.
    Command,
    /// Answering an approval prompt.
    Approval,
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
    /// Toggle the git diff sidebar.
    Diff,
    /// `/publish [branch]` — fork Wizard and get a one-line installer.
    Publish {
        branch: Option<String>,
    },
    /// `/provider ...` — add, remove, or switch LLM providers.
    Provider(ProviderAction),
    /// `/server ...` — status / start / stop the local llama-server.
    Server(ServerAction),
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
                    "usage: /provider add <name> <llamacpp|ollama|openai|anthropic> <base_url> <model> [API_KEY_ENV]"
                        .to_string(),
                );
            }
            let kind = match args[2] {
                "llamacpp" => ProviderKind::LlamaCpp,
                "ollama" => ProviderKind::Ollama,
                "openai" => ProviderKind::Openai,
                "anthropic" => ProviderKind::Anthropic,
                other => {
                    return Err(format!(
                        "unknown provider kind '{other}' (llamacpp|ollama|openai|anthropic)"
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
            "diff" => Ok(Self::Diff),
            "publish" => Ok(Self::Publish {
                branch: args.first().map(|s| s.to_string()),
            }),
            "provider" => parse_provider(&args),
            "server" => parse_server(&args),
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
        name: "diff",
        args: "",
        description: "toggle the git diff sidebar",
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

/// What an open [`Picker`] selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    Model,
    Mode,
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
    pub pending_approval: Option<PendingApproval>,
    pub status: StatusLine,
    /// Git diff sidebar visibility and cached contents.
    pub show_diff: bool,
    pub diff_text: String,
    /// Transcript scroll offset from the bottom (0 = pinned to latest).
    pub scroll: u16,
    pub should_quit: bool,
    /// Tick counter driving the busy spinner.
    pub tick: u64,
    /// Matching [`COMMANDS`] entries for the current `/input`, shown as the
    /// suggestion popup.
    pub suggestions: Vec<&'static CommandSpec>,
    /// Highlighted row in `suggestions`.
    pub suggestion_index: usize,
    /// Open selection popup (model / mode picker), if any.
    pub picker: Option<Picker>,
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
}

impl App {
    pub fn new(config: Config) -> Self {
        let mode = config.mode;
        let status = StatusLine {
            model: config.active().model,
            mode,
            step: 0,
            max_steps: config.max_steps,
            busy: false,
        };
        Self {
            config,
            mode,
            input: String::new(),
            cursor: 0,
            input_mode: InputMode::default(),
            transcript: Vec::new(),
            streaming: String::new(),
            pending_approval: None,
            status,
            show_diff: false,
            diff_text: String::new(),
            scroll: 0,
            should_quit: false,
            tick: 0,
            suggestions: Vec::new(),
            suggestion_index: 0,
            picker: None,
            history: Vec::new(),
            history_pos: None,
            history_draft: String::new(),
            turn_started: None,
            rebuilding: None,
        }
    }

    /// Append a system notice to the transcript.
    pub fn notice(&mut self, message: impl Into<String>) {
        self.transcript
            .push(TranscriptEntry::Notice(message.into()));
    }

    /// Recompute [`InputMode`] from the pending approval and the input text,
    /// then refresh the command suggestions.
    fn sync_input_mode(&mut self) {
        self.input_mode = if self.pending_approval.is_some() {
            InputMode::Approval
        } else if self.input.trim_start().starts_with('/') {
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
                .map(|spec| spec.name)
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
        // Rank: exact match, then prefix matches, then substring matches.
        self.suggestions
            .extend(COMMANDS.iter().filter(|spec| spec.name == token));
        self.suggestions.extend(
            COMMANDS
                .iter()
                .filter(|spec| spec.name != token && spec.name.starts_with(token)),
        );
        self.suggestions.extend(
            COMMANDS
                .iter()
                .filter(|spec| !spec.name.starts_with(token) && spec.name.contains(token)),
        );
        self.suggestion_index = previous
            .and_then(|name| self.suggestions.iter().position(|spec| spec.name == name))
            .unwrap_or(0);
    }

    /// Replace the input with the highlighted suggestion. Returns the
    /// completed spec, or `None` when nothing is highlighted.
    fn accept_suggestion(&mut self) -> Option<&'static CommandSpec> {
        let spec = *self.suggestions.get(self.suggestion_index)?;
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

    /// Move any in-flight streaming text into the transcript.
    fn flush_streaming(&mut self) {
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

    /// Answer the pending approval prompt, if any.
    fn respond_approval(&mut self, approve: bool) {
        if let Some(pending) = self.pending_approval.take() {
            let name = pending.call.function.name.clone();
            // The agent may have given up waiting; a closed channel is fine.
            let _ = pending.respond.send(approve);
            let verdict = if approve { "approved" } else { "denied" };
            self.notice(format!("{verdict} tool call '{name}'"));
        }
        self.sync_input_mode();
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
                if self.input_mode != InputMode::Approval {
                    self.insert_str(&text);
                    self.sync_input_mode();
                }
                Ok(None)
            }
            Event::Resize(_, _) => Ok(None),
            Event::Tick => {
                self.tick = self.tick.wrapping_add(1);
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
    /// chords, approval prompt, open picker, then line editing.
    pub fn handle_key(&mut self, key: KeyEvent) -> Result<Option<AppAction>> {
        if key.kind == KeyEventKind::Release {
            return Ok(None);
        }

        // Global chords, regardless of input mode.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') | KeyCode::Char('d') => {
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
                    // while a turn runs or an approval prompt is up.
                    if self.status.busy || self.pending_approval.is_some() {
                        return Ok(None);
                    }
                    return Ok(Some(AppAction::Command(SlashCommand::Model(None))));
                }
                _ => {}
            }
        }

        if self.input_mode == InputMode::Approval {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    self.respond_approval(true);
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.respond_approval(false);
                }
                _ => {}
            }
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
                }
                None
            }
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
            let spec = self.suggestions[self.suggestion_index.min(self.suggestions.len() - 1)];
            // An exactly-typed command always runs as typed; otherwise Enter
            // completes the highlighted suggestion first.
            let exact = COMMANDS.iter().any(|command| command.name == typed);
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
                self.push_history(&input);
                self.clear_input();
                self.notice(message);
                None
            }
            None => {
                if self.status.busy {
                    // Rejected input never ran; do not record it in history.
                    self.notice("the agent is busy — wait for the current turn to finish");
                    return None;
                }
                if self.rebuilding.is_some() {
                    self.notice("the agent is rebuilding — try again in a moment");
                    return None;
                }
                self.push_history(&input);
                self.clear_input();
                self.transcript.push(TranscriptEntry::User(input.clone()));
                self.scroll = 0;
                Some(AppAction::Submit(input))
            }
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
            AgentEvent::ApprovalRequest { call, respond } => {
                // The approval modal takes over; drop any open picker so it
                // cannot linger stale underneath (or after) the prompt.
                self.picker = None;
                self.pending_approval = Some(PendingApproval { call, respond });
                self.scroll = 0;
                self.sync_input_mode();
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
            AgentEvent::Done { reason } => {
                self.flush_streaming();
                self.status.busy = false;
                self.turn_started = None;
                // Dropping an unanswered approval denies it agent-side.
                self.pending_approval = None;
                self.sync_input_mode();
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
/// (without itself) so subagents cannot recurse.
async fn build_registry(
    manager: &McpManager,
    client: &Arc<dyn LlmProvider>,
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
    let mut registry = build_registry(manager, client).await?;
    attach_config_tools(&mut registry, config);
    let sessions_dir = Config::sessions_dir()?;
    let session = if resume {
        match Session::open_latest(&sessions_dir)? {
            Some(session) => session,
            None => Session::create(&sessions_dir)?,
        }
    } else {
        Session::create(&sessions_dir)?
    };
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
/evolve [--deep] <desc>     self-extension (skill / MCP / scripted tool)\n  \
/publish [branch]           fork Wizard to your GitHub, get a one-line installer\n  \
/provider [list|use|...]    add, remove, or switch LLM providers (llamacpp/ollama/openai/anthropic)\n  \
/server [status|start|stop] manage the local llama-server\n  \
/reload                     reload skills, scripted tools, and MCP servers\n  \
/diff                       toggle the git diff sidebar\n  \
/quit                       exit\n\
keys:\n  \
Tab / →                     accept command completion\n  \
↑ / ↓                       select suggestion · browse input history\n  \
PgUp/PgDn · mouse wheel     scroll the transcript\n  \
Ctrl-P                      model picker  ·  Ctrl-T toggle last tool card\n  \
Ctrl-A/E Home/End ←/→       move cursor   ·  Ctrl-W/U/K kill word/to start/to end\n  \
y / n                       approve / deny a gated tool call\n  \
Ctrl-C                      quit";

// ---------------------------------------------------------------------------
// Genie-mode entry point
// ---------------------------------------------------------------------------

/// Genie-mode entry point: set up the terminal (raw mode + alternate
/// screen), build the agent stack (Ollama client, registry with scripted +
/// MCP tools, skills, session), pre-fill `cli.prompt` if given, and drive
/// the [`EventLoop`](crate::event::EventLoop) until quit. Restores the
/// terminal on exit and on panic.
pub async fn run_tui(config: Config, cli: Cli) -> Result<()> {
    // No usable terminal: run headless when a task was given, otherwise we
    // cannot do anything sensible.
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        if cli.prompt.is_some() {
            return crate::agent::run_headless(config, cli).await;
        }
        anyhow::bail!("wizard needs a terminal for the TUI; pass -p \"task\" to run headless");
    }

    let active = config.active();
    let mut client: Arc<dyn LlmProvider> = active
        .build()
        .with_context(|| format!("building provider '{}'", active.name))?;
    // llama.cpp gets a lifecycle hand: when nothing answers, Wizard starts
    // the server itself. The terminal is still in normal mode here, so the
    // spawn/load progress prints straight to stdout.
    if active.kind == ProviderKind::LlamaCpp {
        server::ensure_running(&active, &|line: &str| println!("{line}")).await?;
    }
    client
        .health()
        .await
        .with_context(|| format!("LLM health check failed for {}", client.label()))?;

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
    let mut agent_task: Option<JoinHandle<Agent>> = None;

    // Genie-mode max_steps as configured, used when switching back from
    // sovereign in-session.
    let genie_max_steps = config.max_steps;

    let mut app = App::new(config);
    if let Some(prompt) = cli.prompt.clone() {
        app.set_input(prompt);
    }
    // No startup notice: the welcome screen already shows the model, mode,
    // and help pointers until the first message arrives.

    let mut events = EventLoop::new(Duration::from_millis(100));
    let mut terminal = setup_terminal()?;
    let _guard = TerminalGuard;

    loop {
        terminal.draw(|frame| crate::ui::draw(frame, &app))?;

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
            if let Some(agent) = rebuild.agent {
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
                        app.turn_started = Some(Instant::now());

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
            }
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

    drop(_guard);
    restore_terminal_best_effort();
    Ok(())
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
            SlashCommand::Clear => self.clear(),
            SlashCommand::Model(None) => self.open_model_picker().await,
            SlashCommand::Model(Some(tag)) => self.switch_model(tag),
            SlashCommand::Mode(None) => self.open_mode_picker(),
            SlashCommand::Mode(Some(mode)) => self.switch_mode(mode),
            SlashCommand::Reload => self.reload().await,
            SlashCommand::Evolve { deep, description } => self.evolve(deep, description),
            SlashCommand::Publish { branch } => self.publish(branch),
            SlashCommand::Provider(action) => self.provider(action).await,
            SlashCommand::Server(action) => self.server(action).await,
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
                self.app.config.auto_approve = true;
                self.app.config.max_steps = self
                    .app
                    .config
                    .max_steps
                    .max(Mode::Sovereign.default_max_steps());
            }
            Mode::Genie => {
                self.app.config.auto_approve = true;
                self.app.config.max_steps = self.genie_max_steps;
            }
        }
        self.app.status.max_steps = self.app.config.max_steps;
        self.app.notice(format!("switched to {mode} mode"));
    }

    async fn reload(&mut self) {
        if self.agent_unavailable("reload") {
            return;
        }
        *self.skills = load_skill_roots();
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
        match build_registry(&manager, self.client).await {
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
        // The TUI cannot use the Evolver's stdin y/N gate (the terminal
        // is in raw mode and owned by the event loop). The explicit
        // `/evolve` command is treated as the user's approval; the
        // outcome notice reports exactly what was added.
        let request = EvolveRequest {
            description,
            tier,
            auto_approve: true,
        };
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
            let req = PublishRequest {
                branch,
                auto_approve: true,
            };
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
            Ok(agent) => {
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
                 add one with: /provider add <name> <llamacpp|ollama|openai|anthropic> <base_url> <model> [API_KEY_ENV]",
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

    fn type_str(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, KeyCode::Char(c));
        }
    }

    #[test]
    fn slash_filters_suggestions_by_prefix() {
        let mut app = app();
        type_str(&mut app, "/mo");
        let names: Vec<&str> = app.suggestions.iter().map(|s| s.name).collect();
        assert_eq!(names, ["model", "mode"]);
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
        assert_eq!(app.suggestion_index, 0);
        press(&mut app, KeyCode::Up);
        assert_eq!(app.suggestion_index, 1);
    }

    #[test]
    fn tab_completes_the_selected_suggestion() {
        let mut app = app();
        type_str(&mut app, "/re");
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
