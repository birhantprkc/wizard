//! TUI state machine: application state, slash commands, and the genie-mode
//! main loop. Rendering lives in [`crate::ui`]; raw events in
//! [`crate::event`].

mod command;
mod paste;
mod picker;
mod prompts;
mod runtime;
mod session;
mod term;
#[cfg(test)]
mod tests;
mod transcript;

pub use picker::{Picker, PickerItem, PickerKind, Selection, StatusLine, Suggestion};
pub use prompts::{Interview, PlanReview, ProviderPrompt};
pub use runtime::run_tui;
pub use term::restore_terminal_best_effort;
pub use transcript::{PaneStatus, SubagentPane, TranscriptEntry};

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use serde_json::Value;

use crate::agent::{Agent, AgentEvent, DoneReason, ImageSource, PlanVerdict, ultra};
// The built-in command table and its parser live in [`crate::commands`].
use crate::commands::CustomCommand;
use crate::commands::{COMMANDS, ProviderAction, SlashCommand, UltraAction};
use crate::config::{Config, Mode, ProviderKind, ReasoningEffort, UltraConfig};
use crate::event::Event;
use crate::image_view::ImageCache;
use crate::import_claude::{self, ImportSelection};
use crate::session_registry::{self, SessionRecord, SessionState};
use crate::tools::todo::TodoItem;
use crate::vim::{self, Pending, VimMode, VimOp, VimState};

use paste::{
    clipboard_image_bytes, looks_like_image_path_token, parse_data_image_url,
    resolve_pasted_image_path, save_image_bytes, save_pasted_image_bytes, sniff_image_ext,
};
use picker::is_builtin_command;
use prompts::{
    PROVIDER_ADD_ROW, PROVIDER_TYPES, PromptField, ULTRA_JUDGE_ROW, WEB_BACKENDS, prompt_question,
    web_backend_label, web_backend_needs_key, xai_oauth_session_present,
};
use transcript::{
    PANE_LINGER, collapse_long, fill_open_card, image_entries, replayed_refs, scroll_step,
};

/// How many user prompts may sit behind a running turn. Beyond this the next
/// Enter is refused with a notice rather than growing without bound.
const MESSAGE_QUEUE_CAP: usize = 32;

/// How long Ctrl-C waits for the turn to stop on its own before the task is
/// aborted instead.
///
/// The cooperative stop is worth waiting for: it keeps the agent (an abort
/// loses it and forces a rebuild off the session), it keeps the partial answer,
/// and it lets every subagent in flight — every `/ultra` candidate — close its
/// own pane out instead of being dropped mid-poll. Where the flag *is* checked
/// (each stream chunk, each tool boundary, each poll of the ultra fan-out) it
/// lands in milliseconds; this budget only bounds the case where it cannot be
/// checked, i.e. a tool call already running, which no flag can shorten.
const INTERRUPT_GRACE: Duration = Duration::from_millis(1_500);

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
    /// Compact todo band above the composer (toggled by `/todos`;
    /// auto-shown on the first todo update of the session). Reserves layout
    /// rows so it never covers transcript text.
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
    /// The mixture-of-agents roster `/ultra` is running, or `None` when ultra is
    /// off. Holds the *built* engine, not the [`UltraConfig`] behind it, for two
    /// reasons: the `ULTRA ×N` badge then counts the lenses the agent will
    /// actually fan out over rather than a config that may no longer resolve,
    /// and [`restore_ultra`](session::restore_ultra) can re-arm a rebuilt agent by cloning the handle
    /// instead of rebuilding a roster that could fail at exactly the moment
    /// there is no good way to report it. The engine binds no client — the agent
    /// supplies the live one — so the same instance survives a `/model` switch
    /// and the candidates follow the new model.
    pub ultra: Option<Arc<ultra::UltraEngine>>,
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
    /// User prompts submitted while a turn is already running. FIFO; the main
    /// loop pops one after each turn finishes (and after any post-turn slash
    /// commands the agent queued) so the next turn starts without the user
    /// having to retype. Each entry is already preprocessed and has already
    /// been written to the transcript + history on enqueue. Capped at
    /// [`MESSAGE_QUEUE_CAP`].
    pub message_queue: VecDeque<crate::commands::Preprocessed>,
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
            ultra: None,
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
            message_queue: VecDeque::new(),
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

    /// Open the `/ultra config` multi-select: one row per lens in the catalog
    /// (ultra's built-ins plus every subagent in `~/.wizard/subagents/`),
    /// pre-toggled to the configured roster, and a final [`ULTRA_JUDGE_ROW`] for
    /// the compare phase. Space toggles; Enter saves `[ultra]`. There is no
    /// separate "candidate count" row because there is no separate number: one
    /// toggled lens is one candidate.
    pub fn open_ultra_picker(&mut self) {
        let ultra = self.config.effective_ultra();
        let roster: std::collections::HashSet<&str> =
            ultra.lenses.iter().map(String::as_str).collect();
        let catalog = ultra::lens_catalog(&Config::subagents_dir().unwrap_or_default());
        let mut items: Vec<PickerItem> = catalog
            .iter()
            .map(|lens| PickerItem {
                value: lens.name.clone(),
                detail: lens.description.clone(),
                current: roster.contains(lens.name.as_str()),
            })
            .collect();
        items.push(PickerItem {
            value: ULTRA_JUDGE_ROW.to_string(),
            detail: match ultra.judges {
                0 => "off — the drafts go to the agent uncompared".to_string(),
                1 => "compares the drafts head-to-head before the agent executes".to_string(),
                n => format!("{n} judges compare the drafts head-to-head"),
            },
            current: ultra.judges > 0,
        });
        self.picker = Some(Picker {
            kind: PickerKind::UltraLenses,
            title: " ultra roster · space toggles · enter saves ".to_string(),
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
    /// [`PROVIDER_TYPES`], followed by the OpenAI-compatible presets from
    /// [`crate::llm::compat::PRESETS`], so the labels stay human-readable.
    pub fn open_provider_type_picker(&mut self) {
        let items: Vec<PickerItem> = PROVIDER_TYPES
            .iter()
            .map(|(label, detail)| PickerItem {
                value: (*label).to_string(),
                detail: (*detail).to_string(),
                current: false,
            })
            .chain(crate::llm::compat::PRESETS.iter().map(|preset| PickerItem {
                value: format!("{} — API key", preset.label),
                detail: preset.detail.to_string(),
                current: false,
            }))
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

    /// Scroll the pane at `index` by `delta` lines, per [`scroll_step`].
    fn scroll_pane(&mut self, index: usize, delta: i16) {
        let Some(pane) = self.panes.get_mut(index) else {
            return;
        };
        let max = pane.max_scroll.get();
        (pane.scroll, pane.scroll_follow) =
            scroll_step(pane.scroll_follow, pane.scroll, max, delta);
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

    /// Scroll the main transcript by `delta` lines, per [`scroll_step`].
    fn scroll_transcript(&mut self, delta: i16) {
        let max = self.transcript_max_scroll.get();
        (self.scroll, self.scroll_follow) =
            scroll_step(self.scroll_follow, self.scroll, max, delta);
    }

    /// Close out every pane still marked running, because the turn that owned
    /// them was killed outright rather than asked to stop.
    ///
    /// A run's pane is closed by the `SubagentRunDone` its own loop emits. Abort
    /// the turn's task and that loop is dropped mid-poll, so the event never
    /// comes: the pane keeps `finished: None`, [`App::retire_finished_panes`]
    /// retains it forever (`None => true`), and the rail grows a permanent
    /// pulsing row — one per in-flight run, every time a turn is aborted.
    ///
    /// The cooperative path ([`CancelHandle`](crate::agent::CancelHandle)) does not need this: every loop
    /// closes its own pane on the way out. This is for the fallback that does
    /// not give them the chance.
    pub fn fail_running_panes(&mut self, why: &str) {
        let now = Instant::now();
        for pane in &mut self.panes {
            if pane.status != PaneStatus::Running {
                continue;
            }
            pane.status = PaneStatus::Failed;
            pane.finished = Some(now);
            pane.transcript
                .push(TranscriptEntry::Notice(format!("failed: {why}")));
        }
    }

    /// Drop finished runs off the rail once they have been resting long enough
    /// to notice, so the rail shows live work instead of accumulating every
    /// subagent the session ever ran.
    ///
    /// Nothing is lost: a foreground run's report is the output of its
    /// `spawn_subagent` card in the main chat, a background run's report is
    /// written back into that same card when it lands (see
    /// [`App::record_subagent_report`]), and an `/ultra` candidate's draft is in
    /// the collapsed guidance card that phase pushes
    /// ([`AgentEvent::UltraGuidance`]).
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
        // Staged attachments belong to the composer's contents; emptying it
        // drops them too, so a cancelled draft never carries a ghost image
        // into the next submit.
        self.pending_images.clear();
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
                    if self.history_pos.is_some() {
                        self.history_next();
                    } else {
                        // Not browsing history: like plain ↓, drop into the
                        // subagent rail when there is one.
                        self.focus_rail();
                    }
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
                        // Then the todo band (it auto-opens on the first
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
                // Attach an image from the clipboard — the explicit companion to
                // the empty-paste path, for terminals (or a tmux passthrough)
                // that don't forward an image paste at all. Not while collecting
                // a masked field, where the clipboard would hold a secret.
                KeyCode::Char('v') if !self.prompt_is_masked() => {
                    if !self.attach_clipboard_image() {
                        self.notice("no image on the clipboard to attach");
                    }
                    self.sync_input_mode();
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
                // out to the rail just to see the next one. j/k join in under
                // vim Normal mode, where letters are motions rather than text.
                KeyCode::Up if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                    self.step_pane(index, -1);
                    return Ok(None);
                }
                KeyCode::Down if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                    self.step_pane(index, 1);
                    return Ok(None);
                }
                KeyCode::Char('k')
                    if self.vim.is_normal()
                        && !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    self.step_pane(index, -1);
                    return Ok(None);
                }
                KeyCode::Char('j')
                    if self.vim.is_normal()
                        && !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
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
            let plain = !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
            match key.code {
                // Arrows only — no j/k. The rail is a focus you land in from a
                // live text composer, so every letter has to fall through and
                // be typed; binding letters here would eat the first character
                // of "just do X". Vim Normal mode is the exception: letters
                // are motions there, not text, so j/k mirror ↑/↓.
                KeyCode::Up => {
                    self.rail_select(-1);
                    return Ok(None);
                }
                KeyCode::Down => {
                    self.rail_select(1);
                    return Ok(None);
                }
                KeyCode::Char('k') if plain && self.vim.is_normal() => {
                    self.rail_select(-1);
                    return Ok(None);
                }
                KeyCode::Char('j') if plain && self.vim.is_normal() => {
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
                        PickerKind::ClaudeImport
                            | PickerKind::FusionPanel
                            | PickerKind::UltraLenses
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
                        PickerKind::UltraLenses => {
                            // The judge row is the tail (open_ultra_picker put it
                            // there); everything above it is a lens. Saving is
                            // left to the command handler, which is the only
                            // place that can both persist [ultra] and re-arm a
                            // running engine in one step.
                            let Some((judge, lenses)) = picker.items.split_last() else {
                                return Ok(None);
                            };
                            let lenses: Vec<String> = lenses
                                .iter()
                                .filter(|item| item.current)
                                .map(|item| item.value.clone())
                                .collect();
                            if lenses.is_empty() {
                                self.notice(
                                    "select at least one lens (Space toggles) — ultra has nothing \
                                     to fan out over without one",
                                );
                                return Ok(None);
                            }
                            // A checkbox can only say none-or-one, so a count
                            // above one (which only `config.toml` can set) is
                            // preserved when the row stays on, exactly as the
                            // fusion picker preserves `rounds`.
                            let base = self.config.effective_ultra();
                            let judges = if judge.current { base.judges.max(1) } else { 0 };
                            AppAction::Command(SlashCommand::Ultra(UltraAction::Apply(
                                UltraConfig {
                                    lenses,
                                    judges,
                                    ..base
                                },
                            )))
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
                                // OpenAI-compatible presets (Gemini, DeepSeek,
                                // Groq, …) appended after the fixed rows — the
                                // default model is preset, so only the key is
                                // asked for.
                                index => {
                                    if let Some(preset) = crate::llm::compat::PRESETS
                                        .get(index - PROVIDER_TYPES.len())
                                    {
                                        self.begin_provider_prompt(ProviderPrompt {
                                            kind: ProviderKind::Openai,
                                            name: preset.name.to_string(),
                                            base_url: preset.base_url.to_string(),
                                            model: preset.default_model().to_string(),
                                            api_key: None,
                                            queue: VecDeque::from([PromptField::ApiKey]),
                                        });
                                    }
                                }
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
                    // Then the todo band (it auto-opens on the first todo
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
    ///
    /// When a turn is already running the prompt is queued instead of rejected:
    /// it still lands in the transcript (so the user sees their words) and the
    /// main loop starts it once the current turn finishes. Rebuilds still
    /// refuse — the agent slot is empty then, and a queued turn would only
    /// bounce again.
    fn submit_prompt(&mut self, input: String) -> Option<AppAction> {
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
        if self.status.busy {
            if self.message_queue.len() >= MESSAGE_QUEUE_CAP {
                self.notice(format!(
                    "message queue is full ({MESSAGE_QUEUE_CAP}) — wait for a turn to finish"
                ));
                // Put the staged images back so the user doesn't lose them.
                self.pending_images.append(&mut prepared.images);
                return None;
            }
            self.push_history(&input);
            self.clear_input();
            self.transcript.push(TranscriptEntry::User(input));
            self.scroll_to_bottom();
            let position = self.message_queue.len() + 1;
            self.message_queue.push_back(prepared);
            self.notice(format!("queued — will send after this turn (#{position})"));
            return None;
        }
        self.push_history(&input);
        self.clear_input();
        self.transcript.push(TranscriptEntry::User(input));
        self.scroll_to_bottom();
        Some(AppAction::Submit(prepared))
    }

    /// Pop the next queued user prompt, if any. Used by the main loop once a
    /// turn returns the agent and any post-turn agent commands have run.
    pub fn pop_queued_message(&mut self) -> Option<crate::commands::Preprocessed> {
        self.message_queue.pop_front()
    }

    /// Queue the first working turn for a freshly set `/goal`. The prompt
    /// lands in the transcript and the message queue, so the main loop's
    /// post-command drain starts it immediately when the agent is idle, or
    /// right after the current turn otherwise.
    pub fn queue_goal_kickoff(&mut self, goal: &str) {
        if self.message_queue.len() >= MESSAGE_QUEUE_CAP {
            self.notice(format!(
                "goal saved, but the message queue is full ({MESSAGE_QUEUE_CAP}) — \
                 work will not auto-start; send a message once a turn finishes"
            ));
            return;
        }
        let kickoff = format!(
            "A standing goal was just set for this project:\n\n{goal}\n\n\
             Start working toward it now: break it into concrete steps and \
             begin executing them. Keep going until you reach a natural \
             checkpoint, then summarize the progress made and what remains."
        );
        self.transcript.push(TranscriptEntry::User(kickoff.clone()));
        self.scroll_to_bottom();
        self.message_queue
            .push_back(crate::commands::Preprocessed::text_only(kickoff));
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

        // An image paste the terminal can't deliver arrives as an empty paste:
        // bracketed paste only carries text, so the image's bytes are left on
        // the OS clipboard. Read them there and attach — the same affordance as
        // Claude Code's `[Image #N]`. A genuinely empty paste finds nothing and
        // stays silent.
        if text.trim().is_empty() {
            self.attach_clipboard_image();
            self.sync_input_mode();
            return;
        }

        self.insert_str(text);
        self.sync_input_mode();
    }

    /// Attach an image from the OS clipboard, if one is present, staging it for
    /// the next submit and showing an `[Image #N]` token. Returns whether an
    /// image was found — so an explicit Ctrl-V can report an empty clipboard
    /// while an empty paste can stay quiet.
    fn attach_clipboard_image(&mut self) -> bool {
        let Some(bytes) = clipboard_image_bytes() else {
            return false;
        };
        let ext = sniff_image_ext(&bytes).unwrap_or("png");
        match save_image_bytes(&bytes, ext) {
            Ok(path) => self.stage_image(path, "pasted image"),
            Err(err) => self.notice(format!("could not attach pasted image: {err}")),
        }
        true
    }

    /// Stage `path` for the next submit and insert a numbered `[Image #N]`
    /// token — the composer indicator Claude Code shows for a pasted image.
    /// `label` names the source only for the confirmation notice.
    fn stage_image(&mut self, path: PathBuf, label: &str) {
        if self.pending_images.iter().any(|p| p == &path) {
            self.notice(format!("{label} is already attached"));
            return;
        }
        self.pending_images.push(path);
        let n = self.pending_images.len();
        let token = format!("[Image #{n}]");
        if !self.input.is_empty() && !self.input.chars().last().is_some_and(|c| c.is_whitespace()) {
            self.insert_char(' ');
        }
        self.insert_str(&token);
        self.notice(format!("attached {label} as Image #{n}"));
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
                if let Some(output) = fill_open_card(&mut self.transcript, &name, output) {
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
            // The ultra pre-phase's drafts and verdict, as a collapsed card.
            // This is the *only* durable record of them: the candidates' panes
            // retire off the rail seconds after they finish, minutes before the
            // main agent is done working from what they wrote, and the guidance
            // itself is a system message, which the transcript never renders.
            AgentEvent::UltraGuidance { label, guidance } => {
                self.flush_streaming();
                self.transcript.push(TranscriptEntry::ToolCard {
                    name: label,
                    args: Value::Null,
                    output: Some(guidance),
                    is_error: false,
                    // Always folded: it is tens of KB, and the point of the turn
                    // is the answer below it, not the drafts behind it.
                    collapsed: true,
                });
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
                // Same collapse policy as the main transcript; a miss (no
                // open card in the pane) is dropped.
                fill_open_card(&mut self.panes[index].transcript, &name, output);
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
                // Auto-show the overlay the first time the agent starts a
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
    /// Interrupt the running turn (Ctrl-C): ask it to stop cooperatively, and
    /// abort its task if it does not (see [`INTERRUPT_GRACE`]).
    Interrupt,
    /// Copy the current mouse selection to the clipboard. Handled in the main
    /// loop because it owns the terminal (and thus the rendered cell buffer).
    CopySelection,
}
