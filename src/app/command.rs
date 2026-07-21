//! Slash-command dispatch: [`CommandContext`] borrows the main loop's
//! stack for the duration of one command.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::{Mutex, mpsc};

use crate::agent::Agent;
use crate::commands::{
    FusionAction, MemoryAction, ProviderAction, ServerAction, SlashCommand, UltraAction,
};
use crate::config::{
    Config, Mode, ProviderConfig, ProviderKind, ReasoningEffort, StepBudget, UltraConfig,
};
use crate::event::{Event, EventLoop};
use crate::evolve::{EvolveRequest, EvolveTier, Evolver, PublishRequest, publish};
use crate::import_claude::{self, ImportSelection};
use crate::llm::provider::LlmProvider;
use crate::mcp::{McpConfig, McpManager};
use crate::server;
use crate::skills::Skill;

use super::session::{
    SessionTarget, build_agent, build_registry, load_skill_roots, restore_ultra, switch_model_task,
};
use crate::agent::subagent;

use super::{App, Picker, PickerItem, PickerKind};

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
pub(super) async fn git_diff_text(root: &Path) -> Result<String> {
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
pub(super) fn is_wizard_state_path(path: &str) -> bool {
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
/btw <question>             ask a side question without adding it to the conversation\n  \
/agents                     browse subagents and delegate to one\n  \
/subagents                  monitor the subagents running in this session\n  \
/evolve [--deep] <desc>     self-extension (skill / MCP / scripted tool)\n  \
/publish [branch]           fork Wizard to your GitHub, get a one-line installer\n  \
/provider                   add or switch LLM providers (interactive picker)\n  \
/fusion [config]            toggle model fusion (panel debate → synthesis), or configure the panel\n  \
/ultra [config]             toggle mixture of agents (candidates draft → judge rules → agent acts)\n  \
/server [status|start|stop] manage the local llama-server\n  \
/login xai                  sign in with your xAI account (OAuth, no API key)\n  \
/reload                     reload skills, scripted tools, and MCP servers\n  \
/diff                       toggle the git diff sidebar\n  \
/todos                      toggle the todo list above the input\n  \
/dashboard                  session manager: all live wizard sessions on this machine\n  \
/cost                       show session token usage and cost\n  \
/memory [read|forget <name>] list, show, or forget saved project memories\n  \
/status                     show session status (model, usage, todos, tasks)\n  \
/bashes                     list background tasks (id, status, command)\n  \
/goal [text]                show the standing goal, or set one and start working on it\n  \
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
Ctrl-C                      interrupt · press twice to quit";

/// Everything a slash command may touch, borrowed from the main loop for
/// the duration of one dispatch.
pub(super) struct CommandContext<'a> {
    pub(super) app: &'a mut App,
    pub(super) client: &'a mut Arc<dyn LlmProvider>,
    pub(super) agent_slot: &'a mut Option<Agent>,
    pub(super) manager: &'a Arc<Mutex<McpManager>>,
    pub(super) skills: &'a mut Vec<Skill>,
    pub(super) project_root: &'a Path,
    pub(super) mcp_path: &'a Path,
    pub(super) genie_max_steps: StepBudget,
    pub(super) events: &'a EventLoop,
}

impl CommandContext<'_> {
    /// Execute one slash command against the running stack.
    pub(super) async fn run(mut self, command: SlashCommand) {
        match command {
            SlashCommand::Help => self.app.notice(HELP_TEXT),
            SlashCommand::Quit => self.app.should_quit = true,
            SlashCommand::Diff => self.toggle_diff().await,
            SlashCommand::Todos => self.toggle_todos(),
            SlashCommand::Dashboard => self.toggle_dashboard(),
            SlashCommand::Subagents => self.toggle_subagents(),
            SlashCommand::Cost => self.cost(),
            SlashCommand::Memory(action) => self.memory(action),
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
            SlashCommand::Btw(question) => self.btw(question),
            SlashCommand::Agents => self.open_agents_picker(),
            SlashCommand::Reload => self.reload().await,
            SlashCommand::Evolve { deep, description } => self.evolve(deep, description),
            SlashCommand::Publish { branch } => self.publish(branch),
            SlashCommand::Fusion(FusionAction::Toggle) => self.toggle_fusion().await,
            SlashCommand::Fusion(FusionAction::Config) => self.open_fusion_picker(),
            SlashCommand::Ultra(UltraAction::Toggle) => self.toggle_ultra(),
            SlashCommand::Ultra(UltraAction::Config) => self.open_ultra_picker(),
            SlashCommand::Ultra(UltraAction::Apply(ultra)) => self.apply_ultra(ultra),
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

    /// `/todos`: toggle the compact todo band above the composer.
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

    /// `/memory`: the user's window onto the memories the agent writes with
    /// the `memory` tool — list them, read one, or forget one. Rendered by
    /// [`crate::memory::report`], the same renderer the GUI answers with.
    fn memory(&mut self, action: MemoryAction) {
        self.app
            .notice(crate::memory::report(self.project_root, &action));
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
        text.push_str(&format!(
            "\nultra: {}",
            match &self.app.ultra {
                Some(ultra) => ultra.label(),
                None => "off".to_string(),
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
    /// non-destructively preserving cycles and existing progress notes, then
    /// immediately start working toward it (queued behind any running turn).
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
        self.app.queue_goal_kickoff(&text);
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
        // Drop any prompts queued behind a previous turn — a cleared
        // conversation shouldn't auto-fire messages the user typed mid-turn.
        self.app.message_queue.clear();
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
                // The rewound turns no longer exist: replay the truncated
                // conversation into the transcript view (same as `/resume`).
                let messages = agent.session().load_messages().unwrap_or_default();
                self.app.load_transcript(messages);
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
        restore_ultra(self.app, &mut agent);
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

    /// `/btw <question>`: one-shot side question. Unlike most commands this is
    /// allowed *while a turn is running* — that is the point — so it does not
    /// go through [`Self::agent_unavailable`]. The main loop owns the client
    /// and either the live agent or a mid-turn snapshot of its history.
    fn btw(&mut self, question: String) {
        if self.app.rebuilding.is_some() {
            self.app
                .notice("cannot ask a side question while the agent is rebuilding");
            return;
        }
        if self.app.pending_btw.is_some() || self.app.btw_inflight {
            self.app.notice("already answering a /btw — wait for it to finish");
            return;
        }
        // A light "working on it" marker; the answer arrives as its own notice.
        self.app.notice("answering /btw…");
        self.app.pending_btw = Some(question);
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
    pub(super) async fn merge_mcp_registry(&mut self) {
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
                Ok(outcome) => crate::evolve::describe_outcome(&outcome),
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
                restore_ultra(self.app, &mut agent);
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
        // Stacked, the two multiply: every ultra candidate is a full agent run
        // on the active client, and a fused client turns each of *its* model
        // calls into a panel debate plus a synthesis. Refuse rather than quietly
        // bill a turn at candidates × panel × rounds.
        if self.app.ultra.is_some() {
            self.app.notice(
                "fusion cannot run under ultra — each of ultra's candidates would re-run the \
                 whole panel; /ultra to turn ultra off first",
            );
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

    /// Toggle `/ultra`: mixture of agents. Where `/fusion` swaps the client and
    /// therefore has to rebuild the agent from scratch, ultra changes nothing
    /// about *which* model answers — the candidates fan out over the client and
    /// model that are already active. So this is a plain flag on the live agent:
    /// no rebuild, no session reset, and the conversation in front of the user
    /// survives the toggle, which is what makes it usable mid-task ("that answer
    /// was thin — /ultra, try again").
    fn toggle_ultra(&mut self) {
        if self.agent_unavailable("toggle ultra") {
            return;
        }
        if self.app.ultra.is_some() {
            self.app.ultra = None;
            if let Some(agent) = self.agent_slot.as_mut() {
                agent.set_ultra(None);
            }
            self.app
                .notice("ultra off — one agent per turn again, no pre-phase");
            return;
        }
        if self.app.fusion_active {
            self.app.notice(
                "ultra cannot run on top of fusion — every candidate would re-run the whole \
                 panel; /fusion to turn fusion off first",
            );
            return;
        }
        // `build_ultra` is the sole validation gate for `[ultra]`, so a roster
        // the user hand-edited into an unusable state surfaces here, at the
        // toggle, instead of at the top of their next turn.
        let engine = match self.app.config.build_ultra() {
            Ok(engine) => Arc::new(engine),
            Err(err) => {
                self.app.notice(format!("could not start ultra: {err:#}"));
                return;
            }
        };
        let label = engine.label();
        if let Some(agent) = self.agent_slot.as_mut() {
            agent.set_ultra(Some(engine.clone()));
        }
        self.app.ultra = Some(engine);
        self.app.notice(format!(
            "{label} — each turn now drafts on the active model, compares, then acts; \
             /ultra to turn off"
        ));
    }

    /// Open the `/ultra config` roster editor: which lenses run as candidates,
    /// and whether a judge compares their drafts.
    fn open_ultra_picker(&mut self) {
        self.app.open_ultra_picker();
    }

    /// Save the roster chosen at that editor. Building it first is not a
    /// formality: [`UltraEngine::build`](ultra::UltraEngine::build) is the only
    /// thing that rejects an unknown lens or an out-of-range count, so a roster
    /// that would not run is reported and never written. When ultra is already
    /// on, the live agent moves to the new roster in the same breath as the
    /// badge — the two must not disagree about how many candidates the next turn
    /// is about to spend.
    fn apply_ultra(&mut self, ultra: UltraConfig) {
        let engine = match self.app.config.build_ultra_from(&ultra) {
            Ok(engine) => Arc::new(engine),
            Err(err) => {
                self.app.notice(format!("ultra roster rejected: {err:#}"));
                return;
            }
        };
        let label = engine.label();
        self.app.config.ultra = Some(ultra);
        if let Err(err) = self.app.config.save() {
            self.app
                .notice(format!("could not save ultra config: {err:#}"));
            return;
        }
        if self.app.ultra.is_none() {
            self.app.notice(format!("{label} — /ultra to turn on"));
            return;
        }
        match self.agent_slot.as_mut() {
            Some(agent) => {
                agent.set_ultra(Some(engine.clone()));
                self.app.ultra = Some(engine);
                self.app.notice(format!("{label} — applied"));
            }
            // Mid-turn the agent is inside the turn and holds the old engine.
            // Swapping only the badge would misreport a fan-out the user is
            // watching run, so leave both alone and say which one they have.
            None => self.app.notice(format!(
                "{label} — saved; the running turn keeps the old roster, /ultra off then on to \
                 pick this one up"
            )),
        }
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
    pub(super) async fn add_provider_config(&mut self, provider: ProviderConfig, summary: String) {
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
