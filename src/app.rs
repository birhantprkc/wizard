//! TUI state machine: application state, slash commands, and the genie-mode
//! main loop. Rendering lives in [`crate::ui`]; raw events in
//! [`crate::event`].

use std::io::IsTerminal;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::agent::{Agent, AgentEvent, DoneReason, session::Session, subagent};
use crate::cli::Cli;
use crate::config::{Config, Mode};
use crate::event::{Event, EventLoop};
use crate::evolve::{EvolveOutcome, EvolveRequest, EvolveTier, Evolver};
use crate::llm::ToolCall;
use crate::llm::ollama::OllamaClient;
use crate::mcp::{McpConfig, McpManager};
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
    Quit,
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
}

impl App {
    pub fn new(config: Config) -> Self {
        let mode = config.mode;
        let status = StatusLine {
            model: config.model.clone(),
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
async fn build_registry(manager: &McpManager, client: &OllamaClient) -> Result<ToolRegistry> {
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
        client.clone(),
        Arc::clone(&base),
    )));
    Ok(registry)
}

/// Build a fully wired [`Agent`]. `resume` reopens the latest session file
/// instead of starting a new one.
async fn build_agent(
    client: &OllamaClient,
    config: &Config,
    skills: &[Skill],
    project_root: &Path,
    manager: &McpManager,
    resume: bool,
) -> Result<Agent> {
    let registry = build_registry(manager, client).await?;
    let sessions_dir = Config::sessions_dir()?;
    let session = if resume {
        match Session::open_latest(&sessions_dir)? {
            Some(session) => session,
            None => Session::create(&sessions_dir)?,
        }
    } else {
        Session::create(&sessions_dir)?
    };
    let native_tools = match client.supports_native_tools(&config.model).await {
        Ok(supported) => supported,
        Err(err) => {
            tracing::warn!("probing tool support for {}: {err:#}", config.model);
            false
        }
    };
    Agent::new(
        client.clone(),
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

    let client = OllamaClient::new(&config.ollama_host);
    client.health().await.with_context(|| {
        format!(
            "cannot reach Ollama at {} — is `ollama serve` running?",
            config.ollama_host
        )
    })?;

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
    let mut manager = match McpManager::connect_all(&mcp_config).await {
        Ok(manager) => manager,
        Err(err) => {
            tracing::warn!("connecting MCP servers: {err:#}");
            McpManager::empty()
        }
    };

    let mut agent_slot: Option<Agent> = Some(
        build_agent(
            &client,
            &config,
            &skills,
            &project_root,
            &manager,
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
                    handle_command(
                        command,
                        &mut app,
                        &client,
                        &mut agent_slot,
                        &mut manager,
                        &mut skills,
                        &project_root,
                        &mcp_path,
                        genie_max_steps,
                        &events,
                    )
                    .await;
                }
            }
        }

        if turn_done && let Some(handle) = agent_task.take() {
            match handle.await {
                Ok(agent) => agent_slot = Some(agent),
                Err(err) => {
                    app.notice(format!("agent task crashed: {err}"));
                    match build_agent(&client, &app.config, &skills, &project_root, &manager, true)
                        .await
                    {
                        Ok(agent) => {
                            agent_slot = Some(agent);
                            app.notice("agent restarted from the last session");
                        }
                        Err(err) => app.notice(format!(
                            "could not restart the agent: {err:#} — /quit and relaunch"
                        )),
                    }
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

/// Execute one slash command against the running stack.
//
// This is the single dispatch point that wires every live subsystem of the
// TUI together; bundling the borrows into a context struct would only move
// the argument list without simplifying any call site (there is exactly one).
#[allow(clippy::too_many_arguments)]
async fn handle_command(
    command: SlashCommand,
    app: &mut App,
    client: &OllamaClient,
    agent_slot: &mut Option<Agent>,
    manager: &mut McpManager,
    skills: &mut Vec<Skill>,
    project_root: &Path,
    mcp_path: &Path,
    genie_max_steps: u32,
    events: &EventLoop,
) {
    match command {
        SlashCommand::Help => {
            app.notice(HELP_TEXT);
        }

        SlashCommand::Quit => {
            app.should_quit = true;
        }

        SlashCommand::Diff => {
            app.show_diff = !app.show_diff;
            if app.show_diff {
                app.diff_text = match git_diff_text(project_root).await {
                    Ok(text) => text,
                    Err(err) => format!("could not read git diff: {err:#}"),
                };
            }
        }

        SlashCommand::Clear => {
            if app.status.busy {
                app.notice("cannot clear while a turn is running");
                return;
            }
            if let Some(agent) = agent_slot.as_mut()
                && let Err(err) = agent.clear()
            {
                app.notice(format!("failed to rotate session: {err:#}"));
                return;
            }
            app.transcript.clear();
            app.streaming.clear();
            app.scroll = 0;
            app.notice("conversation cleared");
        }

        SlashCommand::Model(None) => {
            if app.status.busy {
                app.notice("cannot switch models while a turn is running");
                return;
            }
            // Open the interactive model picker with all installed models.
            match client.list_models().await {
                Ok(models) if !models.is_empty() => {
                    let current = app.config.model.clone();
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
                    app.picker = Some(Picker {
                        kind: PickerKind::Model,
                        title: " select model ".to_string(),
                        items,
                        selected,
                    });
                }
                Ok(_) => app.notice("no models installed — try `ollama pull <model>`"),
                Err(err) => app.notice(format!("could not list models: {err:#}")),
            }
        }

        SlashCommand::Model(Some(tag)) => {
            if app.status.busy {
                app.notice("cannot switch models while a turn is running");
                return;
            }
            if let Ok(models) = client.list_models().await {
                let known = models
                    .iter()
                    .any(|m| *m == tag || m.split(':').next() == Some(tag.as_str()));
                if !known {
                    app.notice(format!(
                        "model '{tag}' is not installed (try `ollama pull {tag}`)"
                    ));
                    return;
                }
            }
            app.config.model = tag.clone();
            app.status.model = tag.clone();
            let native_tools = match client.supports_native_tools(&tag).await {
                Ok(supported) => supported,
                Err(err) => {
                    tracing::warn!("probing tool support for {tag}: {err:#}");
                    false
                }
            };
            match agent_slot.as_mut() {
                Some(agent) => {
                    agent.set_model(tag.clone(), native_tools);
                    app.notice(format!("switched to model {tag} (context preserved)"));
                }
                None => {
                    match build_agent(client, &app.config, skills, project_root, manager, false)
                        .await
                    {
                        Ok(agent) => {
                            *agent_slot = Some(agent);
                            app.notice(format!("switched to model {tag}"));
                        }
                        Err(err) => app.notice(format!("failed to switch model: {err:#}")),
                    }
                }
            }
        }

        SlashCommand::Mode(None) => {
            if app.status.busy {
                app.notice("cannot switch modes while a turn is running");
                return;
            }
            // Open the interactive mode picker.
            let items = vec![
                PickerItem {
                    value: "genie".to_string(),
                    detail: "interactive — confirms risky actions".to_string(),
                    current: app.mode == Mode::Genie,
                },
                PickerItem {
                    value: "sovereign".to_string(),
                    detail: "autonomous — auto-approves all tool calls".to_string(),
                    current: app.mode == Mode::Sovereign,
                },
            ];
            let selected = items.iter().position(|item| item.current).unwrap_or(0);
            app.picker = Some(Picker {
                kind: PickerKind::Mode,
                title: " select mode ".to_string(),
                items,
                selected,
            });
        }

        SlashCommand::Mode(Some(mode)) => {
            if app.status.busy {
                app.notice("cannot switch modes while a turn is running");
                return;
            }
            if let Some(agent) = agent_slot.as_mut() {
                agent.set_mode(mode);
            }
            app.mode = mode;
            app.config.mode = mode;
            app.status.mode = mode;
            match mode {
                Mode::Sovereign => {
                    app.config.auto_approve = true;
                    app.config.max_steps = app
                        .config
                        .max_steps
                        .max(Mode::Sovereign.default_max_steps());
                }
                Mode::Genie => {
                    app.config.auto_approve = false;
                    app.config.max_steps = genie_max_steps;
                }
            }
            app.status.max_steps = app.config.max_steps;
            app.notice(format!("switched to {mode} mode"));
        }

        SlashCommand::Reload => {
            if app.status.busy {
                app.notice("cannot reload while a turn is running");
                return;
            }
            *skills = load_skill_roots();
            match McpConfig::load(mcp_path) {
                Ok(mcp_config) => {
                    if let Err(err) = manager.reload(&mcp_config).await {
                        app.notice(format!("MCP reload warning: {err:#}"));
                    }
                }
                Err(err) => app.notice(format!("could not reload MCP config: {err:#}")),
            }
            match build_registry(manager, client).await {
                Ok(registry) => {
                    let tool_count = registry.len();
                    if let Some(agent) = agent_slot.as_mut() {
                        agent.set_registry(registry);
                        agent.set_skills(skills.clone());
                    }
                    app.notice(format!(
                        "reloaded: {tool_count} tools, {} skills",
                        skills.len()
                    ));
                }
                Err(err) => app.notice(format!("reload failed: {err:#}")),
            }
        }

        SlashCommand::Evolve { deep, description } => {
            let tier = if deep {
                EvolveTier::Deep
            } else {
                EvolveTier::Runtime
            };
            app.notice(format!(
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
            let mut evolver = Evolver::new(app.config.clone());
            let notify = events.sender();
            tokio::spawn(async move {
                let message = match evolver.run(request).await {
                    Ok(outcome) => describe_evolve_outcome(&outcome),
                    Err(err) => format!("evolve failed: {err:#}"),
                };
                let _ = notify.send(Event::Notice(message)).await;
            });
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
