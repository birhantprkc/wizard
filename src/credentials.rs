//! Plaintext API keys for cloud providers, stored in
//! `~/.wizard/credentials.toml` (0600) keyed by provider name. Unlike
//! `config.toml` — which only ever names the env var holding a key — this file
//! holds the secret itself, so it is written atomically with tight
//! permissions (mirroring `xai_oauth::save_tokens`) and reads never hard-fail.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::config::Config;

/// On-disk shape of `credentials.toml`: a `[keys]` table mapping provider name
/// to its API key.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    keys: BTreeMap<String, String>,
}

/// `~/.wizard/credentials.toml`.
pub fn path() -> Result<PathBuf> {
    Ok(Config::wizard_dir()?.join("credentials.toml"))
}

/// Read and parse the store at `path`. A missing file yields an empty store;
/// a parse error is logged and also yields an empty store — reads never
/// hard-fail, so a corrupt file degrades to "no stored keys" rather than
/// breaking provider setup.
fn load_at(path: &Path) -> Store {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Store::default(),
        Err(err) => {
            tracing::warn!("could not read {}: {err}", path.display());
            return Store::default();
        }
    };
    match toml::from_str(&raw) {
        Ok(store) => store,
        Err(err) => {
            tracing::warn!("could not parse {}: {err}", path.display());
            Store::default()
        }
    }
}

/// The stored key for `name` at `path`, or `None` when absent or empty.
fn get_at(path: &Path, name: &str) -> Option<String> {
    load_at(path)
        .keys
        .get(name)
        .filter(|key| !key.is_empty())
        .cloned()
}

/// Insert `key` for `name` and persist atomically (0600). Mirrors
/// `xai_oauth::save_tokens`: the parent dir is created and tightened to 0700,
/// then a 0600 temp file is written and renamed over the target.
fn store_at(path: &Path, name: &str, key: &str) -> Result<()> {
    let mut store = load_at(path);
    store.keys.insert(name.to_string(), key.to_string());
    write_at(path, &store)
}

/// Remove `name` from the store and persist the result.
fn remove_at(path: &Path, name: &str) -> Result<()> {
    let mut store = load_at(path);
    store.keys.remove(name);
    write_at(path, &store)
}

/// Serialize `store` to `path` atomically with 0600 permissions.
fn write_at(path: &Path, store: &Store) -> Result<()> {
    let dir = path.parent().ok_or_else(|| {
        anyhow!(
            "credentials path {} has no parent directory",
            path.display()
        )
    })?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restricting permissions on {}", dir.display()))?;
    }

    let raw = toml::to_string_pretty(store).context("serializing credentials")?;
    let tmp = dir.join(".credentials.toml.tmp");
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        // create(true) keeps the mode of a pre-existing file; enforce 0600.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("restricting permissions on {}", tmp.display()))?;
        }
        file.write_all(raw.as_bytes())
            .with_context(|| format!("writing {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("syncing {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("moving {} into place", path.display()))?;
    Ok(())
}

/// The stored API key for provider `name`, or `None` when none is set.
pub fn get(name: &str) -> Option<String> {
    let path = path().ok()?;
    get_at(&path, name)
}

/// Store `key` as the API key for provider `name`, persisting atomically
/// (0600).
pub fn store(name: &str, key: &str) -> Result<()> {
    store_at(&path()?, name, key)
}

/// Remove any stored API key for provider `name`.
pub fn remove(name: &str) -> Result<()> {
    remove_at(&path()?, name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_get_remove_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("credentials.toml");

        // Missing file: nothing stored.
        assert_eq!(get_at(&path, "openai"), None);

        // Store then read back.
        store_at(&path, "openai", "sk-test-123").expect("store");
        assert_eq!(get_at(&path, "openai"), Some("sk-test-123".to_string()));

        // A second key coexists with the first.
        store_at(&path, "claude", "sk-ant-456").expect("store second");
        assert_eq!(get_at(&path, "openai"), Some("sk-test-123".to_string()));
        assert_eq!(get_at(&path, "claude"), Some("sk-ant-456".to_string()));

        // Empty values read back as None.
        store_at(&path, "blank", "").expect("store empty");
        assert_eq!(get_at(&path, "blank"), None);

        // Remove drops just the one key.
        remove_at(&path, "openai").expect("remove");
        assert_eq!(get_at(&path, "openai"), None);
        assert_eq!(get_at(&path, "claude"), Some("sk-ant-456".to_string()));
    }

    #[test]
    fn corrupt_file_degrades_to_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("credentials.toml");
        std::fs::write(&path, "this is not valid toml = = =").expect("write garbage");
        assert_eq!(get_at(&path, "openai"), None);
    }

    #[cfg(unix)]
    #[test]
    fn stored_file_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("credentials.toml");
        store_at(&path, "openai", "sk-test").expect("store");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "credentials file must be 0600");
    }
}
