//! Pi packages as Wizard plugins (beta).
//!
//! A Pi package bundles skills, prompt templates, TypeScript extensions and
//! themes. Wizard reads the same `SKILL.md` format and has its own prompt
//! commands, so `wizard plugins install` fetches a package the way `pi
//! install` does (npm tarball, git clone, or a local directory), copies the
//! skills into `~/.wizard/skills/`, turns the prompts into commands under
//! `~/.wizard/commands/`, and reports every extension and theme as not
//! supported yet rather than pretending. See [`layout`] for the mapping.
//!
//! What was written is recorded in `~/.wizard/pi-plugins.json`, so `remove`
//! deletes exactly those paths and a reinstall replaces them. A path that
//! already exists and did not come from the same package is never
//! overwritten.
//!
//! `--for pi` hands the spec to `pi install`/`pi remove`, and `--for both`
//! does both. Pi keeps its own state; nothing here writes under `~/.pi`.

pub mod layout;
pub mod source;

#[cfg(test)]
mod tests;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::cli::PluginsCmd;
use crate::config::Config;
pub use layout::{Item, ItemKind, ItemStatus};
pub use source::Source;

/// Version of the `--json` output and of `pi-plugins.json`.
pub const SCHEMA: u32 = 1;

/// Said wherever a Pi package is installed for Wizard.
pub const BETA_NOTE: &str = "Pi plugins in Wizard are in beta: skills and prompts work, \
                             extensions and themes don't yet.";

/// Where Pi looks, for a package in which nothing was found.
const NOTHING_FOUND: &str = "Pi reads a package's resources from the `pi` key in its \
                             package.json, or from its skills/, prompts/, extensions/ and \
                             themes/ directories.";

const STORE_FILE: &str = "pi-plugins.json";
const PI_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const SEARCH_URL: &str = "https://registry.npmjs.org/-/v1/search";

/// Which harness a package goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Target {
    Wizard,
    Pi,
    Both,
}

impl Target {
    pub fn wizard(self) -> bool {
        matches!(self, Target::Wizard | Target::Both)
    }

    pub fn pi(self) -> bool {
        matches!(self, Target::Pi | Target::Both)
    }

    pub fn label(self) -> &'static str {
        match self {
            Target::Wizard => "Wizard",
            Target::Pi => "Pi",
            Target::Both => "Pi and Wizard",
        }
    }
}

/// A package as Wizard installed it (or would).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Plugin {
    /// The package's name (`package.json` `name`, else from the source).
    pub name: String,
    /// The spec it came from, as `pi install` takes it.
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_at: Option<String>,
    pub items: Vec<Item>,
    /// Package-level caveats (runtime dependencies Wizard does not install).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

impl Plugin {
    /// `2 skills, 1 prompt` for what works, `1 extension` for what doesn't.
    pub fn summary(&self) -> (String, String) {
        let count = |ok: bool| {
            ItemKind::ALL
                .iter()
                .filter_map(|kind| {
                    let n = self
                        .items
                        .iter()
                        .filter(|i| i.kind == *kind)
                        .filter(|i| {
                            matches!(i.status, ItemStatus::Ready | ItemStatus::Installed) == ok
                        })
                        .count();
                    (n > 0).then(|| format!("{n} {}", kind.noun(n)))
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        (count(true), count(false))
    }
}

/// `~/.wizard/pi-plugins.json`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    schema: u32,
    #[serde(default)]
    plugins: Vec<Plugin>,
}

impl Store {
    fn load(home: &Path) -> Result<Self> {
        let path = home.join(STORE_FILE);
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("{} is not valid JSON", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
        }
    }

    fn save(&mut self, home: &Path) -> Result<()> {
        self.schema = SCHEMA;
        self.plugins.sort_by(|a, b| a.name.cmp(&b.name));
        let json = serde_json::to_vec_pretty(self)?;
        crate::platform::secrets::write_atomic(&home.join(STORE_FILE), &json)
    }

    /// The installed plugin `query` names: its name, its source, or the npm
    /// name inside an `npm:` spec.
    fn position(&self, query: &str) -> Option<usize> {
        let query = query.trim();
        let npm = query
            .strip_prefix("npm:")
            .map(source::npm_name)
            .unwrap_or(query);
        self.plugins
            .iter()
            .position(|p| p.name == query || p.source == query || p.name == npm)
    }

    /// Who owns `path` (relative to `~/.wizard`), other than `except`.
    fn owner(&self, path: &str, except: Option<&str>) -> Option<&str> {
        self.plugins
            .iter()
            .filter(|p| Some(p.name.as_str()) != except)
            .find(|p| {
                p.items
                    .iter()
                    .any(|i| i.path.as_deref() == Some(path) && i.status == ItemStatus::Installed)
            })
            .map(|p| p.name.as_str())
    }
}

fn wizard_home() -> Result<PathBuf> {
    Config::wizard_dir()
}

fn scratch(home: &Path) -> PathBuf {
    home.join("tmp").join("pi-plugins")
}

/// Fetch and map a package without writing anything: what `install` would do.
pub async fn inspect(spec: &str) -> Result<Plugin> {
    let source = source::parse(spec)?;
    let home = wizard_home()?;
    let fetched = source::fetch(&source, &scratch(&home)).await?;
    let (mut plugin, planned) = plan_package(&source, &fetched.root);
    let store = Store::load(&home)?;
    for (item, planned) in plugin.items.iter_mut().zip(&planned) {
        if planned.action.is_some() {
            check_conflict(item, &home, &store, &plugin.name);
        }
    }
    Ok(plugin)
}

fn plan_package(source: &Source, root: &Path) -> (Plugin, Vec<layout::Planned>) {
    let info = layout::read_package_info(root);
    let planned = layout::plan(root, &info);
    let mut notes = Vec::new();
    let skills = planned
        .iter()
        .any(|p| p.item.kind == ItemKind::Skill && p.action.is_some());
    if info.has_dependencies && skills {
        notes.push(
            "the package declares npm dependencies, which Pi installs and Wizard doesn't; \
             a skill script that imports one needs `npm install` in its directory"
                .to_string(),
        );
    }
    let plugin = Plugin {
        name: info.name.clone().unwrap_or_else(|| source.fallback_name()),
        source: source.spec(),
        version: info.version.clone(),
        description: info.description.clone(),
        installed_at: None,
        items: planned.iter().map(|p| p.item.clone()).collect(),
        notes,
    };
    (plugin, planned)
}

/// Mark a ready item skipped when its destination is somebody else's.
fn check_conflict(item: &mut Item, home: &Path, store: &Store, plugin: &str) {
    let Some(path) = item.path.clone() else {
        return;
    };
    let owned_here = store
        .plugins
        .iter()
        .find(|p| p.name == plugin)
        .is_some_and(|p| p.items.iter().any(|i| i.path.as_deref() == Some(&path)));
    let reason = if let Some(other) = store.owner(&path, Some(plugin)) {
        Some(format!(
            "the Pi plugin {other} already installed ~/.wizard/{path}"
        ))
    } else if !owned_here && home.join(&path).exists() {
        Some(format!(
            "~/.wizard/{path} already exists and didn't come from this plugin"
        ))
    } else {
        None
    };
    if let Some(reason) = reason {
        item.status = ItemStatus::Skipped;
        item.reason = Some(reason);
        item.path = None;
    }
}

/// Install `spec` for Wizard: fetch, map, write, record.
pub async fn install_wizard(spec: &str) -> Result<Plugin> {
    let source = source::parse(spec)?;
    let home = wizard_home()?;
    let fetched = source::fetch(&source, &scratch(&home)).await?;
    install_from(&source, &fetched.root, &home)
}

/// The synchronous half of [`install_wizard`], on a package already on disk.
pub(crate) fn install_from(source: &Source, root: &Path, home: &Path) -> Result<Plugin> {
    let (mut plugin, planned) = plan_package(source, root);
    if plugin.items.is_empty() {
        bail!("{} holds no Pi resources. {NOTHING_FOUND}", plugin.name);
    }
    let (works, missing) = plugin.summary();
    if works.is_empty() {
        bail!(
            "nothing in {} runs in Wizard yet ({missing}); install it for Pi instead",
            plugin.name
        );
    }
    let mut store = Store::load(home)?;
    let previous = store
        .position(&plugin.name)
        .map(|i| store.plugins.remove(i));
    // Put the old record back while checking, so its own paths read as ours.
    if let Some(previous) = &previous {
        store.plugins.push(previous.clone());
    }
    for (item, planned) in plugin.items.iter_mut().zip(&planned) {
        let Some(action) = &planned.action else {
            continue;
        };
        check_conflict(item, home, &store, &plugin.name);
        if item.status != ItemStatus::Ready {
            continue;
        }
        let path = item.path.clone().unwrap_or_default();
        match write_item(home, &path, action) {
            Ok(()) => item.status = ItemStatus::Installed,
            Err(err) => {
                item.status = ItemStatus::Skipped;
                item.reason = Some(format!("couldn't write it: {err:#}"));
                item.path = None;
            }
        }
    }
    // A reinstall drops what the old version had and the new one doesn't.
    store.plugins.retain(|p| p.name != plugin.name);
    if let Some(previous) = previous {
        let kept: BTreeSet<&str> = plugin
            .items
            .iter()
            .filter(|i| i.status == ItemStatus::Installed)
            .filter_map(|i| i.path.as_deref())
            .collect();
        for item in previous.items.iter() {
            if item.status == ItemStatus::Installed
                && let Some(path) = item.path.as_deref()
                && !kept.contains(path)
            {
                delete_path(home, path);
            }
        }
    }
    plugin.installed_at =
        Some(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    store.plugins.push(plugin.clone());
    store.save(home)?;
    Ok(plugin)
}

/// A recorded path, resolved under `home`, refusing anything that climbs out.
fn resolve(home: &Path, path: &str) -> Option<PathBuf> {
    let rel = Path::new(path);
    let top = rel.components().next()?;
    let safe = rel
        .components()
        .all(|c| matches!(c, std::path::Component::Normal(_)))
        && matches!(top.as_os_str().to_str(), Some("skills" | "commands"));
    safe.then(|| home.join(rel))
}

fn write_item(home: &Path, path: &str, action: &layout::Action) -> Result<()> {
    let dest = resolve(home, path).ok_or_else(|| anyhow!("refusing to write {path}"))?;
    let parent = dest
        .parent()
        .ok_or_else(|| anyhow!("{path} has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".{}.pi-install-{}",
        dest.file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default(),
        uuid::Uuid::new_v4().simple()
    ));
    let result = (|| -> Result<()> {
        match action {
            layout::Action::CopyDir(from) => copy_dir(from, &tmp)?,
            layout::Action::Write(text) => {
                // A one-file skill still becomes `<name>/SKILL.md`.
                if path.starts_with("skills/") {
                    std::fs::create_dir_all(&tmp)?;
                    std::fs::write(tmp.join("SKILL.md"), text)?;
                } else {
                    std::fs::write(&tmp, text)?;
                }
            }
        }
        remove_any(&dest)?;
        std::fs::rename(&tmp, &dest)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = remove_any(&tmp);
    }
    result
}

fn remove_any(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// Copy a skill directory: regular files and directories only, without
/// `node_modules` or `.git`.
fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == "node_modules" || name == ".git" {
            continue;
        }
        let kind = entry.file_type()?;
        let target = to.join(&name);
        if kind.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

fn delete_path(home: &Path, path: &str) -> bool {
    match resolve(home, path) {
        Some(full) => remove_any(&full).is_ok(),
        None => false,
    }
}

/// Remove a Wizard install: delete what its record says it wrote.
pub fn remove_wizard(query: &str) -> Result<Plugin> {
    let home = wizard_home()?;
    remove_from(query, &home)
}

pub(crate) fn remove_from(query: &str, home: &Path) -> Result<Plugin> {
    let mut store = Store::load(home)?;
    let index = store
        .position(query)
        .ok_or_else(|| anyhow!("{query} isn't installed for Wizard"))?;
    let plugin = store.plugins.remove(index);
    for item in &plugin.items {
        if item.status == ItemStatus::Installed
            && let Some(path) = item.path.as_deref()
        {
            delete_path(home, path);
        }
    }
    store.save(home)?;
    Ok(plugin)
}

/// Wizard's installs, each item checked against what the loaders see now.
pub fn list_wizard() -> Result<Vec<Plugin>> {
    let home = wizard_home()?;
    let mut plugins = Store::load(&home)?.plugins;
    let skills = crate::skills::load_skills(&crate::skills::default_roots()).unwrap_or_default();
    let commands = crate::commands::load_from_dirs(&[home.join("commands")]);
    mark_loaded(&mut plugins, &home, &skills, &commands);
    Ok(plugins)
}

pub(crate) fn mark_loaded(
    plugins: &mut [Plugin],
    home: &Path,
    skills: &[crate::skills::Skill],
    commands: &[crate::commands::CustomCommand],
) {
    let same = |a: &Path, b: &Path| match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    for plugin in plugins {
        for item in &mut plugin.items {
            let Some(path) = item
                .path
                .as_deref()
                .filter(|_| item.status == ItemStatus::Installed)
            else {
                continue;
            };
            let full = home.join(path);
            item.loaded = Some(match item.kind {
                ItemKind::Skill => skills
                    .iter()
                    .find(|s| s.name == item.name)
                    .is_some_and(|s| same(&s.path, &full.join("SKILL.md"))),
                ItemKind::Prompt => commands
                    .iter()
                    .find(|c| c.name == item.name)
                    .is_some_and(|c| same(&c.path, &full)),
                _ => false,
            });
        }
    }
}

/// One gallery hit: an npm package tagged `pi-package`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    pub name: String,
    pub version: String,
    /// What `install` takes: `npm:<name>`.
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publisher: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly_downloads: Option<u64>,
}

/// Search the npm registry for Pi packages (what pi.dev/packages lists).
pub async fn search(query: &str, size: usize) -> Result<Vec<SearchHit>> {
    let text = match query.trim() {
        "" => "keywords:pi-package".to_string(),
        query => format!("keywords:pi-package {query}"),
    };
    let body: serde_json::Value = source::http()?
        .get(SEARCH_URL)
        .query(&[("text", text.as_str()), ("size", &size.to_string())])
        .send()
        .await
        .context("couldn't reach the npm registry")?
        .error_for_status()?
        .json()
        .await
        .context("reading the npm registry's answer")?;
    Ok(parse_search(&body))
}

pub(crate) fn parse_search(body: &serde_json::Value) -> Vec<SearchHit> {
    let Some(objects) = body.get("objects").and_then(|o| o.as_array()) else {
        return Vec::new();
    };
    objects
        .iter()
        .filter_map(|object| {
            let package = object.get("package")?;
            let name = package.get("name")?.as_str()?.to_string();
            let text = |v: Option<&serde_json::Value>| {
                v.and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(str::to_string)
            };
            Some(SearchHit {
                version: text(package.get("version")).unwrap_or_default(),
                description: text(package.get("description")),
                publisher: text(package.get("publisher").and_then(|p| p.get("username"))),
                weekly_downloads: object
                    .get("downloads")
                    .and_then(|d| d.get("weekly"))
                    .and_then(|w| w.as_u64()),
                source: format!("npm:{name}"),
                name,
            })
        })
        .collect()
}

/// Whether `pi` answers on this machine.
pub fn pi_available() -> bool {
    crate::platform::host::on_path("pi")
}

/// Run `pi <args>` from the home directory (so only the personal scope is
/// read or written), returning its last lines on failure.
pub async fn run_pi(args: &[&str]) -> Result<()> {
    let mut command = tokio::process::Command::new("pi");
    command
        .args(args)
        .env("NO_COLOR", "1")
        .env("FORCE_COLOR", "0")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    if let Some(home) = dirs::home_dir() {
        command.current_dir(home);
    }
    let verb = args.first().copied().unwrap_or("");
    let output = tokio::time::timeout(PI_TIMEOUT, command.output())
        .await
        .map_err(|_| anyhow!("pi {verb} didn't finish within 10 minutes"))?
        .map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => anyhow!("Pi isn't installed (no `pi` on PATH)"),
            _ => anyhow!("couldn't run pi: {err}"),
        })?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let text = if stderr.trim().is_empty() {
        stdout
    } else {
        stderr
    };
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = lines[lines.len().saturating_sub(4)..].join("\n");
    if tail.is_empty() {
        bail!("pi {verb} failed ({})", output.status)
    }
    bail!("{tail}")
}

/// The spec `pi install` gets: a local directory as an absolute path, since
/// Pi resolves relative ones against its settings file.
fn pi_spec(source: &Source) -> String {
    match source {
        Source::Local(path) => std::fs::canonicalize(path)
            .unwrap_or_else(|_| path.clone())
            .display()
            .to_string(),
        other => other.spec(),
    }
}

/// Pi's personal package list (`<agent dir>/settings.json`), for `list`.
pub fn pi_packages() -> Vec<String> {
    let dir = std::env::var_os("PI_CODING_AGENT_DIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".pi").join("agent")));
    let Some(text) = dir.and_then(|d| std::fs::read_to_string(d.join("settings.json")).ok()) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    value
        .get("packages")
        .and_then(|p| p.as_array())
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| match entry {
                    serde_json::Value::String(s) => Some(s.trim().to_string()),
                    serde_json::Value::Object(o) => {
                        o.get("source")?.as_str().map(|s| s.trim().to_string())
                    }
                    _ => None,
                })
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// One harness's half of an install or remove.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Outcome {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The Wizard side's plugin record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<Plugin>,
    /// The Pi side's spec.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

impl Outcome {
    fn failed(err: anyhow::Error) -> Self {
        Self {
            ok: false,
            error: Some(format!("{err:#}")),
            plugin: None,
            source: None,
        }
    }
}

/// What `install --json` and `remove --json` print.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub schema: u32,
    pub beta: bool,
    pub ok: bool,
    pub target: Target,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wizard: Option<Outcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pi: Option<Outcome>,
}

impl Report {
    fn new(target: Target, wizard: Option<Outcome>, pi: Option<Outcome>) -> Self {
        let ok = wizard.iter().chain(pi.iter()).all(|o| o.ok);
        Self {
            schema: SCHEMA,
            beta: true,
            ok,
            target,
            wizard,
            pi,
        }
    }
}

/// Install for `target`. Wizard goes first, so a spec that doesn't fetch is
/// reported before Pi is touched; each side's result stands on its own.
pub async fn install(spec: &str, target: Target) -> Report {
    let wizard = if target.wizard() {
        Some(match install_wizard(spec).await {
            Ok(plugin) => Outcome {
                ok: true,
                error: None,
                plugin: Some(plugin),
                source: None,
            },
            Err(err) => Outcome::failed(err),
        })
    } else {
        None
    };
    let pi = if target.pi() {
        Some(match source::parse(spec) {
            Err(err) => Outcome::failed(err),
            Ok(source) => {
                let spec = pi_spec(&source);
                match run_pi(&["install", &spec]).await {
                    Ok(()) => Outcome {
                        ok: true,
                        error: None,
                        plugin: None,
                        source: Some(spec),
                    },
                    Err(err) => Outcome::failed(err),
                }
            }
        })
    } else {
        None
    };
    Report::new(target, wizard, pi)
}

/// Remove for `target`. `query` is the plugin's name or its source; for Pi a
/// bare name means the npm package, unless Wizard's record knows the source.
pub async fn remove(query: &str, target: Target) -> Report {
    let known = wizard_home()
        .ok()
        .and_then(|home| Store::load(&home).ok())
        .and_then(|store| {
            store
                .position(query)
                .map(|i| store.plugins[i].source.clone())
        });
    let wizard = target.wizard().then(|| match remove_wizard(query) {
        Ok(plugin) => Outcome {
            ok: true,
            error: None,
            plugin: Some(plugin),
            source: None,
        },
        Err(err) => Outcome::failed(err),
    });
    let pi = if target.pi() {
        let spec = known.unwrap_or_else(|| {
            if query.contains(':') || query.starts_with(['.', '/', '~']) {
                query.to_string()
            } else {
                format!("npm:{query}")
            }
        });
        Some(match run_pi(&["remove", &spec]).await {
            Ok(()) => Outcome {
                ok: true,
                error: None,
                plugin: None,
                source: Some(spec),
            },
            Err(err) => Outcome::failed(err),
        })
    } else {
        None
    };
    Report::new(target, wizard, pi)
}

/* ---------------------------------------------------------------------- */
/* Text                                                                   */
/* ---------------------------------------------------------------------- */

/// One plugin's items, one per line, for the terminal and the TUI.
pub fn describe_items(plugin: &Plugin) -> String {
    let mut out = String::new();
    if plugin.items.is_empty() {
        out.push_str(&format!("  nothing found. {NOTHING_FOUND}\n"));
    }
    for item in &plugin.items {
        let label = match item.kind {
            ItemKind::Prompt => format!("/{}", item.name),
            _ => item.name.clone(),
        };
        let state = match (item.status, item.path.as_deref()) {
            (ItemStatus::Installed, Some(path)) => format!("~/.wizard/{path}"),
            (ItemStatus::Ready, Some(path)) => format!("-> ~/.wizard/{path}"),
            _ => item.reason.clone().unwrap_or_default(),
        };
        out.push_str(&format!(
            "  {:<9} {:<28} {state}\n",
            format!("{:?}", item.kind).to_lowercase(),
            label
        ));
        for note in &item.notes {
            out.push_str(&format!("  {:<9} {:<28} note: {note}\n", "", ""));
        }
    }
    for note in &plugin.notes {
        out.push_str(&format!("  note: {note}\n"));
    }
    out
}

/// The terminal's account of an install or remove.
pub fn describe_report(report: &Report, verb: &str) -> String {
    let mut out = String::new();
    if let Some(wizard) = &report.wizard {
        match (&wizard.plugin, &wizard.error) {
            (Some(plugin), _) => {
                let version = plugin
                    .version
                    .as_deref()
                    .map(|v| format!(" {v}"))
                    .unwrap_or_default();
                out.push_str(&format!("{BETA_NOTE}\n"));
                out.push_str(&format!("{verb} {}{version} for Wizard\n", plugin.name));
                out.push_str(&describe_items(plugin));
                if verb == "installed" {
                    let (works, _) = plugin.summary();
                    if works.is_empty() {
                        out.push_str("nothing in this package runs in Wizard yet\n");
                    } else {
                        out.push_str(
                            "new sessions load them; /reload picks them up in a running one\n",
                        );
                    }
                }
            }
            (None, error) => out.push_str(&format!(
                "Wizard: {}\n",
                error.as_deref().unwrap_or("failed")
            )),
        }
    }
    if let Some(pi) = &report.pi {
        match (&pi.source, &pi.error) {
            (Some(source), None) => out.push_str(&format!("{verb} {source} for Pi\n")),
            (_, error) => out.push_str(&format!("Pi: {}\n", error.as_deref().unwrap_or("failed"))),
        }
    }
    out
}

fn print_json<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

/// The `--json` shape of a failure that happened before any harness ran.
fn json_error(err: &anyhow::Error) -> serde_json::Value {
    serde_json::json!({ "schema": SCHEMA, "beta": true, "ok": false, "error": format!("{err:#}") })
}

/// `wizard plugins ...`.
pub async fn run_cli(cmd: PluginsCmd) -> Result<i32> {
    let json = match &cmd {
        PluginsCmd::Search { json, .. }
        | PluginsCmd::Inspect { json, .. }
        | PluginsCmd::Install { json, .. }
        | PluginsCmd::List { json }
        | PluginsCmd::Remove { json, .. } => *json,
    };
    match run(cmd).await {
        Ok(code) => Ok(code),
        Err(err) if json => {
            print_json(&json_error(&err))?;
            Ok(1)
        }
        Err(err) => Err(err),
    }
}

async fn run(cmd: PluginsCmd) -> Result<i32> {
    match cmd {
        PluginsCmd::Search { query, json } => {
            let hits = search(&query.join(" "), 25).await?;
            if json {
                print_json(&serde_json::json!({
                    "schema": SCHEMA, "beta": true, "ok": true, "results": hits,
                }))?;
            } else if hits.is_empty() {
                println!("no Pi packages match that search");
            } else {
                for hit in &hits {
                    let downloads = hit
                        .weekly_downloads
                        .map(|n| format!("  {n}/wk"))
                        .unwrap_or_default();
                    println!("{} {}{downloads}", hit.name, hit.version);
                    if let Some(description) = &hit.description {
                        println!("  {description}");
                    }
                }
                println!(
                    "\ninstall one with: wizard plugins install <name> [--for wizard|pi|both]"
                );
            }
            Ok(0)
        }
        PluginsCmd::Inspect { spec, json } => {
            let plugin = inspect(&spec).await?;
            if json {
                print_json(&serde_json::json!({
                    "schema": SCHEMA, "beta": true, "ok": true, "plugin": plugin,
                }))?;
            } else {
                println!("{BETA_NOTE}");
                match plugin.version.as_deref() {
                    Some(version) => println!("{} {version}", plugin.name),
                    None => println!("{}", plugin.name),
                }
                if let Some(description) = &plugin.description {
                    println!("{description}");
                }
                print!("{}", describe_items(&plugin));
            }
            Ok(0)
        }
        PluginsCmd::Install { spec, target, json } => {
            let report = install(&spec, target).await;
            if json {
                print_json(&report)?;
            } else {
                print!("{}", describe_report(&report, "installed"));
            }
            Ok(if report.ok { 0 } else { 1 })
        }
        PluginsCmd::Remove { name, target, json } => {
            let report = remove(&name, target).await;
            if json {
                print_json(&report)?;
            } else {
                print!("{}", describe_report(&report, "removed"));
            }
            Ok(if report.ok { 0 } else { 1 })
        }
        PluginsCmd::List { json } => {
            let plugins = list_wizard()?;
            let pi = pi_packages();
            if json {
                print_json(&serde_json::json!({
                    "schema": SCHEMA,
                    "beta": true,
                    "ok": true,
                    "plugins": plugins,
                    "pi": { "available": pi_available(), "packages": pi },
                }))?;
                return Ok(0);
            }
            println!("{BETA_NOTE}");
            if plugins.is_empty() {
                println!("no Pi plugins installed for Wizard");
            }
            for plugin in &plugins {
                let version = plugin
                    .version
                    .as_deref()
                    .map(|v| format!(" {v}"))
                    .unwrap_or_default();
                println!("\n{}{version}  ({})", plugin.name, plugin.source);
                for item in &plugin.items {
                    let label = match item.kind {
                        ItemKind::Prompt => format!("/{}", item.name),
                        _ => item.name.clone(),
                    };
                    let state = match (item.status, item.loaded) {
                        (ItemStatus::Installed, Some(true)) => "loaded".to_string(),
                        (ItemStatus::Installed, _) => {
                            "installed, but Wizard resolves this name elsewhere".to_string()
                        }
                        _ => item.reason.clone().unwrap_or_default(),
                    };
                    println!(
                        "  {:<9} {:<28} {state}",
                        format!("{:?}", item.kind).to_lowercase(),
                        label
                    );
                }
            }
            if !pi.is_empty() {
                println!("\nPi packages:");
                for source in &pi {
                    println!("  {source}");
                }
            }
            Ok(0)
        }
    }
}
