//! The GUI's view of `~/.wizard/config.toml`: a store that serializes writes
//! and always re-reads the file before changing it, plus the provider presets
//! the Settings page and onboarding offer.
//!
//! Config is shared mutable state across processes — the TUI, other `wizard
//! gui` servers, and this one all write the same file, and [`Config::save`]
//! rewrites it whole. A long-lived process that saved a snapshot it loaded at
//! startup would silently drop everything added since. So every mutation here
//! re-reads the file, applies the change to *that*, and writes it back under a
//! lock; a stale in-memory copy can never be the thing that lands on disk.

use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::config::{Config, ProviderConfig, ProviderKind};
use crate::credentials;
use crate::llm::{cloudflare, openrouter, xai_oauth};

/// A provider offered by the Settings page's "add provider" list and by
/// onboarding: the defaults to prefill, and what the user still has to give.
#[derive(Debug, Clone, Serialize)]
pub struct Preset {
    /// Suggested provider name (the credentials key, and the sidebar label).
    pub name: &'static str,
    pub label: &'static str,
    pub kind: &'static str,
    pub base_url: &'static str,
    pub model: &'static str,
    /// The provider cannot answer without an API key.
    pub needs_key: bool,
    /// The base URL is a template the user must complete (Cloudflare's
    /// account id); the UI keeps the field editable and shows the placeholder.
    pub needs_base_url: bool,
}

/// The providers the GUI can set up by pasting a key. `xaioauth` and
/// `chatgptoauth` are deliberately absent: a subscription is not a string you
/// can paste, so they are earned through the sign-in rows
/// (`POST /api/login/{provider}`, see [`crate::gui::oauth`]) and then show up
/// here like any other provider.
pub const PRESETS: &[Preset] = &[
    Preset {
        name: "anthropic",
        label: "Anthropic",
        kind: "anthropic",
        base_url: "https://api.anthropic.com",
        model: "claude-fable-5",
        needs_key: true,
        needs_base_url: false,
    },
    Preset {
        name: "openai",
        label: "OpenAI",
        kind: "openai",
        base_url: "https://api.openai.com/v1",
        model: "gpt-5.2",
        needs_key: true,
        needs_base_url: false,
    },
    Preset {
        name: "xai",
        label: "xAI",
        kind: "xai",
        base_url: "https://api.x.ai/v1",
        model: xai_oauth::DEFAULT_MODEL,
        needs_key: true,
        needs_base_url: false,
    },
    Preset {
        name: "openrouter",
        label: "OpenRouter",
        kind: "openrouter",
        base_url: openrouter::DEFAULT_BASE_URL,
        model: openrouter::DEFAULT_MODEL,
        needs_key: true,
        needs_base_url: false,
    },
    Preset {
        name: "cloudflare",
        label: "Cloudflare Workers AI",
        kind: "cloudflare",
        base_url: cloudflare::BASE_URL_TEMPLATE,
        model: cloudflare::DEFAULT_MODEL,
        needs_key: true,
        needs_base_url: true,
    },
    Preset {
        name: "ollama",
        label: "Ollama",
        kind: "ollama",
        base_url: "http://127.0.0.1:11434",
        model: "qwen3:8b",
        needs_key: false,
        needs_base_url: false,
    },
    Preset {
        name: "llamacpp",
        label: "llama.cpp",
        kind: "llamacpp",
        base_url: "http://127.0.0.1:11435",
        model: "qwen3.6:27b",
        needs_key: false,
        needs_base_url: false,
    },
];

/// Where a provider's API key comes from, for the Settings page's key column.
/// The order mirrors [`ProviderConfig`]'s own resolution: the credential file
/// wins over the environment.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeySource {
    /// Stored in `~/.wizard/credentials.toml` under the provider's name.
    Stored,
    /// Read from an environment variable at request time.
    Env,
    /// An OAuth token from `wizard login` — no key to manage here.
    Oauth,
    /// A local backend that needs no key.
    NotNeeded,
    /// The provider needs a key and has none: requests will 401.
    Missing,
}

/// The default environment variable a kind falls back to, if any.
fn default_key_env(kind: ProviderKind) -> Option<&'static str> {
    match kind {
        ProviderKind::OpenRouter => Some(openrouter::DEFAULT_KEY_ENV),
        ProviderKind::Xai => Some(xai_oauth::DEFAULT_KEY_ENV),
        ProviderKind::Cloudflare => Some(cloudflare::DEFAULT_KEY_ENV),
        _ => None,
    }
}

/// Where `provider` would get its key right now.
pub fn key_source(provider: &ProviderConfig) -> KeySource {
    match provider.kind {
        ProviderKind::LlamaCpp | ProviderKind::Ollama => return KeySource::NotNeeded,
        ProviderKind::XaiOauth | ProviderKind::ChatgptOauth => return KeySource::Oauth,
        _ => {}
    }
    if credentials::get(&provider.name).is_some_and(|key| !key.is_empty()) {
        return KeySource::Stored;
    }
    let env = provider
        .api_key_env
        .as_deref()
        .or_else(|| default_key_env(provider.kind));
    match env.and_then(|name| std::env::var(name).ok()) {
        Some(key) if !key.is_empty() => KeySource::Env,
        _ => KeySource::Missing,
    }
}

/// Serialized access to `~/.wizard/config.toml`.
///
/// `current()` is the config the server acts on (env overrides applied, as
/// everywhere else in wizard); `update()` is the only way to change the file.
pub struct ConfigStore {
    /// The last known config, kept so a transient read failure still leaves
    /// the server able to answer.
    cached: Mutex<Config>,
    /// Held across the read-modify-write of a mutation, so two concurrent
    /// settings requests cannot interleave into a lost update.
    write_lock: Mutex<()>,
}

impl ConfigStore {
    pub fn new(config: Config) -> Self {
        Self {
            cached: Mutex::new(config),
            write_lock: Mutex::new(()),
        }
    }

    /// The current config: re-read from disk so edits made by the TUI (or
    /// another GUI) are picked up without a restart. A read failure falls back
    /// to the last good copy rather than failing the request.
    pub fn current(&self) -> Config {
        match Config::load() {
            Ok(config) => {
                *self.lock_cached() = config.clone();
                config
            }
            Err(err) => {
                tracing::warn!("re-reading the config failed, using the cached copy: {err:#}");
                self.lock_cached().clone()
            }
        }
    }

    /// Apply `mutate` to the config **as it is on disk** and write it back.
    ///
    /// The mutation runs against a raw parse of the file — not against
    /// [`Config::load`], whose env overrides (`WIZARD_MODEL` and friends) would
    /// otherwise be baked into the file as if the user had typed them.
    pub fn update<F>(&self, mutate: F) -> Result<Config>
    where
        F: FnOnce(&mut Config) -> Result<()>,
    {
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let mut on_disk = read_raw()?;
        mutate(&mut on_disk)?;
        on_disk.save().context("saving the config")?;
        drop(_guard);
        Ok(self.current())
    }

    fn lock_cached(&self) -> std::sync::MutexGuard<'_, Config> {
        self.cached.lock().unwrap_or_else(|err| err.into_inner())
    }
}

/// Parse `~/.wizard/config.toml` with no env overrides applied. A missing file
/// is a fresh install, not an error.
fn read_raw() -> Result<Config> {
    let path = Config::path()?;
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(Config::default());
    };
    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// The path the Settings page shows, best-effort.
pub fn config_path() -> String {
    Config::path()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "~/.wizard/config.toml".to_string())
}

/// Add `provider`, replacing any provider of the same name (that is what an
/// edit is), and make it active when asked.
pub fn upsert_provider(config: &mut Config, provider: ProviderConfig, activate: bool) {
    let name = provider.name.clone();
    config.providers.retain(|existing| existing.name != name);
    config.providers.push(provider);
    if activate || config.active_provider.is_none() {
        config.active_provider = Some(name);
    }
}

/// Remove the provider named `name`. Removing the active one hands `active` to
/// whatever is left, so the config never points at a provider that is gone.
pub fn remove_provider(config: &mut Config, name: &str) -> Result<()> {
    anyhow::ensure!(
        config.providers.iter().any(|p| p.name == name),
        "no provider named '{name}'"
    );
    config.providers.retain(|p| p.name != name);
    if config.active_provider.as_deref() == Some(name) {
        config.active_provider = config.providers.first().map(|p| p.name.clone());
    }
    Ok(())
}

/// A `~/.wizard/credentials.toml` entry, or its removal when `key` is empty.
pub fn store_key(name: &str, key: &str) -> Result<()> {
    if key.trim().is_empty() {
        return Ok(());
    }
    credentials::store(name, key.trim())
}

/// Path of the credentials file, for the Settings page's "where keys live" note.
pub fn credentials_path() -> Option<PathBuf> {
    credentials::path().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(name: &str) -> ProviderConfig {
        ProviderConfig {
            name: name.to_string(),
            kind: ProviderKind::Openai,
            base_url: "https://example.test/v1".to_string(),
            model: "m".to_string(),
            api_key_env: None,
            gguf_path: None,
            usd_per_mtok_in: None,
            usd_per_mtok_out: None,
        }
    }

    #[test]
    fn upsert_replaces_by_name_and_can_activate() {
        let mut config = Config::default();
        upsert_provider(&mut config, provider("a"), false);
        // The first provider always becomes active — a config with providers
        // and no active one would silently fall back to the first anyway.
        assert_eq!(config.active_provider.as_deref(), Some("a"));

        upsert_provider(&mut config, provider("b"), false);
        assert_eq!(config.providers.len(), 2);
        assert_eq!(config.active_provider.as_deref(), Some("a"));

        let mut edited = provider("a");
        edited.model = "m2".to_string();
        upsert_provider(&mut config, edited, true);
        assert_eq!(config.providers.len(), 2, "an edit is not a second entry");
        assert_eq!(config.active_provider.as_deref(), Some("a"));
        assert_eq!(
            config
                .providers
                .iter()
                .find(|p| p.name == "a")
                .unwrap()
                .model,
            "m2"
        );
    }

    #[test]
    fn removing_the_active_provider_hands_active_to_a_survivor() {
        let mut config = Config::default();
        upsert_provider(&mut config, provider("a"), true);
        upsert_provider(&mut config, provider("b"), false);
        remove_provider(&mut config, "a").unwrap();
        assert_eq!(config.active_provider.as_deref(), Some("b"));

        remove_provider(&mut config, "b").unwrap();
        assert!(config.providers.is_empty());
        assert_eq!(config.active_provider, None, "nothing left to point at");
        assert!(remove_provider(&mut config, "gone").is_err());
    }

    #[test]
    fn local_providers_need_no_key() {
        let mut local = provider("local");
        local.kind = ProviderKind::LlamaCpp;
        assert!(matches!(key_source(&local), KeySource::NotNeeded));

        let mut oauth = provider("xai");
        oauth.kind = ProviderKind::XaiOauth;
        assert!(matches!(key_source(&oauth), KeySource::Oauth));
    }

    #[test]
    fn a_cloud_provider_with_no_key_anywhere_is_reported_missing() {
        // A distinctive name: the credential store is shared process-wide in
        // tests, and this provider must have no stored key.
        let unkeyed = provider("settings-test-unkeyed");
        assert!(matches!(key_source(&unkeyed), KeySource::Missing));

        // An env fallback that names an unset variable is no key either — the
        // Settings page must say "missing", not "env".
        let mut env_only = provider("settings-test-env");
        env_only.api_key_env = Some("WIZARD_TEST_KEY_THAT_IS_NEVER_SET".to_string());
        assert!(matches!(key_source(&env_only), KeySource::Missing));
    }

    #[test]
    fn presets_are_all_valid_provider_kinds() {
        for preset in PRESETS {
            let kind: ProviderKind = toml::from_str(&format!("kind = \"{}\"", preset.kind))
                .map(|v: KindProbe| v.kind)
                .unwrap_or_else(|err| panic!("preset {}: {err}", preset.name));
            // Every preset that needs no key must be a local backend.
            if !preset.needs_key {
                assert!(matches!(
                    kind,
                    ProviderKind::LlamaCpp | ProviderKind::Ollama
                ));
            }
        }
    }

    #[derive(serde::Deserialize)]
    struct KindProbe {
        kind: ProviderKind,
    }
}
