//! User configuration: `~/.wizard/config.toml` plus env overrides and
//! well-known paths under `~/.wizard/` (see "Data on disk" in
//! `docs/architecture.md`).

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::cli::Cli;
use crate::llm::anthropic::AnthropicProvider;
use crate::llm::llamacpp::LlamaCppProvider;
use crate::llm::ollama::OllamaClient;
use crate::llm::openai::{OpenAiProvider, StaticToken};
use crate::llm::provider::LlmProvider;
use crate::llm::xai_oauth;

/// Personality mode. Shares tools and model; differs in prompting,
/// temperature, step budget, and confirmation behavior (`docs/modes.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
#[clap(rename_all = "lowercase")]
pub enum Mode {
    /// Interactive TUI. Bypass-permissions: auto-approves tool calls and acts
    /// without per-action prompts.
    #[default]
    Genie,
    /// Autonomous agent. Works continuously without human intervention;
    /// self-directing and self-improving.
    Sovereign,
}

impl Mode {
    /// Sampling temperature for this mode (genie 0.8, sovereign 0.6).
    pub fn temperature(self) -> f32 {
        match self {
            Mode::Genie => 0.8,
            Mode::Sovereign => 0.6,
        }
    }

    /// Default agent-loop step budget per turn (genie 25, sovereign 100).
    pub fn default_max_steps(self) -> u32 {
        match self {
            Mode::Genie => 25,
            Mode::Sovereign => 100,
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Mode::Genie => write!(f, "genie"),
            Mode::Sovereign => write!(f, "sovereign"),
        }
    }
}

/// Which backend a [`ProviderConfig`] talks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
#[clap(rename_all = "lowercase")]
pub enum ProviderKind {
    /// Local llama.cpp `llama-server` (OpenAI-compatible `/v1` API plus the
    /// native `/health` probe). The default local backend.
    LlamaCpp,
    /// Local Ollama server (native `/api/chat`).
    Ollama,
    /// OpenAI-compatible Chat Completions endpoint (OpenAI, OpenRouter, Groq,
    /// together.ai, vLLM, LM Studio, ...).
    Openai,
    /// Anthropic Messages API.
    Anthropic,
    /// xAI (Grok) Chat Completions at `https://api.x.ai/v1` with a plain API
    /// key (default env var `XAI_API_KEY`).
    Xai,
    /// xAI via account sign-in: OAuth tokens from `wizard --login xai`
    /// (stored in `~/.wizard/xai_oauth.json`), no API key needed.
    XaiOauth,
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProviderKind::LlamaCpp => write!(f, "llamacpp"),
            ProviderKind::Ollama => write!(f, "ollama"),
            ProviderKind::Openai => write!(f, "openai"),
            ProviderKind::Anthropic => write!(f, "anthropic"),
            ProviderKind::Xai => write!(f, "xai"),
            ProviderKind::XaiOauth => write!(f, "xaioauth"),
        }
    }
}

/// Which messaging gateway, if any, Wizard exposes (`wizard --gateway`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
#[clap(rename_all = "lowercase")]
pub enum GatewayKind {
    /// No gateway — terminal only.
    #[default]
    None,
    /// Telegram bot (long-poll `getUpdates` / `sendMessage`).
    Telegram,
}

impl fmt::Display for GatewayKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GatewayKind::None => write!(f, "none"),
            GatewayKind::Telegram => write!(f, "telegram"),
        }
    }
}

/// Configuration for the optional messaging gateway. Bot tokens are never
/// stored here — only the name of the environment variable that holds the
/// token (`token_env`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct GatewayConfig {
    /// Which gateway to run (default [`GatewayKind::None`]).
    #[serde(default)]
    pub kind: GatewayKind,
    /// Name of the env var holding the bot token (default
    /// `WIZARD_TELEGRAM_TOKEN`); the token itself is never persisted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_env: Option<String>,
    /// Allowed inbound chat IDs. Empty means "allow all".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_chat_ids: Vec<i64>,
}

impl GatewayConfig {
    /// Default name of the env var holding a Telegram bot token.
    pub const DEFAULT_TOKEN_ENV: &'static str = "WIZARD_TELEGRAM_TOKEN";

    /// The env var name to read the bot token from, falling back to
    /// [`Self::DEFAULT_TOKEN_ENV`] when unset.
    pub fn token_env(&self) -> &str {
        self.token_env.as_deref().unwrap_or(Self::DEFAULT_TOKEN_ENV)
    }
}

/// A named LLM provider. Cloud keys are never stored here — only the name of
/// the environment variable holding the key (`api_key_env`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Unique id, e.g. `"local"`, `"openai"`, `"claude"`.
    pub name: String,
    /// Backend kind.
    pub kind: ProviderKind,
    /// Base URL: llamacpp `http://127.0.0.1:8080`; ollama
    /// `http://127.0.0.1:11434`; openai `https://api.openai.com/v1`;
    /// anthropic `https://api.anthropic.com`; xai / xaioauth
    /// `https://api.x.ai/v1`.
    pub base_url: String,
    /// Model tag.
    pub model: String,
    /// Name of the env var holding the API key (cloud only); the key itself is
    /// never persisted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Path to the GGUF model file (llamacpp only) — used when Wizard spawns
    /// `llama-server` itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gguf_path: Option<String>,
}

impl ProviderConfig {
    /// Read the API key from `api_key_env`, or empty when unset/missing.
    fn api_key(&self) -> String {
        self.api_key_env
            .as_ref()
            .and_then(|name| std::env::var(name).ok())
            .unwrap_or_default()
    }

    /// Like [`api_key`](Self::api_key) but with a per-kind default env var
    /// when `api_key_env` is unset, warning when the key is missing.
    fn cloud_key(&self, default_env: &str) -> String {
        let env_name = self.api_key_env.as_deref().unwrap_or(default_env);
        let key = std::env::var(env_name).unwrap_or_default();
        if key.is_empty() {
            tracing::warn!(
                "provider '{}' has no API key (set {env_name}); requests will likely 401",
                self.name,
            );
        }
        key
    }

    /// Construct the concrete client for this provider. For cloud kinds a
    /// missing key is a soft warning (the client is still built so `health()`
    /// can report the real error).
    pub fn build(&self) -> Result<Arc<dyn LlmProvider>> {
        match self.kind {
            ProviderKind::LlamaCpp => Ok(Arc::new(LlamaCppProvider::new(
                self.base_url.clone(),
                self.model.clone(),
            ))),
            ProviderKind::Ollama => Ok(Arc::new(OllamaClient::new(self.base_url.clone()))),
            ProviderKind::Openai => {
                let key = self.api_key();
                if key.is_empty() {
                    tracing::warn!(
                        "provider '{}' has no API key (set {}); requests will likely 401",
                        self.name,
                        self.api_key_env.as_deref().unwrap_or("an env var")
                    );
                }
                Ok(Arc::new(OpenAiProvider::new(
                    self.base_url.clone(),
                    self.model.clone(),
                    key,
                )))
            }
            ProviderKind::Anthropic => {
                let key = self.api_key();
                if key.is_empty() {
                    tracing::warn!(
                        "provider '{}' has no API key (set {}); requests will likely 401",
                        self.name,
                        self.api_key_env.as_deref().unwrap_or("an env var")
                    );
                }
                Ok(Arc::new(AnthropicProvider::new(
                    self.base_url.clone(),
                    self.model.clone(),
                    key,
                )))
            }
            // xAI speaks the OpenAI-compatible Chat Completions API; only the
            // credentials differ between the two kinds.
            ProviderKind::Xai => {
                let key = self.cloud_key(xai_oauth::DEFAULT_KEY_ENV);
                Ok(Arc::new(OpenAiProvider::with_token_source(
                    self.base_url.clone(),
                    self.model.clone(),
                    Arc::new(StaticToken::new(key)),
                    "xai",
                )))
            }
            ProviderKind::XaiOauth => {
                let source = xai_oauth::XaiTokenSource::new()
                    .context("setting up xAI OAuth token storage")?;
                Ok(Arc::new(OpenAiProvider::with_token_source(
                    self.base_url.clone(),
                    self.model.clone(),
                    Arc::new(source),
                    "xai",
                )))
            }
        }
    }
}

/// Contents of `~/.wizard/config.toml`. Unknown keys are ignored; missing
/// keys take the documented defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Model tag for the synthesized local provider (default `qwen3.6:27b`).
    pub model: String,
    /// Base URL of the Ollama server — only used by explicitly configured
    /// Ollama providers; the synthesized local default is always llama.cpp.
    pub ollama_host: String,
    /// Base URL of the local llama.cpp `llama-server` — feeds the synthesized
    /// default provider when `providers` is empty.
    pub llamacpp_host: String,
    /// Path to the GGUF model file for the synthesized llama.cpp provider —
    /// used when Wizard spawns `llama-server` itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gguf_path: Option<String>,
    /// Default personality mode.
    pub mode: Mode,
    /// Bypass per-action confirmation prompts. Default true: genie bypasses
    /// permissions. Set false to restore the y/n gate for tools that request
    /// approval.
    pub auto_approve: bool,
    /// Agent loop limit per turn (genie). Sovereign uses its own default
    /// unless this is explicitly raised above it.
    pub max_steps: u32,
    /// Perpetual sovereign operation: keep working/self-directing/self-improving
    /// until stopped.
    pub continuous: bool,
    /// Base seconds for exponential backoff when the LLM server is unreachable
    /// or rate-limited.
    pub retry_base_secs: u64,
    /// Cap on backoff sleep in seconds.
    pub retry_max_secs: u64,
    /// Pause between continuous cycles (0 = none).
    pub cycle_pause_secs: u64,
    /// When the serialized chat history exceeds this many bytes, compact older
    /// messages into a summary.
    pub compact_threshold_bytes: usize,
    /// Configured LLM providers. Empty means "use the legacy `model` /
    /// `ollama_host` fields as a single local Ollama provider".
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    /// Name of the active provider in [`providers`](Self::providers). `None`
    /// (or an unknown name) selects the first configured provider.
    #[serde(default)]
    pub active_provider: Option<String>,
    /// Optional messaging gateway (`wizard --gateway`). Defaults to
    /// [`GatewayKind::None`] — terminal only.
    #[serde(default)]
    pub gateway: GatewayConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: "qwen3.6:27b".to_string(),
            ollama_host: "http://127.0.0.1:11434".to_string(),
            llamacpp_host: "http://127.0.0.1:8080".to_string(),
            gguf_path: None,
            mode: Mode::Genie,
            auto_approve: true,
            max_steps: 25,
            continuous: false,
            retry_base_secs: 5,
            retry_max_secs: 300,
            cycle_pause_secs: 0,
            compact_threshold_bytes: 48_000,
            providers: Vec::new(),
            active_provider: None,
            gateway: GatewayConfig::default(),
        }
    }
}

impl Config {
    /// `~/.wizard` — root of all Wizard state on disk.
    pub fn wizard_dir() -> Result<PathBuf> {
        let home = dirs::home_dir().context("could not determine home directory")?;
        Ok(home.join(".wizard"))
    }

    /// `~/.wizard/config.toml`
    pub fn path() -> Result<PathBuf> {
        Ok(Self::wizard_dir()?.join("config.toml"))
    }

    /// `~/.wizard/mcp.toml` — MCP server declarations.
    pub fn mcp_config_path() -> Result<PathBuf> {
        Ok(Self::wizard_dir()?.join("mcp.toml"))
    }

    /// `~/.wizard/sessions/` — JSONL chat history.
    pub fn sessions_dir() -> Result<PathBuf> {
        Ok(Self::wizard_dir()?.join("sessions"))
    }

    /// `~/.wizard/tools/` — agent-authored scripted tools.
    pub fn scripted_tools_dir() -> Result<PathBuf> {
        Ok(Self::wizard_dir()?.join("tools"))
    }

    /// `~/.wizard/skills/` — user/evolved skills (in addition to bundled ones).
    pub fn skills_dir() -> Result<PathBuf> {
        Ok(Self::wizard_dir()?.join("skills"))
    }

    /// `~/.wizard/subagents/` — user-defined subagent definitions (TOML).
    pub fn subagents_dir() -> Result<PathBuf> {
        Ok(Self::wizard_dir()?.join("subagents"))
    }

    /// `~/.wizard/src/` — source checkout for deep evolve.
    pub fn source_dir() -> Result<PathBuf> {
        Ok(Self::wizard_dir()?.join("src"))
    }

    /// `~/.wizard/memory/` — persistent per-project memory
    /// (`crate::memory::MemoryStore`).
    pub fn memory_dir() -> Result<PathBuf> {
        Ok(Self::wizard_dir()?.join("memory"))
    }

    /// `~/.wizard/evolution.jsonl` — self-extension log.
    pub fn evolution_log_path() -> Result<PathBuf> {
        Ok(Self::wizard_dir()?.join("evolution.jsonl"))
    }

    /// `~/.wizard/logs/` — debug traces.
    pub fn logs_dir() -> Result<PathBuf> {
        Ok(Self::wizard_dir()?.join("logs"))
    }

    /// Create the `~/.wizard` directory tree (sessions, tools, skills, logs)
    /// if it does not exist yet. Idempotent; called on every load so a fresh
    /// install is usable without running the installer.
    pub fn ensure_dirs() -> Result<()> {
        for dir in [
            Self::wizard_dir()?,
            Self::sessions_dir()?,
            Self::scripted_tools_dir()?,
            Self::skills_dir()?,
            Self::subagents_dir()?,
            Self::memory_dir()?,
            Self::logs_dir()?,
        ] {
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        Ok(())
    }

    /// Load config from disk, falling back to defaults when the file is
    /// missing, then apply env overrides (`WIZARD_MODEL`,
    /// `WIZARD_OLLAMA_HOST`, `WIZARD_LLAMACPP_HOST`, `WIZARD_GGUF_PATH`).
    /// Creates the `~/.wizard` directory tree on first run.
    pub fn load() -> Result<Self> {
        Self::ensure_dirs()?;

        let path = Self::path()?;
        let mut config = if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            Self::from_toml(&raw).with_context(|| format!("parsing {}", path.display()))?
        } else {
            Self::default()
        };
        config.apply_env();

        Ok(config)
    }

    /// Parse a config file. Unknown keys are ignored; missing keys take the
    /// documented defaults. Ollama-era files (`model` / `ollama_host` only)
    /// parse fine but synthesize a llama.cpp provider like everything else —
    /// Ollama is opt-in via an explicit `[[providers]]` entry.
    fn from_toml(raw: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(raw)
    }

    /// The effective active provider. When [`providers`](Self::providers) is
    /// non-empty, returns the one named by
    /// [`active_provider`](Self::active_provider) (or the first if unset or
    /// unknown). Otherwise synthesizes a local llama.cpp provider from
    /// `model` / `llamacpp_host` / `gguf_path`.
    pub fn active(&self) -> ProviderConfig {
        if !self.providers.is_empty() {
            let chosen = self
                .active_provider
                .as_ref()
                .and_then(|name| self.providers.iter().find(|p| &p.name == name))
                .or_else(|| self.providers.first());
            if let Some(provider) = chosen {
                return provider.clone();
            }
        }
        ProviderConfig {
            name: "local".to_string(),
            kind: ProviderKind::LlamaCpp,
            base_url: self.llamacpp_host.clone(),
            model: self.model.clone(),
            api_key_env: None,
            gguf_path: self.gguf_path.clone(),
        }
    }

    /// Index of the effective active provider in [`providers`](Self::providers),
    /// when any are configured.
    fn active_index(&self) -> Option<usize> {
        if self.providers.is_empty() {
            return None;
        }
        Some(
            self.active_provider
                .as_ref()
                .and_then(|name| self.providers.iter().position(|p| &p.name == name))
                .unwrap_or(0),
        )
    }

    /// Apply environment-variable overrides on top of file/default config.
    /// Empty values are ignored.
    fn apply_env(&mut self) {
        self.apply_env_from(|name| std::env::var(name).ok());
    }

    /// Testable core of [`apply_env`]: `lookup` supplies the value of an
    /// environment variable, or `None` when unset.
    ///
    /// `WIZARD_MODEL` overrides the legacy `model` field and, when providers
    /// are explicitly configured, the active provider's model too;
    /// `WIZARD_OLLAMA_HOST` overrides `ollama_host` (used by explicitly
    /// configured Ollama providers); `WIZARD_LLAMACPP_HOST` overrides
    /// `llamacpp_host`; `WIZARD_GGUF_PATH` overrides `gguf_path` and, when
    /// the active provider is llamacpp, its `gguf_path` too.
    fn apply_env_from(&mut self, lookup: impl Fn(&str) -> Option<String>) {
        if let Some(model) = lookup("WIZARD_MODEL")
            && !model.trim().is_empty()
        {
            let model = model.trim().to_string();
            self.model = model.clone();
            if let Some(index) = self.active_index() {
                self.providers[index].model = model;
            }
        }
        if let Some(host) = lookup("WIZARD_OLLAMA_HOST") {
            let host = host.trim().trim_end_matches('/');
            if !host.is_empty() {
                self.ollama_host = host.to_string();
            }
        }
        if let Some(host) = lookup("WIZARD_LLAMACPP_HOST") {
            let host = host.trim().trim_end_matches('/');
            if !host.is_empty() {
                self.llamacpp_host = host.to_string();
            }
        }
        if let Some(path) = lookup("WIZARD_GGUF_PATH") {
            let path = path.trim();
            if !path.is_empty() {
                self.gguf_path = Some(path.to_string());
                if let Some(index) = self.active_index()
                    && self.providers[index].kind == ProviderKind::LlamaCpp
                {
                    self.providers[index].gguf_path = Some(path.to_string());
                }
            }
        }
    }

    /// Persist config to `~/.wizard/config.toml`, creating the directory if
    /// needed.
    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let raw = toml::to_string_pretty(self).context("serializing config")?;
        std::fs::write(&path, raw).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// Apply CLI flag overrides on top of file/env config for this run.
    /// CLI mode wins; `--auto` forces `auto_approve`; sovereign mode raises
    /// `max_steps` to its default if the configured value is lower.
    pub fn apply_cli(&mut self, cli: &Cli) {
        if let Some(mode) = cli.mode {
            self.mode = mode;
        }
        if cli.continuous {
            self.mode = Mode::Sovereign;
            self.continuous = true;
        }
        if cli.auto || self.mode == Mode::Sovereign {
            self.auto_approve = true;
        }
        if self.mode == Mode::Sovereign && self.max_steps < Mode::Sovereign.default_max_steps() {
            self.max_steps = Mode::Sovereign.default_max_steps();
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    fn cli(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("wizard").chain(args.iter().copied()))
            .expect("valid args")
    }

    #[test]
    fn defaults_match_docs() {
        let config = Config::default();
        assert_eq!(config.model, "qwen3.6:27b");
        assert_eq!(config.ollama_host, "http://127.0.0.1:11434");
        assert_eq!(config.llamacpp_host, "http://127.0.0.1:8080");
        assert!(config.gguf_path.is_none());
        assert_eq!(config.mode, Mode::Genie);
        assert!(config.auto_approve);
        assert_eq!(config.max_steps, 25);
        assert!(!config.continuous);
        assert_eq!(config.retry_base_secs, 5);
        assert_eq!(config.retry_max_secs, 300);
        assert_eq!(config.cycle_pause_secs, 0);
        assert_eq!(config.compact_threshold_bytes, 48_000);
    }

    #[test]
    fn mode_parameters() {
        assert_eq!(Mode::Genie.temperature(), 0.8);
        assert_eq!(Mode::Sovereign.temperature(), 0.6);
        assert_eq!(Mode::Genie.default_max_steps(), 25);
        assert_eq!(Mode::Sovereign.default_max_steps(), 100);
        assert_eq!(Mode::Genie.to_string(), "genie");
        assert_eq!(Mode::Sovereign.to_string(), "sovereign");
    }

    #[test]
    fn missing_keys_take_defaults() {
        let config: Config = toml::from_str("model = \"qwen3.5:9b\"").expect("valid toml");
        assert_eq!(config.model, "qwen3.5:9b");
        assert_eq!(config.ollama_host, "http://127.0.0.1:11434");
        assert_eq!(config.mode, Mode::Genie);
        assert_eq!(config.max_steps, 25);
    }

    #[test]
    fn full_file_round_trips() {
        let original = Config {
            model: "llama3.3:70b".to_string(),
            ollama_host: "http://10.0.0.5:11434".to_string(),
            llamacpp_host: "http://10.0.0.5:8080".to_string(),
            gguf_path: Some("/models/qwen3-8b-q4_k_m.gguf".to_string()),
            mode: Mode::Sovereign,
            auto_approve: true,
            max_steps: 200,
            continuous: true,
            retry_base_secs: 10,
            retry_max_secs: 600,
            cycle_pause_secs: 30,
            compact_threshold_bytes: 96_000,
            providers: vec![ProviderConfig {
                name: "openai".to_string(),
                kind: ProviderKind::Openai,
                base_url: "https://api.openai.com/v1".to_string(),
                model: "gpt-4o".to_string(),
                api_key_env: Some("OPENAI_API_KEY".to_string()),
                gguf_path: None,
            }],
            active_provider: Some("openai".to_string()),
            gateway: GatewayConfig {
                kind: GatewayKind::Telegram,
                token_env: Some("MY_BOT_TOKEN".to_string()),
                allowed_chat_ids: vec![42, -100123],
            },
        };
        let raw = toml::to_string_pretty(&original).expect("serialize");
        let parsed: Config = toml::from_str(&raw).expect("parse back");
        assert_eq!(parsed.model, original.model);
        assert_eq!(parsed.ollama_host, original.ollama_host);
        assert_eq!(parsed.llamacpp_host, original.llamacpp_host);
        assert_eq!(parsed.gguf_path, original.gguf_path);
        assert_eq!(parsed.mode, original.mode);
        assert_eq!(parsed.auto_approve, original.auto_approve);
        assert_eq!(parsed.max_steps, original.max_steps);
        assert_eq!(parsed.continuous, original.continuous);
        assert_eq!(parsed.retry_base_secs, original.retry_base_secs);
        assert_eq!(parsed.retry_max_secs, original.retry_max_secs);
        assert_eq!(parsed.cycle_pause_secs, original.cycle_pause_secs);
        assert_eq!(
            parsed.compact_threshold_bytes,
            original.compact_threshold_bytes
        );
        assert_eq!(parsed.providers.len(), 1);
        assert_eq!(parsed.providers[0].name, "openai");
        assert_eq!(parsed.providers[0].kind, ProviderKind::Openai);
        assert_eq!(
            parsed.providers[0].api_key_env.as_deref(),
            Some("OPENAI_API_KEY")
        );
        assert_eq!(parsed.active_provider.as_deref(), Some("openai"));
        assert_eq!(parsed.gateway.kind, GatewayKind::Telegram);
        assert_eq!(parsed.gateway.token_env.as_deref(), Some("MY_BOT_TOKEN"));
        assert_eq!(parsed.gateway.allowed_chat_ids, vec![42, -100123]);
    }

    #[test]
    fn gateway_defaults_to_none_and_round_trips() {
        // A config without a [gateway] table defaults to None / terminal only.
        let config: Config = toml::from_str("model = \"m\"").expect("valid toml");
        assert_eq!(config.gateway.kind, GatewayKind::None);
        assert!(config.gateway.token_env.is_none());
        assert!(config.gateway.allowed_chat_ids.is_empty());
        assert_eq!(config.gateway.token_env(), GatewayConfig::DEFAULT_TOKEN_ENV);

        // A Telegram gateway round-trips through TOML.
        let raw = toml::to_string_pretty(&Config {
            gateway: GatewayConfig {
                kind: GatewayKind::Telegram,
                token_env: None,
                allowed_chat_ids: vec![7],
            },
            ..Config::default()
        })
        .expect("serialize");
        let parsed: Config = toml::from_str(&raw).expect("parse back");
        assert_eq!(parsed.gateway.kind, GatewayKind::Telegram);
        assert_eq!(parsed.gateway.allowed_chat_ids, vec![7]);
    }

    #[test]
    fn legacy_ollama_config_synthesizes_llamacpp() {
        // A file with only model/ollama_host (no providers table) still
        // parses, but the synthesized local provider is llama.cpp — Ollama
        // is opt-in via an explicit [[providers]] entry.
        let config =
            Config::from_toml("model = \"qwen3.5:9b\"\nollama_host = \"http://10.0.0.5:11434\"")
                .expect("valid toml");
        assert!(config.providers.is_empty());
        let active = config.active();
        assert_eq!(active.name, "local");
        assert_eq!(active.kind, ProviderKind::LlamaCpp);
        assert_eq!(active.base_url, "http://127.0.0.1:8080");
        assert_eq!(active.model, "qwen3.5:9b");
        assert!(active.api_key_env.is_none());
        assert_eq!(config.ollama_host, "http://10.0.0.5:11434");
    }

    #[test]
    fn fresh_default_synthesizes_llamacpp() {
        // No config file at all: the synthesized provider is llama.cpp.
        let config = Config::default();
        let active = config.active();
        assert_eq!(active.name, "local");
        assert_eq!(active.kind, ProviderKind::LlamaCpp);
        assert_eq!(active.base_url, "http://127.0.0.1:8080");
        assert_eq!(active.model, "qwen3.6:27b");
        assert!(active.api_key_env.is_none());
        assert!(active.gguf_path.is_none());

        // An empty file is equivalent to no file.
        let config = Config::from_toml("").expect("valid toml");
        assert_eq!(config.active().kind, ProviderKind::LlamaCpp);
    }

    #[test]
    fn saved_default_config_stays_llamacpp_on_reload() {
        // save() writes every field, including ollama_host — its presence
        // must not change the synthesized llama.cpp default.
        let raw = toml::to_string_pretty(&Config::default()).expect("serialize");
        assert!(raw.contains("ollama_host"), "save writes legacy fields");
        let config = Config::from_toml(&raw).expect("parse back");
        assert_eq!(config.active().kind, ProviderKind::LlamaCpp);
    }

    #[test]
    fn llamacpp_provider_round_trips_through_toml() {
        let original = Config {
            providers: vec![ProviderConfig {
                name: "local".to_string(),
                kind: ProviderKind::LlamaCpp,
                base_url: "http://127.0.0.1:8080".to_string(),
                model: "qwen3-8b".to_string(),
                api_key_env: None,
                gguf_path: Some("/home/u/.wizard/models/qwen3-8b-q4_k_m.gguf".to_string()),
            }],
            active_provider: Some("local".to_string()),
            ..Config::default()
        };
        let raw = toml::to_string_pretty(&original).expect("serialize");
        assert!(raw.contains("kind = \"llamacpp\""), "raw: {raw}");
        let parsed: Config = toml::from_str(&raw).expect("parse back");
        assert_eq!(parsed.providers.len(), 1);
        assert_eq!(parsed.providers[0].kind, ProviderKind::LlamaCpp);
        assert_eq!(
            parsed.providers[0].gguf_path.as_deref(),
            Some("/home/u/.wizard/models/qwen3-8b-q4_k_m.gguf")
        );
        assert!(parsed.providers[0].api_key_env.is_none());
        assert_eq!(parsed.active().kind, ProviderKind::LlamaCpp);
    }

    #[test]
    fn xai_kinds_round_trip_through_toml() {
        let original = Config {
            providers: vec![
                ProviderConfig {
                    name: "xai".to_string(),
                    kind: ProviderKind::Xai,
                    base_url: "https://api.x.ai/v1".to_string(),
                    model: "grok-4.3".to_string(),
                    api_key_env: Some("XAI_API_KEY".to_string()),
                    gguf_path: None,
                },
                ProviderConfig {
                    name: "xai-account".to_string(),
                    kind: ProviderKind::XaiOauth,
                    base_url: "https://api.x.ai/v1".to_string(),
                    model: "grok-4.3".to_string(),
                    api_key_env: None,
                    gguf_path: None,
                },
            ],
            active_provider: Some("xai-account".to_string()),
            ..Config::default()
        };
        let raw = toml::to_string_pretty(&original).expect("serialize");
        // The serde names are what the /provider parser and Display use.
        assert!(raw.contains("kind = \"xai\""), "raw: {raw}");
        assert!(raw.contains("kind = \"xaioauth\""), "raw: {raw}");
        let parsed: Config = toml::from_str(&raw).expect("parse back");
        assert_eq!(parsed.providers[0].kind, ProviderKind::Xai);
        assert_eq!(
            parsed.providers[0].api_key_env.as_deref(),
            Some("XAI_API_KEY")
        );
        assert_eq!(parsed.providers[1].kind, ProviderKind::XaiOauth);
        assert!(parsed.providers[1].api_key_env.is_none());
        assert_eq!(parsed.active().kind, ProviderKind::XaiOauth);
    }

    #[test]
    fn provider_kind_display_matches_serde_names() {
        for (kind, name) in [
            (ProviderKind::LlamaCpp, "llamacpp"),
            (ProviderKind::Ollama, "ollama"),
            (ProviderKind::Openai, "openai"),
            (ProviderKind::Anthropic, "anthropic"),
            (ProviderKind::Xai, "xai"),
            (ProviderKind::XaiOauth, "xaioauth"),
        ] {
            assert_eq!(kind.to_string(), name);
            let json = serde_json::to_value(kind).expect("serialize kind");
            assert_eq!(
                json,
                serde_json::json!(name),
                "Display and serde must agree"
            );
        }
    }

    #[test]
    fn active_selects_by_name_and_falls_back_to_first() {
        let providers = vec![
            ProviderConfig {
                name: "local".to_string(),
                kind: ProviderKind::Ollama,
                base_url: "http://127.0.0.1:11434".to_string(),
                model: "qwen3.6:27b".to_string(),
                api_key_env: None,
                gguf_path: None,
            },
            ProviderConfig {
                name: "claude".to_string(),
                kind: ProviderKind::Anthropic,
                base_url: "https://api.anthropic.com".to_string(),
                model: "claude-fable-5".to_string(),
                api_key_env: Some("ANTHROPIC_API_KEY".to_string()),
                gguf_path: None,
            },
        ];

        // Explicit selection by name.
        let config = Config {
            providers: providers.clone(),
            active_provider: Some("claude".to_string()),
            ..Config::default()
        };
        assert_eq!(config.active().name, "claude");
        assert_eq!(config.active().kind, ProviderKind::Anthropic);

        // Unset active_provider falls back to the first.
        let config = Config {
            providers: providers.clone(),
            active_provider: None,
            ..Config::default()
        };
        assert_eq!(config.active().name, "local");

        // Unknown active_provider also falls back to the first.
        let config = Config {
            providers,
            active_provider: Some("missing".to_string()),
            ..Config::default()
        };
        assert_eq!(config.active().name, "local");
    }

    #[test]
    fn env_model_overrides_active_provider_when_configured() {
        let mut config = Config {
            providers: vec![ProviderConfig {
                name: "openai".to_string(),
                kind: ProviderKind::Openai,
                base_url: "https://api.openai.com/v1".to_string(),
                model: "gpt-4o".to_string(),
                api_key_env: Some("OPENAI_API_KEY".to_string()),
                gguf_path: None,
            }],
            active_provider: Some("openai".to_string()),
            ..Config::default()
        };
        config.apply_env_from(|name| match name {
            "WIZARD_MODEL" => Some("gpt-4o-mini".to_string()),
            _ => None,
        });
        assert_eq!(config.active().model, "gpt-4o-mini");
        assert_eq!(config.model, "gpt-4o-mini", "legacy field also updated");
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let config: Config =
            toml::from_str("model = \"m\"\nfuture_option = true").expect("valid toml");
        assert_eq!(config.model, "m");
    }

    #[test]
    fn env_overrides_model_and_host() {
        let mut config = Config::default();
        config.apply_env_from(|name| match name {
            "WIZARD_MODEL" => Some("  llama3.3:70b  ".to_string()),
            "WIZARD_OLLAMA_HOST" => Some("http://10.0.0.5:11434///".to_string()),
            _ => None,
        });
        assert_eq!(config.model, "llama3.3:70b", "model is trimmed");
        assert_eq!(
            config.ollama_host, "http://10.0.0.5:11434",
            "host trailing slashes are trimmed"
        );
    }

    #[test]
    fn env_ollama_host_does_not_change_synthesized_kind() {
        // The env var updates the field (for explicitly configured Ollama
        // providers) but the synthesized local provider stays llama.cpp.
        let mut config = Config::default();
        config.apply_env_from(|name| match name {
            "WIZARD_OLLAMA_HOST" => Some("http://10.0.0.5:11434".to_string()),
            _ => None,
        });
        assert_eq!(config.ollama_host, "http://10.0.0.5:11434");
        assert_eq!(config.active().kind, ProviderKind::LlamaCpp);
    }

    #[test]
    fn env_llamacpp_host_overrides_synthesized_base_url() {
        let mut config = Config::from_toml("model = \"qwen3.5:9b\"").expect("valid toml");
        config.apply_env_from(|name| match name {
            "WIZARD_OLLAMA_HOST" => Some("http://10.0.0.5:11434".to_string()),
            "WIZARD_LLAMACPP_HOST" => Some("http://10.0.0.5:8080///".to_string()),
            _ => None,
        });
        let active = config.active();
        assert_eq!(active.kind, ProviderKind::LlamaCpp);
        assert_eq!(
            active.base_url, "http://10.0.0.5:8080",
            "host trailing slashes are trimmed"
        );
        assert_eq!(config.ollama_host, "http://10.0.0.5:11434");
    }

    #[test]
    fn env_gguf_path_feeds_synthesized_and_active_llamacpp_provider() {
        // Synthesized provider picks up the path.
        let mut config = Config::default();
        config.apply_env_from(|name| match name {
            "WIZARD_GGUF_PATH" => Some("  /models/a.gguf  ".to_string()),
            _ => None,
        });
        assert_eq!(config.gguf_path.as_deref(), Some("/models/a.gguf"));
        assert_eq!(config.active().gguf_path.as_deref(), Some("/models/a.gguf"));

        // An explicitly configured active llamacpp provider is updated too;
        // other kinds are left alone.
        let mut config = Config {
            providers: vec![ProviderConfig {
                name: "local".to_string(),
                kind: ProviderKind::LlamaCpp,
                base_url: "http://127.0.0.1:8080".to_string(),
                model: "qwen3-8b".to_string(),
                api_key_env: None,
                gguf_path: None,
            }],
            active_provider: Some("local".to_string()),
            ..Config::default()
        };
        config.apply_env_from(|name| match name {
            "WIZARD_GGUF_PATH" => Some("/models/b.gguf".to_string()),
            _ => None,
        });
        assert_eq!(config.active().gguf_path.as_deref(), Some("/models/b.gguf"));
    }

    #[test]
    fn env_unset_keeps_existing_values() {
        let mut config = Config::default();
        config.apply_env_from(|_| None);
        assert_eq!(config.model, "qwen3.6:27b");
        assert_eq!(config.ollama_host, "http://127.0.0.1:11434");
        assert_eq!(config.llamacpp_host, "http://127.0.0.1:8080");
        assert!(config.gguf_path.is_none());
    }

    #[test]
    fn env_empty_values_are_ignored() {
        let mut config = Config::default();
        config.apply_env_from(|name| match name {
            "WIZARD_MODEL" => Some("   ".to_string()),
            "WIZARD_OLLAMA_HOST" => Some("".to_string()),
            "WIZARD_LLAMACPP_HOST" => Some("  ".to_string()),
            "WIZARD_GGUF_PATH" => Some("".to_string()),
            _ => None,
        });
        assert_eq!(config.model, "qwen3.6:27b");
        assert_eq!(config.ollama_host, "http://127.0.0.1:11434");
        assert_eq!(config.llamacpp_host, "http://127.0.0.1:8080");
        assert!(config.gguf_path.is_none());
        assert_eq!(
            config.active().kind,
            ProviderKind::LlamaCpp,
            "empty env values do not opt into Ollama"
        );
    }

    #[test]
    fn cli_mode_overrides_config() {
        let mut config = Config::default();
        config.apply_cli(&cli(&["--mode", "sovereign"]));
        assert_eq!(config.mode, Mode::Sovereign);
        assert!(config.auto_approve, "sovereign implies auto-approve");
        assert_eq!(config.max_steps, 100, "sovereign raises the step budget");
    }

    #[test]
    fn continuous_flag_forces_sovereign() {
        let mut config = Config::default();
        config.apply_cli(&cli(&["--continuous"]));
        assert_eq!(config.mode, Mode::Sovereign);
        assert!(config.continuous);
        assert!(config.auto_approve);
        assert_eq!(config.max_steps, 100);
    }

    #[test]
    fn sovereign_keeps_explicitly_higher_max_steps() {
        let mut config = Config {
            max_steps: 250,
            ..Config::default()
        };
        config.apply_cli(&cli(&["--mode", "sovereign"]));
        assert_eq!(config.max_steps, 250);
    }

    #[test]
    fn auto_flag_forces_auto_approve_in_genie() {
        let mut config = Config {
            auto_approve: false, // start from the opt-in gated posture
            ..Config::default()
        };
        config.apply_cli(&cli(&["--auto"]));
        assert_eq!(config.mode, Mode::Genie);
        assert!(config.auto_approve);
        assert_eq!(config.max_steps, 25, "genie keeps its budget");
    }

    #[test]
    fn no_flags_leaves_config_untouched() {
        let mut config = Config::default();
        config.apply_cli(&cli(&[]));
        assert_eq!(config.mode, Mode::Genie);
        assert!(config.auto_approve);
        assert_eq!(config.max_steps, 25);
    }

    #[test]
    fn config_sovereign_mode_implies_auto_approve_without_flags() {
        let mut config = Config {
            mode: Mode::Sovereign,
            ..Config::default()
        };
        config.apply_cli(&cli(&[]));
        assert!(config.auto_approve);
        assert_eq!(config.max_steps, 100);
    }
}
