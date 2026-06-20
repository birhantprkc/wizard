//! First-run onboarding: a small full-screen wizard that asks for a provider,
//! model, optional messaging gateway, and mode, then writes
//! `~/.wizard/config.toml`.
//!
//! The module is split into two halves:
//! - **Pure logic** ([`Answers`], [`Answers::into_config`], [`parse_chat_ids`],
//!   and the option tables) — fully unit-tested without a terminal.
//! - **TUI** ([`run`] and the private `select` / `text_input` event loops) —
//!   ratatui + crossterm rendering in the existing aesthetic (white accent,
//!   dim rounded borders, transparent background).
//!
//! Keeping the answer → [`Config`] mapping pure means the interesting behavior
//! is testable; the TUI layer is a thin shell over it.

use std::io::Stdout;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use crate::config::{Config, GatewayConfig, GatewayKind, Mode, ProviderConfig, ProviderKind};
use crate::hardware::{self, GgufModel};
use crate::import_claude::{self, ImportSelection};

/// White accent, matching [`crate::ui`].
const ACCENT: Color = Color::White;
/// Dim chrome (borders, hints).
const DIM: Color = Color::DarkGray;
/// Secondary text.
const TEXT_DIM: Color = Color::Gray;

// ---------------------------------------------------------------------------
// Pure logic (unit-tested)
// ---------------------------------------------------------------------------

/// Which provider family the user picked in step 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderChoice {
    /// Local llama.cpp `llama-server` — the one-click "Local" pick resolves
    /// here too (Wizard downloads the model and installs llama-server itself).
    LlamaCpp,
    /// Local Ollama server.
    Ollama,
    /// OpenAI or any OpenAI-compatible endpoint.
    Openai,
    /// Anthropic Messages API.
    Anthropic,
    /// OpenRouter with a plain API key.
    OpenRouter,
    /// xAI (Grok) with a plain API key.
    Xai,
    /// xAI via account sign-in (OAuth, `wizard --login xai`).
    XaiOauth,
    /// A custom OpenAI-compatible endpoint (base URL entered by hand).
    Custom,
}

/// The collected answers from the wizard. Converting this into a [`Config`]
/// ([`Answers::into_config`]) is pure and unit-tested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answers {
    /// Provider family chosen.
    pub provider: ProviderChoice,
    /// Provider id stored in [`ProviderConfig::name`] (e.g. `"local"`).
    pub provider_name: String,
    /// Backend kind for the single configured provider.
    pub kind: ProviderKind,
    /// Base URL for the provider.
    pub base_url: String,
    /// Model tag.
    pub model: String,
    /// Env var holding the API key (cloud providers only).
    pub api_key_env: Option<String>,
    /// Path to the GGUF model file (llama.cpp only) — lets Wizard spawn
    /// `llama-server` itself.
    pub gguf_path: Option<String>,
    /// Messaging gateway to configure.
    pub gateway_kind: GatewayKind,
    /// Env var holding the gateway bot token (Telegram only).
    pub gateway_token_env: Option<String>,
    /// Allowed inbound chat IDs (Telegram only; empty = allow all).
    pub gateway_allowed_chat_ids: Vec<i64>,
    /// Personality mode.
    pub mode: Mode,
    /// Artifacts to import from an existing Claude Code install, if any. The
    /// actual import (file writes + spinner verbs) runs in [`run_blocking`]
    /// after [`Answers::into_config`], so this is consumed there rather than in
    /// the pure config mapping.
    pub claude_import: Option<ImportSelection>,
}

impl Answers {
    /// Build a [`Config`] from the answers: one configured provider (set
    /// active), the chosen mode, the `[gateway]` section, and — for an Ollama
    /// choice — the legacy `model` / `ollama_host` fields mirrored for
    /// back-compat with pre-`providers` config readers.
    pub fn into_config(self) -> Config {
        let mut config = Config::default();

        let provider = ProviderConfig {
            name: self.provider_name.clone(),
            kind: self.kind,
            base_url: self.base_url.clone(),
            model: self.model.clone(),
            api_key_env: self.api_key_env.clone(),
            gguf_path: self.gguf_path.clone(),
            usd_per_mtok_in: None,
            usd_per_mtok_out: None,
        };

        // Mirror an Ollama choice into the legacy fields so config files remain
        // readable by code paths that predate the providers table.
        if self.kind == ProviderKind::Ollama {
            config.model = self.model.clone();
            config.ollama_host = self.base_url.clone();
        }

        // Mirror a llama.cpp choice into the top-level fields so the same
        // local provider is synthesized if the providers table ever empties
        // (e.g. `/provider remove`).
        if self.kind == ProviderKind::LlamaCpp {
            config.llamacpp_host = self.base_url.clone();
            config.gguf_path = self.gguf_path.clone();
        }

        config.providers = vec![provider];
        config.active_provider = Some(self.provider_name);
        config.mode = self.mode;
        config.gateway = GatewayConfig {
            kind: self.gateway_kind,
            token_env: self.gateway_token_env,
            allowed_chat_ids: self.gateway_allowed_chat_ids,
        };
        config
    }
}

/// Parse a comma-separated list of numeric chat IDs. Whitespace and empty
/// entries are ignored; an empty input yields an empty list ("allow all").
/// A non-numeric entry is an error naming the offending token.
pub fn parse_chat_ids(input: &str) -> Result<Vec<i64>, String> {
    let mut ids = Vec::new();
    for token in input.split(',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        let id: i64 = token
            .parse()
            .map_err(|_| format!("'{token}' is not a valid numeric chat id"))?;
        ids.push(id);
    }
    Ok(ids)
}

/// OpenAI model options offered in the picker (first is the default).
const OPENAI_MODELS: &[&str] = &["gpt-4o", "gpt-4o-mini", "o1"];
/// Anthropic model options offered in the picker (first is the default — the
/// latest Claude).
const ANTHROPIC_MODELS: &[&str] = &[
    "claude-fable-5",
    "claude-opus-4-8",
    "claude-sonnet-4-6",
    "claude-haiku-4-5",
];
/// xAI (Grok) model options offered in the picker (first is the default).
const XAI_MODELS: &[&str] = &["grok-4.3", "grok-code-fast-1"];
/// Ollama tier options offered alongside the hardware-suggested default.
const OLLAMA_TIERS: &[&str] = &["qwen3.6:35b", "qwen3.6:27b", "qwen3.5:9b"];

/// Default base URL for a local llama.cpp `llama-server`.
const LLAMACPP_BASE_URL: &str = crate::config::DEFAULT_LLAMACPP_HOST;
/// Default base URL for a local Ollama server.
const OLLAMA_BASE_URL: &str = "http://127.0.0.1:11434";
/// Default base URL for the OpenAI API.
const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
/// Default base URL for the Anthropic API.
const ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";
/// Default base URL for the xAI API.
const XAI_BASE_URL: &str = crate::llm::xai_oauth::DEFAULT_BASE_URL;
/// Default base URL for the OpenRouter API.
const OPENROUTER_BASE_URL: &str = crate::llm::openrouter::DEFAULT_BASE_URL;
/// Default env var name for the OpenAI key.
const OPENAI_KEY_ENV: &str = "OPENAI_API_KEY";
/// Default env var name for the Anthropic key.
const ANTHROPIC_KEY_ENV: &str = "ANTHROPIC_API_KEY";
/// Default env var name for the xAI key.
const XAI_KEY_ENV: &str = crate::llm::xai_oauth::DEFAULT_KEY_ENV;
/// Default env var name for the OpenRouter key.
const OPENROUTER_KEY_ENV: &str = crate::llm::openrouter::DEFAULT_KEY_ENV;
/// Default OpenRouter model (the Auto Router).
const OPENROUTER_MODEL: &str = crate::llm::openrouter::DEFAULT_MODEL;

// ---------------------------------------------------------------------------
// TUI entry point
// ---------------------------------------------------------------------------

/// Run the onboarding wizard. Returns `Ok(Some(config))` once the user
/// finishes (the config has already been saved to `~/.wizard/config.toml` and
/// a plaintext summary printed), or `Ok(None)` if the user cancelled
/// (Esc / Ctrl-C). Terminal setup/teardown is restored on every exit path,
/// including errors.
///
/// The interactive loop is synchronous (blocking crossterm reads); it runs on
/// a blocking thread so it never stalls the async runtime.
pub async fn run() -> Result<Option<Config>> {
    tokio::task::spawn_blocking(run_blocking)
        .await
        .context("onboarding task panicked")?
}

/// Synchronous core of [`run`].
fn run_blocking() -> Result<Option<Config>> {
    let mut terminal = setup_terminal()?;
    let outcome = collect_answers(&mut terminal);
    restore_terminal_best_effort();

    let answers = match outcome {
        Ok(Some(answers)) => answers,
        Ok(None) => return Ok(None),
        Err(err) => return Err(err),
    };

    let import = answers.claude_import;
    let mut config = answers.into_config();

    // Perform the Claude Code import (MCP/commands file writes + spinner verbs)
    // before saving, so the verbs land in the same config write.
    let import_summary = match import {
        Some(selection) => match import_claude::run_import(&selection) {
            Ok(outcome) => {
                if !outcome.spinner_verbs.is_empty() {
                    config.ui.spinner_verbs = outcome.spinner_verbs.clone();
                }
                Some(outcome.summary())
            }
            Err(err) => Some(format!("import failed: {err:#}")),
        },
        None => None,
    };

    config.save().context("saving config from onboarding")?;
    print_summary(&config);
    if let Some(summary) = import_summary
        && !summary.is_empty()
    {
        println!("Imported from Claude Code:");
        for line in summary.lines() {
            println!("  • {line}");
        }
        println!();
    }
    Ok(Some(config))
}

type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Drive the sequence of steps. Returns `Ok(None)` as soon as any step is
/// cancelled.
fn collect_answers(terminal: &mut Tui) -> Result<Option<Answers>> {
    // Step 1 — provider. "Local" is one pick: no further model questions —
    // Wizard self-configures llama.cpp (or an Ollama install that already has
    // a model) and downloads a hardware-sized GGUF on first run. The BYOM
    // local flavors sit alongside the cloud providers for people who want to
    // bring their own model and pick the pieces themselves.
    let provider_options = [
        Opt::new(
            "Local (recommended)",
            "one pick — llama.cpp & Ollama set up for you, model sized to this machine; \
             private, no API key",
        ),
        Opt::new("OpenRouter", "hundreds of models via OPENROUTER_API_KEY"),
        Opt::new("xAI (Grok), API key", "grok-4.3 via XAI_API_KEY"),
        Opt::new("xAI account sign-in", "grok-4.3 via OAuth, no API key"),
        Opt::new("OpenAI / OpenAI-compatible", "gpt-4o and friends"),
        Opt::new("Anthropic (Claude)", "claude-fable-5"),
        Opt::new("Custom OpenAI-compatible endpoint", "any base URL"),
        Opt::new(
            "BYOM — llama.cpp",
            "bring your own model: any GGUF, your server URL",
        ),
        Opt::new("BYOM — Ollama", "bring your own model: any Ollama tag"),
    ];
    let provider = match select(
        terminal,
        "Provider",
        "Where should Wizard send its requests?",
        &provider_options,
        0,
    )? {
        Some(index) => index,
        None => return Ok(None),
    };

    // Step 2 — model (+ key env / base url, depending on provider). The
    // one-click local pick asks nothing further.
    let collected = match provider {
        0 => collect_local_auto(),
        1 => match collect_openrouter(terminal)? {
            Some(c) => c,
            None => return Ok(None),
        },
        2 => match collect_xai(terminal)? {
            Some(c) => c,
            None => return Ok(None),
        },
        3 => match collect_xai_oauth(terminal)? {
            Some(c) => c,
            None => return Ok(None),
        },
        4 => match collect_openai(terminal)? {
            Some(c) => c,
            None => return Ok(None),
        },
        5 => match collect_anthropic(terminal)? {
            Some(c) => c,
            None => return Ok(None),
        },
        6 => match collect_custom(terminal)? {
            Some(c) => c,
            None => return Ok(None),
        },
        7 => match collect_llamacpp(terminal)? {
            Some(c) => c,
            None => return Ok(None),
        },
        _ => match collect_ollama(terminal)? {
            Some(c) => c,
            None => return Ok(None),
        },
    };

    // Step 3 — messaging gateway.
    let gateway_options = [
        Opt::new("None — terminal only", "recommended"),
        Opt::new("Telegram", "chat with Wizard from a bot"),
    ];
    let gateway = match select(
        terminal,
        "Messaging gateway",
        "Expose Wizard over a chat platform?",
        &gateway_options,
        0,
    )? {
        Some(index) => index,
        None => return Ok(None),
    };

    let (gateway_kind, gateway_token_env, gateway_allowed_chat_ids) = if gateway == 1 {
        let token_env = match text_input(
            terminal,
            "Telegram bot token env var",
            "Create a bot via @BotFather, then export the token in this var.",
            GatewayConfig::DEFAULT_TOKEN_ENV,
        )? {
            Some(value) => value,
            None => return Ok(None),
        };
        // Allowed chat IDs: re-prompt on a parse error rather than discarding
        // the answer.
        let allowed = loop {
            let raw = match text_input(
                terminal,
                "Allowed chat IDs (optional)",
                "Comma-separated numeric chat IDs. Leave empty to allow all.",
                "",
            )? {
                Some(value) => value,
                None => return Ok(None),
            };
            match parse_chat_ids(&raw) {
                Ok(ids) => break ids,
                Err(message) => {
                    notice(terminal, &message)?;
                }
            }
        };
        (GatewayKind::Telegram, Some(token_env), allowed)
    } else {
        (GatewayKind::None, None, Vec::new())
    };

    // Step 4 — mode.
    let mode_options = [
        Opt::new(
            "Genie — interactive",
            "bypass permissions; acts without asking (recommended)",
        ),
        Opt::new("Sovereign — autonomous", "autonomous; works continuously"),
    ];
    let mode = match select(
        terminal,
        "Mode",
        "How should Wizard behave by default?",
        &mode_options,
        0,
    )? {
        Some(0) => Mode::Genie,
        Some(_) => Mode::Sovereign,
        None => return Ok(None),
    };

    // Step 5 — optional: import artifacts from an existing Claude Code install.
    // Only shown when `~/.claude` exists. Esc here skips the import (the rest of
    // the config is already complete) rather than aborting onboarding.
    let claude_import = if import_claude::claude_home().is_some() {
        collect_claude_import(terminal)?
    } else {
        None
    };

    Ok(Some(Answers {
        provider: collected.provider,
        provider_name: collected.provider_name,
        kind: collected.kind,
        base_url: collected.base_url,
        model: collected.model,
        api_key_env: collected.api_key_env,
        gguf_path: collected.gguf_path,
        gateway_kind,
        gateway_token_env,
        gateway_allowed_chat_ids,
        mode,
        claude_import,
    }))
}

/// Optional final step: offer to import artifacts from an existing Claude Code
/// install (`~/.claude`). Returns the chosen selection, or `None` to skip (Esc,
/// or nothing toggled).
fn collect_claude_import(terminal: &mut Tui) -> Result<Option<ImportSelection>> {
    let (mcp, commands, verbs) = import_claude::counts();
    let options = [
        Opt::new(
            format!("MCP servers ({mcp})"),
            "merge into ~/.wizard/mcp.toml",
        ),
        Opt::new(
            format!("Custom commands ({commands})"),
            "copy into ~/.wizard/commands/",
        ),
        Opt::new(
            format!("Spinner verbs ({verbs})"),
            "adopt Claude Code's spinner verbs",
        ),
    ];
    let checked = match multi_select(
        terminal,
        "Import from Claude Code",
        "Found ~/.claude — bring over any of these?",
        &options,
    )? {
        Some(checked) => checked,
        None => return Ok(None), // skipped
    };
    let selection = ImportSelection {
        mcp: checked.first().copied().unwrap_or(false),
        commands: checked.get(1).copied().unwrap_or(false),
        verbs: checked.get(2).copied().unwrap_or(false),
    };
    Ok((!selection.is_empty()).then_some(selection))
}

/// Per-provider answers gathered in step 2.
struct ProviderAnswers {
    provider: ProviderChoice,
    provider_name: String,
    kind: ProviderKind,
    base_url: String,
    model: String,
    api_key_env: Option<String>,
    gguf_path: Option<String>,
}

/// Pick a model from `models` plus a "type a custom tag" row; on the custom
/// row, fall through to a free-text input defaulting to `custom_default`.
/// Returns `Ok(None)` on cancel.
fn pick_model(
    terminal: &mut Tui,
    subtitle: &str,
    models: &[(String, String)],
    custom_default: &str,
) -> Result<Option<String>> {
    let mut options: Vec<Opt> = models
        .iter()
        .map(|(value, detail)| Opt::new(value, detail))
        .collect();
    options.push(Opt::new("Type a custom tag…", ""));
    let custom_index = options.len() - 1;

    let selected = match select(terminal, "Model", subtitle, &options, 0)? {
        Some(index) => index,
        None => return Ok(None),
    };
    if selected == custom_index {
        match text_input(
            terminal,
            "Custom model tag",
            "Enter the exact model tag.",
            custom_default,
        )? {
            Some(model) => Ok(Some(model)),
            None => Ok(None),
        }
    } else {
        Ok(Some(models[selected].0.clone()))
    }
}

/// `~/.wizard/models/` — where `install.sh` downloads GGUF files.
fn models_dir() -> PathBuf {
    Config::wizard_dir()
        .map(|dir| dir.join("models"))
        .unwrap_or_else(|_| PathBuf::from("~/.wizard/models"))
}

/// List `*.gguf` files in `dir`, sorted by name. Empty when the directory is
/// missing or unreadable.
fn existing_ggufs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("gguf"))
        })
        .collect();
    files.sort();
    files
}

/// Model tag for a GGUF path: the filename without the `.gguf` extension
/// (e.g. `/x/Qwen3.6-27B-Q4_K_M.gguf` → `Qwen3.6-27B-Q4_K_M`).
fn gguf_model_tag(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .unwrap_or("default")
        .to_string()
}

/// What the one-click "Local" pick resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalPlan {
    /// llama.cpp serving `gguf_path`; when the file is missing, Wizard
    /// downloads it (and installs llama-server) on first run.
    LlamaCpp { gguf_path: String },
    /// An existing Ollama install that already has `model` pulled.
    Ollama { model: String },
}

/// Decide the one-click local plan. Pure, so it is unit-tested:
/// 1. A GGUF already in `~/.wizard/models` wins — nothing to download
///    (preferring the hardware-suggested tier, else the first by name).
/// 2. Otherwise an Ollama install with at least one model pulled is reused
///    (the hardware-suggested tag when pulled, else the first listed).
/// 3. Otherwise llama.cpp with the suggested tier: Wizard downloads the GGUF
///    and installs llama-server itself on first run.
pub fn plan_local_auto(
    existing: &[PathBuf],
    models_dir: &Path,
    ollama_models: &[String],
    suggested: &GgufModel,
    suggested_tag: &str,
) -> LocalPlan {
    if !existing.is_empty() {
        let chosen = existing
            .iter()
            .find(|path| path.file_name().is_some_and(|name| name == suggested.file))
            .unwrap_or(&existing[0]);
        return LocalPlan::LlamaCpp {
            gguf_path: chosen.display().to_string(),
        };
    }
    if !ollama_models.is_empty() {
        let model = ollama_models
            .iter()
            .find(|model| model.as_str() == suggested_tag)
            .unwrap_or(&ollama_models[0]);
        return LocalPlan::Ollama {
            model: model.clone(),
        };
    }
    LocalPlan::LlamaCpp {
        gguf_path: models_dir.join(suggested.file).display().to_string(),
    }
}

/// Model tags an installed Ollama already has pulled (empty when Ollama is
/// absent, its server is down, or the listing fails).
fn installed_ollama_models() -> Vec<String> {
    if !crate::server::on_path("ollama") {
        return Vec::new();
    }
    let Ok(output) = std::process::Command::new("ollama").arg("list").output() else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .skip(1) // header row
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_string)
        .collect()
}

/// The one-click "Local" pick: no questions. Resolve the plan from what is
/// already on this machine and the hardware suggestion.
fn collect_local_auto() -> ProviderAnswers {
    let (suggested, _) = hardware::suggest_gguf();
    let (suggested_tag, _) = hardware::suggest_model();
    let dir = models_dir();
    let existing = existing_ggufs(&dir);
    match plan_local_auto(
        &existing,
        &dir,
        &installed_ollama_models(),
        suggested,
        &suggested_tag,
    ) {
        LocalPlan::LlamaCpp { gguf_path } => ProviderAnswers {
            provider: ProviderChoice::LlamaCpp,
            provider_name: "local".to_string(),
            kind: ProviderKind::LlamaCpp,
            base_url: LLAMACPP_BASE_URL.to_string(),
            model: gguf_model_tag(&gguf_path),
            api_key_env: None,
            gguf_path: Some(gguf_path),
        },
        LocalPlan::Ollama { model } => ProviderAnswers {
            provider: ProviderChoice::Ollama,
            provider_name: "local".to_string(),
            kind: ProviderKind::Ollama,
            base_url: OLLAMA_BASE_URL.to_string(),
            model,
            api_key_env: None,
            gguf_path: None,
        },
    }
}

fn collect_llamacpp(terminal: &mut Tui) -> Result<Option<ProviderAnswers>> {
    let (suggested, explanation) = hardware::suggest_gguf();
    let dir = models_dir();
    let existing = existing_ggufs(&dir);

    // Each option stands for a GGUF path. GGUFs already on disk come first
    // (so a downloaded model is the default), followed by the tiers the
    // installer knows how to fetch.
    let mut options: Vec<Opt> = Vec::new();
    let mut paths: Vec<String> = Vec::new();
    for path in &existing {
        let label = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_string();
        options.push(Opt::new(label, "found in ~/.wizard/models"));
        paths.push(path.display().to_string());
    }
    for tier in hardware::GGUF_TIERS {
        if existing
            .iter()
            .any(|path| path.file_name().is_some_and(|name| name == tier.file))
        {
            continue; // already listed as a downloaded model
        }
        let detail = if tier.file == suggested.file {
            format!("{} — recommended for this machine", tier.name)
        } else {
            tier.name.to_string()
        };
        options.push(Opt::new(tier.file, detail));
        paths.push(dir.join(tier.file).display().to_string());
    }
    let custom_index = options.len();
    options.push(Opt::new("Type a custom GGUF path…", ""));
    let default = if existing.is_empty() {
        paths
            .iter()
            .position(|path| {
                Path::new(path)
                    .file_name()
                    .is_some_and(|n| n == suggested.file)
            })
            .unwrap_or(0)
    } else {
        0
    };

    let selected = match select(terminal, "Model", &explanation, &options, default)? {
        Some(index) => index,
        None => return Ok(None),
    };
    let gguf_path = if selected == custom_index {
        // Re-prompt until a non-empty path is entered.
        loop {
            let path = match text_input(
                terminal,
                "GGUF path",
                "Absolute path to a .gguf model file.",
                "",
            )? {
                Some(value) => value,
                None => return Ok(None),
            };
            if path.trim().is_empty() {
                notice(terminal, "enter a path to a .gguf file")?;
            } else {
                break path;
            }
        }
    } else {
        paths[selected].clone()
    };

    let base_url = match text_input(
        terminal,
        "llama-server URL",
        "Where llama-server listens. Wizard starts it for you if it isn't running.",
        LLAMACPP_BASE_URL,
    )? {
        Some(value) => value.trim_end_matches('/').to_string(),
        None => return Ok(None),
    };

    Ok(Some(ProviderAnswers {
        provider: ProviderChoice::LlamaCpp,
        provider_name: "local".to_string(),
        kind: ProviderKind::LlamaCpp,
        base_url,
        model: gguf_model_tag(&gguf_path),
        api_key_env: None,
        gguf_path: Some(gguf_path),
    }))
}

fn collect_ollama(terminal: &mut Tui) -> Result<Option<ProviderAnswers>> {
    let (suggested, explanation) = hardware::suggest_model();
    let mut models: Vec<(String, String)> = vec![(
        suggested.clone(),
        "recommended for this machine".to_string(),
    )];
    for tier in OLLAMA_TIERS {
        if *tier != suggested {
            models.push(((*tier).to_string(), String::new()));
        }
    }
    let model = match pick_model(terminal, &explanation, &models, &suggested)? {
        Some(model) => model,
        None => return Ok(None),
    };
    Ok(Some(ProviderAnswers {
        provider: ProviderChoice::Ollama,
        provider_name: "local".to_string(),
        kind: ProviderKind::Ollama,
        base_url: OLLAMA_BASE_URL.to_string(),
        model,
        api_key_env: None,
        gguf_path: None,
    }))
}

fn collect_openai(terminal: &mut Tui) -> Result<Option<ProviderAnswers>> {
    let models: Vec<(String, String)> = OPENAI_MODELS
        .iter()
        .enumerate()
        .map(|(i, m)| {
            (
                (*m).to_string(),
                if i == 0 {
                    "default".to_string()
                } else {
                    String::new()
                },
            )
        })
        .collect();
    let model = match pick_model(
        terminal,
        "OpenAI-compatible model.",
        &models,
        OPENAI_MODELS[0],
    )? {
        Some(model) => model,
        None => return Ok(None),
    };
    let api_key_env = match text_input(
        terminal,
        "API key env var",
        "Wizard reads your key from this env var (never stored on disk).",
        OPENAI_KEY_ENV,
    )? {
        Some(value) => value,
        None => return Ok(None),
    };
    Ok(Some(ProviderAnswers {
        provider: ProviderChoice::Openai,
        provider_name: "openai".to_string(),
        kind: ProviderKind::Openai,
        base_url: OPENAI_BASE_URL.to_string(),
        model,
        api_key_env: Some(api_key_env),
        gguf_path: None,
    }))
}

fn collect_anthropic(terminal: &mut Tui) -> Result<Option<ProviderAnswers>> {
    let models: Vec<(String, String)> = ANTHROPIC_MODELS
        .iter()
        .enumerate()
        .map(|(i, m)| {
            (
                (*m).to_string(),
                if i == 0 {
                    "latest Claude (default)".to_string()
                } else {
                    String::new()
                },
            )
        })
        .collect();
    let model = match pick_model(
        terminal,
        "Anthropic Claude model.",
        &models,
        ANTHROPIC_MODELS[0],
    )? {
        Some(model) => model,
        None => return Ok(None),
    };
    let api_key_env = match text_input(
        terminal,
        "API key env var",
        "Wizard reads your key from this env var (never stored on disk).",
        ANTHROPIC_KEY_ENV,
    )? {
        Some(value) => value,
        None => return Ok(None),
    };
    Ok(Some(ProviderAnswers {
        provider: ProviderChoice::Anthropic,
        provider_name: "claude".to_string(),
        kind: ProviderKind::Anthropic,
        base_url: ANTHROPIC_BASE_URL.to_string(),
        model,
        api_key_env: Some(api_key_env),
        gguf_path: None,
    }))
}

fn collect_openrouter(terminal: &mut Tui) -> Result<Option<ProviderAnswers>> {
    let models: Vec<(String, String)> = vec![(
        OPENROUTER_MODEL.to_string(),
        "Auto Router picks a model per prompt (default)".to_string(),
    )];
    let model = match pick_model(
        terminal,
        "OpenRouter model (any vendor/model tag from openrouter.ai/models).",
        &models,
        OPENROUTER_MODEL,
    )? {
        Some(model) => model,
        None => return Ok(None),
    };
    let api_key_env = match text_input(
        terminal,
        "API key env var",
        "Wizard reads your key from this env var (never stored on disk).",
        OPENROUTER_KEY_ENV,
    )? {
        Some(value) => value,
        None => return Ok(None),
    };
    Ok(Some(ProviderAnswers {
        provider: ProviderChoice::OpenRouter,
        provider_name: "openrouter".to_string(),
        kind: ProviderKind::OpenRouter,
        base_url: OPENROUTER_BASE_URL.to_string(),
        model,
        api_key_env: Some(api_key_env),
        gguf_path: None,
    }))
}

fn collect_xai(terminal: &mut Tui) -> Result<Option<ProviderAnswers>> {
    let models: Vec<(String, String)> = XAI_MODELS
        .iter()
        .enumerate()
        .map(|(i, m)| {
            (
                (*m).to_string(),
                if i == 0 {
                    "default".to_string()
                } else {
                    String::new()
                },
            )
        })
        .collect();
    let model = match pick_model(terminal, "xAI Grok model.", &models, XAI_MODELS[0])? {
        Some(model) => model,
        None => return Ok(None),
    };
    let api_key_env = match text_input(
        terminal,
        "API key env var",
        "Wizard reads your key from this env var (never stored on disk).",
        XAI_KEY_ENV,
    )? {
        Some(value) => value,
        None => return Ok(None),
    };
    Ok(Some(ProviderAnswers {
        provider: ProviderChoice::Xai,
        provider_name: "xai".to_string(),
        kind: ProviderKind::Xai,
        base_url: XAI_BASE_URL.to_string(),
        model,
        api_key_env: Some(api_key_env),
        gguf_path: None,
    }))
}

fn collect_xai_oauth(terminal: &mut Tui) -> Result<Option<ProviderAnswers>> {
    let models: Vec<(String, String)> = XAI_MODELS
        .iter()
        .enumerate()
        .map(|(i, m)| {
            (
                (*m).to_string(),
                if i == 0 {
                    "default".to_string()
                } else {
                    String::new()
                },
            )
        })
        .collect();
    let model = match pick_model(
        terminal,
        "xAI Grok model (sign in with `wizard --login xai` after setup).",
        &models,
        XAI_MODELS[0],
    )? {
        Some(model) => model,
        None => return Ok(None),
    };
    Ok(Some(ProviderAnswers {
        provider: ProviderChoice::XaiOauth,
        provider_name: "xai".to_string(),
        kind: ProviderKind::XaiOauth,
        base_url: XAI_BASE_URL.to_string(),
        model,
        api_key_env: None,
        gguf_path: None,
    }))
}

fn collect_custom(terminal: &mut Tui) -> Result<Option<ProviderAnswers>> {
    let base_url = match text_input(
        terminal,
        "Base URL",
        "OpenAI-compatible endpoint (e.g. http://localhost:8000/v1).",
        OPENAI_BASE_URL,
    )? {
        Some(value) => value,
        None => return Ok(None),
    };
    let model = match text_input(terminal, "Model tag", "The model name to request.", "")? {
        Some(value) => value,
        None => return Ok(None),
    };
    let api_key_env = match text_input(
        terminal,
        "API key env var",
        "Env var holding the key (leave empty if the endpoint needs none).",
        OPENAI_KEY_ENV,
    )? {
        Some(value) => value,
        None => return Ok(None),
    };
    let api_key_env = if api_key_env.trim().is_empty() {
        None
    } else {
        Some(api_key_env)
    };
    Ok(Some(ProviderAnswers {
        provider: ProviderChoice::Custom,
        provider_name: "custom".to_string(),
        kind: ProviderKind::Openai,
        base_url,
        model,
        api_key_env,
        gguf_path: None,
    }))
}

// ---------------------------------------------------------------------------
// Summary
// ---------------------------------------------------------------------------

/// Print a clean plaintext summary plus concrete next steps to stdout, after
/// the alternate screen has been left.
fn print_summary(config: &Config) {
    let provider = config.active();
    let path = Config::path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "~/.wizard/config.toml".to_string());

    println!();
    println!("✓ Wizard is configured.");
    println!();
    println!("  provider : {} ({})", provider.name, provider.kind);
    println!("  model    : {}", provider.model);
    println!("  endpoint : {}", provider.base_url);
    println!("  gateway  : {}", config.gateway.kind);
    println!("  mode     : {}", config.mode);
    println!("  config   : {path}");
    println!();
    println!("Next steps:");

    match provider.kind {
        ProviderKind::LlamaCpp => match provider.gguf_path.as_deref() {
            Some(path) if Path::new(path).exists() => {
                println!("  • llama-server starts automatically (model: {path})");
            }
            Some(path)
                if Path::new(path)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .and_then(hardware::gguf_tier_for_file)
                    .is_some() =>
            {
                println!("  • first run downloads the model and starts llama-server for you");
                println!("    (model: {path})");
            }
            Some(path) => {
                println!("  • download the model to: {path}");
                println!("    (Wizard then starts llama-server automatically)");
            }
            None => {
                println!(
                    "  • start the server: llama-server -m <model.gguf> --port {}",
                    crate::config::DEFAULT_LLAMACPP_PORT
                );
            }
        },
        ProviderKind::Ollama => {
            println!("  • pull the model:  ollama pull {}", provider.model);
        }
        ProviderKind::Openai
        | ProviderKind::Anthropic
        | ProviderKind::OpenRouter
        | ProviderKind::Xai => {
            if let Some(env) = provider.api_key_env.as_deref() {
                println!("  • export your key: export {env}=...");
            }
        }
        ProviderKind::XaiOauth => {
            println!("  • sign in to xAI:  wizard --login xai");
        }
    }

    if config.gateway.kind == GatewayKind::Telegram {
        let env = config.gateway.token_env();
        println!("  • create a bot via @BotFather and export the token:");
        println!("        export {env}=...");
        println!("  • run the gateway: wizard --gateway");
    }

    println!("  • start Wizard:    wizard");
    println!("  • change settings: run /settings anytime inside Wizard");
    println!();
}

// ---------------------------------------------------------------------------
// Terminal lifecycle (mirrors src/app.rs)
// ---------------------------------------------------------------------------

fn setup_terminal() -> Result<Tui> {
    crossterm::terminal::enable_raw_mode().context("enabling raw mode")?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(stdout, crossterm::terminal::EnterAlternateScreen)
        .context("entering alternate screen")?;
    Terminal::new(CrosstermBackend::new(stdout)).context("creating terminal")
}

/// Restore the terminal if (and only if) raw mode is active. Safe on any exit
/// path; idempotent.
fn restore_terminal_best_effort() {
    if crossterm::terminal::is_raw_mode_enabled().unwrap_or(false) {
        let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::LeaveAlternateScreen);
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

// ---------------------------------------------------------------------------
// Step widgets: select list, text input, transient notice
// ---------------------------------------------------------------------------

/// One selectable row.
struct Opt {
    label: String,
    detail: String,
}

impl Opt {
    fn new(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            detail: detail.into(),
        }
    }
}

/// True when `key` is Esc or Ctrl-C — the universal cancel chord.
fn is_cancel(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Esc)
        || (key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')))
}

/// Render a vertical list of options; navigate with ↑/↓, confirm with Enter.
/// Returns the selected index, or `None` on Esc/Ctrl-C.
fn select(
    terminal: &mut Tui,
    title: &str,
    subtitle: &str,
    options: &[Opt],
    default: usize,
) -> Result<Option<usize>> {
    let mut selected = default.min(options.len().saturating_sub(1));
    loop {
        terminal.draw(|frame| draw_select(frame, title, subtitle, options, selected))?;
        let Some(key) = next_key()? else { continue };
        if is_cancel(&key) {
            return Ok(None);
        }
        match key.code {
            KeyCode::Up | KeyCode::BackTab | KeyCode::Char('k') => {
                selected = if selected == 0 {
                    options.len().saturating_sub(1)
                } else {
                    selected - 1
                };
            }
            KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') => {
                selected = if selected + 1 >= options.len() {
                    0
                } else {
                    selected + 1
                };
            }
            KeyCode::Enter => return Ok(Some(selected)),
            _ => {}
        }
    }
}

/// Render a checklist of options; ↑/↓ move, Space toggles the current row,
/// Enter confirms. Returns the per-row checked state, or `None` on Esc/Ctrl-C.
/// All rows start unchecked.
fn multi_select(
    terminal: &mut Tui,
    title: &str,
    subtitle: &str,
    options: &[Opt],
) -> Result<Option<Vec<bool>>> {
    let mut checked = vec![false; options.len()];
    let mut selected = 0usize;
    loop {
        terminal
            .draw(|frame| draw_multi_select(frame, title, subtitle, options, &checked, selected))?;
        let Some(key) = next_key()? else { continue };
        if is_cancel(&key) {
            return Ok(None);
        }
        match key.code {
            KeyCode::Up | KeyCode::BackTab | KeyCode::Char('k') => {
                selected = if selected == 0 {
                    options.len().saturating_sub(1)
                } else {
                    selected - 1
                };
            }
            KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') => {
                selected = if selected + 1 >= options.len() {
                    0
                } else {
                    selected + 1
                };
            }
            KeyCode::Char(' ') => {
                if let Some(slot) = checked.get_mut(selected) {
                    *slot = !*slot;
                }
            }
            KeyCode::Enter => return Ok(Some(checked)),
            _ => {}
        }
    }
}

/// Free-text input step. Enter accepts (empty submits the default); Esc/Ctrl-C
/// cancels. Returns the entered (or default) value.
fn text_input(
    terminal: &mut Tui,
    title: &str,
    subtitle: &str,
    default: &str,
) -> Result<Option<String>> {
    let mut buffer = String::new();
    loop {
        terminal.draw(|frame| draw_input(frame, title, subtitle, &buffer, default))?;
        let Some(key) = next_key()? else { continue };
        if is_cancel(&key) {
            return Ok(None);
        }
        match key.code {
            KeyCode::Enter => {
                let value = if buffer.trim().is_empty() {
                    default.to_string()
                } else {
                    buffer.trim().to_string()
                };
                return Ok(Some(value));
            }
            KeyCode::Backspace => {
                buffer.pop();
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                buffer.push(c);
            }
            _ => {}
        }
    }
}

/// Show a transient message until the user presses a key (used for validation
/// errors). Always returns once a key is read.
fn notice(terminal: &mut Tui, message: &str) -> Result<()> {
    loop {
        terminal.draw(|frame| draw_notice(frame, message))?;
        if let Some(key) = next_key()?
            && (is_cancel(&key) || matches!(key.code, KeyCode::Enter | KeyCode::Char(_)))
        {
            return Ok(());
        }
    }
}

/// Block until the next key *press* (ignoring releases), polling so the draw
/// loop stays responsive. `None` means "nothing yet, redraw".
fn next_key() -> Result<Option<KeyEvent>> {
    if event::poll(Duration::from_millis(150)).context("polling terminal events")?
        && let Event::Key(key) = event::read().context("reading terminal event")?
        && key.kind != KeyEventKind::Release
    {
        return Ok(Some(key));
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Compose the outer frame (header + bordered body + footer) and return the
/// inner content area for the step to fill.
fn frame_body(frame: &mut ratatui::Frame, title: &str, subtitle: &str, footer: &str) -> Rect {
    let area = frame.area();
    let [header, body, foot] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(area);

    let header_lines = Text::from(vec![
        Line::from(Span::styled(
            format!("  {title}"),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!("  {subtitle}"),
            Style::default().fg(TEXT_DIM),
        )),
    ]);
    frame.render_widget(Paragraph::new(header_lines), header);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(DIM))
        .title(Span::styled(" wizard setup ", Style::default().fg(DIM)));
    let inner = block.inner(body);
    frame.render_widget(block, body);

    frame.render_widget(
        Paragraph::new(Span::styled(
            format!("  {footer}"),
            Style::default().fg(DIM),
        )),
        foot,
    );
    inner
}

fn draw_select(
    frame: &mut ratatui::Frame,
    title: &str,
    subtitle: &str,
    options: &[Opt],
    selected: usize,
) {
    let inner = frame_body(
        frame,
        title,
        subtitle,
        "↑/↓ move · enter select · esc cancel",
    );
    let mut lines = Vec::with_capacity(options.len());
    for (index, option) in options.iter().enumerate() {
        let active = index == selected;
        let marker = if active { "▸ " } else { "  " };
        let label_style = if active {
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(TEXT_DIM)
        };
        let mut spans = vec![
            Span::styled(format!(" {marker}"), Style::default().fg(ACCENT)),
            Span::styled(option.label.clone(), label_style),
        ];
        if !option.detail.is_empty() {
            spans.push(Span::styled(
                format!("   {}", option.detail),
                Style::default().fg(DIM),
            ));
        }
        lines.push(Line::from(spans));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

fn draw_multi_select(
    frame: &mut ratatui::Frame,
    title: &str,
    subtitle: &str,
    options: &[Opt],
    checked: &[bool],
    selected: usize,
) {
    let inner = frame_body(
        frame,
        title,
        subtitle,
        "↑/↓ move · space toggle · enter confirm · esc skip",
    );
    let mut lines = Vec::with_capacity(options.len());
    for (index, option) in options.iter().enumerate() {
        let active = index == selected;
        let marker = if active { "▸ " } else { "  " };
        let box_ = if checked.get(index).copied().unwrap_or(false) {
            "[x]"
        } else {
            "[ ]"
        };
        let label_style = if active {
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(TEXT_DIM)
        };
        let mut spans = vec![
            Span::styled(format!(" {marker}"), Style::default().fg(ACCENT)),
            Span::styled(format!("{box_} "), Style::default().fg(ACCENT)),
            Span::styled(option.label.clone(), label_style),
        ];
        if !option.detail.is_empty() {
            spans.push(Span::styled(
                format!("   {}", option.detail),
                Style::default().fg(DIM),
            ));
        }
        lines.push(Line::from(spans));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

fn draw_input(
    frame: &mut ratatui::Frame,
    title: &str,
    subtitle: &str,
    buffer: &str,
    default: &str,
) {
    let inner = frame_body(frame, title, subtitle, "enter accept · esc cancel");
    let shown = if buffer.is_empty() {
        Span::styled(
            if default.is_empty() {
                "  (type a value)".to_string()
            } else {
                format!("  {default}")
            },
            Style::default().fg(DIM),
        )
    } else {
        Span::styled(format!("  {buffer}"), Style::default().fg(ACCENT))
    };
    let mut lines = vec![Line::from(vec![
        Span::styled(" ▸ ", Style::default().fg(ACCENT)),
        shown,
        Span::styled("▏", Style::default().fg(ACCENT)),
    ])];
    if !default.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("   default: {default}"),
            Style::default().fg(DIM),
        )));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

fn draw_notice(frame: &mut ratatui::Frame, message: &str) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::White))
        .title(Span::styled(
            " notice ",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                format!("  {message}"),
                Style::default().fg(TEXT_DIM),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  press any key to continue",
                Style::default().fg(DIM),
            )),
        ])
        .alignment(Alignment::Left),
        inner,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_answers() -> Answers {
        Answers {
            provider: ProviderChoice::Ollama,
            provider_name: "local".to_string(),
            kind: ProviderKind::Ollama,
            base_url: OLLAMA_BASE_URL.to_string(),
            model: "qwen3.6:27b".to_string(),
            api_key_env: None,
            gguf_path: None,
            gateway_kind: GatewayKind::None,
            gateway_token_env: None,
            gateway_allowed_chat_ids: Vec::new(),
            mode: Mode::Genie,
            claude_import: None,
        }
    }

    #[test]
    fn ollama_answers_mirror_legacy_fields() {
        let answers = Answers {
            base_url: "http://10.0.0.5:11434".to_string(),
            model: "qwen3.5:9b".to_string(),
            ..base_answers()
        };
        let config = answers.into_config();
        assert_eq!(config.providers.len(), 1);
        assert_eq!(config.active_provider.as_deref(), Some("local"));
        assert_eq!(config.active().kind, ProviderKind::Ollama);
        assert_eq!(config.active().model, "qwen3.5:9b");
        // Legacy fields mirror the Ollama choice for back-compat.
        assert_eq!(config.model, "qwen3.5:9b");
        assert_eq!(config.ollama_host, "http://10.0.0.5:11434");
        assert_eq!(config.gateway.kind, GatewayKind::None);
        assert_eq!(config.mode, Mode::Genie);
    }

    #[test]
    fn llamacpp_answers_carry_gguf_and_skip_legacy_ollama_fields() {
        let answers = Answers {
            provider: ProviderChoice::LlamaCpp,
            kind: ProviderKind::LlamaCpp,
            base_url: "http://127.0.0.1:9090".to_string(),
            model: "Qwen3.6-27B-Q4_K_M".to_string(),
            gguf_path: Some("/home/u/.wizard/models/Qwen3.6-27B-Q4_K_M.gguf".to_string()),
            ..base_answers()
        };
        let defaults = Config::default();
        let config = answers.into_config();
        assert_eq!(config.active().kind, ProviderKind::LlamaCpp);
        assert_eq!(config.active().model, "Qwen3.6-27B-Q4_K_M");
        assert_eq!(
            config.active().gguf_path.as_deref(),
            Some("/home/u/.wizard/models/Qwen3.6-27B-Q4_K_M.gguf")
        );
        // Top-level llamacpp fields mirror the choice…
        assert_eq!(config.llamacpp_host, "http://127.0.0.1:9090");
        assert_eq!(config.gguf_path, config.active().gguf_path);
        // …while the legacy Ollama fields stay at their defaults.
        assert_eq!(config.model, defaults.model);
        assert_eq!(config.ollama_host, defaults.ollama_host);
    }

    #[test]
    fn gguf_model_tag_strips_directory_and_extension() {
        assert_eq!(
            gguf_model_tag("/home/u/.wizard/models/Qwen3.5-9B-Q4_K_M.gguf"),
            "Qwen3.5-9B-Q4_K_M"
        );
        assert_eq!(gguf_model_tag("model.gguf"), "model");
        assert_eq!(gguf_model_tag(""), "default");
    }

    #[test]
    fn existing_ggufs_lists_only_gguf_files_sorted() {
        let dir = tempfile::tempdir().expect("tempdir");
        for name in ["b.gguf", "a.GGUF", "notes.txt"] {
            std::fs::write(dir.path().join(name), b"x").expect("write");
        }
        std::fs::create_dir(dir.path().join("sub.gguf")).expect("mkdir");
        let found = existing_ggufs(dir.path());
        let names: Vec<_> = found
            .iter()
            .filter_map(|p| p.file_name().and_then(|n| n.to_str()))
            .collect();
        assert_eq!(names, vec!["a.GGUF", "b.gguf"]);
        // Missing directory → empty, not an error.
        assert!(existing_ggufs(&dir.path().join("missing")).is_empty());
    }

    #[test]
    fn cloud_answers_do_not_touch_legacy_ollama_fields() {
        let answers = Answers {
            provider: ProviderChoice::Anthropic,
            provider_name: "claude".to_string(),
            kind: ProviderKind::Anthropic,
            base_url: ANTHROPIC_BASE_URL.to_string(),
            model: "claude-fable-5".to_string(),
            api_key_env: Some(ANTHROPIC_KEY_ENV.to_string()),
            mode: Mode::Sovereign,
            ..base_answers()
        };
        let defaults = Config::default();
        let config = answers.into_config();
        assert_eq!(config.active().name, "claude");
        assert_eq!(config.active().kind, ProviderKind::Anthropic);
        assert_eq!(
            config.active().api_key_env.as_deref(),
            Some(ANTHROPIC_KEY_ENV)
        );
        // Legacy fields untouched (still defaults) since this isn't an Ollama choice.
        assert_eq!(config.model, defaults.model);
        assert_eq!(config.ollama_host, defaults.ollama_host);
        assert_eq!(config.mode, Mode::Sovereign);
    }

    #[test]
    fn xai_answers_build_the_expected_providers() {
        // API-key flavor.
        let answers = Answers {
            provider: ProviderChoice::Xai,
            provider_name: "xai".to_string(),
            kind: ProviderKind::Xai,
            base_url: XAI_BASE_URL.to_string(),
            model: "grok-4.3".to_string(),
            api_key_env: Some(XAI_KEY_ENV.to_string()),
            ..base_answers()
        };
        let config = answers.into_config();
        assert_eq!(config.active().name, "xai");
        assert_eq!(config.active().kind, ProviderKind::Xai);
        assert_eq!(config.active().base_url, "https://api.x.ai/v1");
        assert_eq!(config.active().api_key_env.as_deref(), Some("XAI_API_KEY"));

        // OAuth flavor: no API key env; credentials come from the token file.
        let answers = Answers {
            provider: ProviderChoice::XaiOauth,
            provider_name: "xai".to_string(),
            kind: ProviderKind::XaiOauth,
            base_url: XAI_BASE_URL.to_string(),
            model: "grok-4.3".to_string(),
            api_key_env: None,
            ..base_answers()
        };
        let config = answers.into_config();
        assert_eq!(config.active().kind, ProviderKind::XaiOauth);
        assert!(config.active().api_key_env.is_none());
        // Legacy Ollama fields stay untouched for cloud choices.
        let defaults = Config::default();
        assert_eq!(config.model, defaults.model);
        assert_eq!(config.ollama_host, defaults.ollama_host);
    }

    #[test]
    fn openrouter_answers_build_the_expected_provider() {
        let answers = Answers {
            provider: ProviderChoice::OpenRouter,
            provider_name: "openrouter".to_string(),
            kind: ProviderKind::OpenRouter,
            base_url: OPENROUTER_BASE_URL.to_string(),
            model: OPENROUTER_MODEL.to_string(),
            api_key_env: Some(OPENROUTER_KEY_ENV.to_string()),
            ..base_answers()
        };
        let config = answers.into_config();
        assert_eq!(config.active().name, "openrouter");
        assert_eq!(config.active().kind, ProviderKind::OpenRouter);
        assert_eq!(config.active().base_url, "https://openrouter.ai/api/v1");
        assert_eq!(config.active().model, "openrouter/auto");
        assert_eq!(
            config.active().api_key_env.as_deref(),
            Some("OPENROUTER_API_KEY")
        );
        // Legacy Ollama fields stay untouched for cloud choices.
        let defaults = Config::default();
        assert_eq!(config.model, defaults.model);
        assert_eq!(config.ollama_host, defaults.ollama_host);
    }

    #[test]
    fn telegram_gateway_persists_into_config() {
        let answers = Answers {
            gateway_kind: GatewayKind::Telegram,
            gateway_token_env: Some("MY_TOKEN".to_string()),
            gateway_allowed_chat_ids: vec![1, 2, 3],
            ..base_answers()
        };
        let config = answers.into_config();
        assert_eq!(config.gateway.kind, GatewayKind::Telegram);
        assert_eq!(config.gateway.token_env.as_deref(), Some("MY_TOKEN"));
        assert_eq!(config.gateway.allowed_chat_ids, vec![1, 2, 3]);
    }

    #[test]
    fn parse_chat_ids_handles_empty_and_whitespace() {
        assert_eq!(parse_chat_ids("").unwrap(), Vec::<i64>::new());
        assert_eq!(parse_chat_ids("   ").unwrap(), Vec::<i64>::new());
        assert_eq!(parse_chat_ids(" , , ").unwrap(), Vec::<i64>::new());
    }

    #[test]
    fn parse_chat_ids_parses_numbers_including_negative() {
        assert_eq!(parse_chat_ids("42").unwrap(), vec![42]);
        assert_eq!(
            parse_chat_ids("42, -100123 , 7").unwrap(),
            vec![42, -100123, 7]
        );
    }

    #[test]
    fn parse_chat_ids_rejects_non_numeric() {
        let err = parse_chat_ids("42, abc").expect_err("non-numeric must error");
        assert!(
            err.contains("abc"),
            "error should name the bad token: {err}"
        );
    }

    #[test]
    fn anthropic_default_model_is_latest_claude() {
        assert_eq!(ANTHROPIC_MODELS[0], "claude-fable-5");
    }

    fn tier(file: &'static str) -> GgufModel {
        GgufModel {
            name: "Test",
            file,
            url: "https://example.com/x.gguf",
        }
    }

    #[test]
    fn local_plan_prefers_a_downloaded_gguf() {
        let dir = Path::new("/m");
        let existing = vec![PathBuf::from("/m/a.gguf"), PathBuf::from("/m/big.gguf")];
        // The suggested tier is on disk: it wins over the first-by-name file.
        let plan = plan_local_auto(
            &existing,
            dir,
            &["qwen3.5:9b".to_string()],
            &tier("big.gguf"),
            "qwen3.5:9b",
        );
        assert_eq!(
            plan,
            LocalPlan::LlamaCpp {
                gguf_path: "/m/big.gguf".to_string()
            }
        );
        // Suggested tier not on disk: first existing GGUF wins, still no
        // download and still ahead of any Ollama install.
        let plan = plan_local_auto(
            &existing,
            dir,
            &["qwen3.5:9b".to_string()],
            &tier("other.gguf"),
            "qwen3.5:9b",
        );
        assert_eq!(
            plan,
            LocalPlan::LlamaCpp {
                gguf_path: "/m/a.gguf".to_string()
            }
        );
    }

    #[test]
    fn local_plan_reuses_an_ollama_install_with_models() {
        let dir = Path::new("/m");
        let pulled = vec!["llama3:8b".to_string(), "qwen3.5:9b".to_string()];
        // The hardware-suggested tag is pulled: use it.
        let plan = plan_local_auto(&[], dir, &pulled, &tier("x.gguf"), "qwen3.5:9b");
        assert_eq!(
            plan,
            LocalPlan::Ollama {
                model: "qwen3.5:9b".to_string()
            }
        );
        // Suggested tag not pulled: first listed model.
        let plan = plan_local_auto(&[], dir, &pulled, &tier("x.gguf"), "qwen3.6:27b");
        assert_eq!(
            plan,
            LocalPlan::Ollama {
                model: "llama3:8b".to_string()
            }
        );
    }

    #[test]
    fn local_plan_falls_back_to_a_fresh_llamacpp_download() {
        let plan = plan_local_auto(&[], Path::new("/m"), &[], &tier("big.gguf"), "qwen3.5:9b");
        assert_eq!(
            plan,
            LocalPlan::LlamaCpp {
                gguf_path: "/m/big.gguf".to_string()
            }
        );
    }
}
