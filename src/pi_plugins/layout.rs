//! What is inside a Pi package, and which parts Wizard can use.
//!
//! Discovery follows Pi's own package manager: a `pi` object in
//! `package.json` lists each resource type's paths and globs (with `!`, `+`
//! and `-` overrides), and a package without one is read by convention from
//! `extensions/`, `skills/`, `prompts/` and `themes/`. A directory holding a
//! `SKILL.md` is one skill; a loose `.md` directly under a skills root is a
//! one-file skill; prompts are every `.md` under a prompts root; extensions
//! are `index.ts`/`index.js` or the `.ts`/`.js` files of an extensions root.
//!
//! Mapping is where the formats meet. Skills share the Agent Skills
//! `SKILL.md` format, so they copy over whole (scripts and references come
//! with them). Prompt templates become Wizard custom commands, marked
//! `syntax: pi` so Pi's placeholders (`$@`, `${1:-default}`, `${@:2}`) keep
//! their meaning. Extensions are TypeScript written against Pi's runtime and
//! themes are Pi's TUI colors: Wizard runs neither, and says so per item.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use globset::{Glob, GlobMatcher};
use serde::{Deserialize, Serialize};

/// What a Pi package holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemKind {
    Skill,
    Prompt,
    Extension,
    Theme,
}

impl ItemKind {
    pub const ALL: [ItemKind; 4] = [
        ItemKind::Skill,
        ItemKind::Prompt,
        ItemKind::Extension,
        ItemKind::Theme,
    ];

    /// The `package.json` `pi` key and conventional directory.
    pub fn key(self) -> &'static str {
        match self {
            ItemKind::Skill => "skills",
            ItemKind::Prompt => "prompts",
            ItemKind::Extension => "extensions",
            ItemKind::Theme => "themes",
        }
    }

    pub fn noun(self, count: usize) -> &'static str {
        match (self, count == 1) {
            (ItemKind::Skill, true) => "skill",
            (ItemKind::Skill, false) => "skills",
            (ItemKind::Prompt, true) => "prompt",
            (ItemKind::Prompt, false) => "prompts",
            (ItemKind::Extension, true) => "extension",
            (ItemKind::Extension, false) => "extensions",
            (ItemKind::Theme, true) => "theme",
            (ItemKind::Theme, false) => "themes",
        }
    }
}

/// Where one item stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemStatus {
    /// Wizard can use it; an install will write it.
    Ready,
    /// Written under `~/.wizard`.
    Installed,
    /// Wizard can't run this kind of item yet.
    Unsupported,
    /// Wizard could use it, but it was left alone (a name clash, a bad name).
    Skipped,
}

/// One resource of a package and what Wizard does with it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    pub kind: ItemKind,
    /// Skill name, command name, or the file's path in the package.
    pub name: String,
    pub status: ItemStatus,
    /// Why it is unsupported or skipped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Differences worth knowing about for an item that does work.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// Path inside the package, `/`-separated.
    pub from: String,
    /// Where it was written, relative to `~/.wizard`, `/`-separated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// `list` only: whether Wizard's skill loader resolves this name to this
    /// install (false when another root shadows it or the files are gone).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loaded: Option<bool>,
}

/// What an install does for one [`ItemStatus::Ready`] item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Copy this skill directory to `skills/<name>/`.
    CopyDir(PathBuf),
    /// Write this text to the item's path.
    Write(String),
}

/// An item plus how to install it.
#[derive(Debug, Clone)]
pub struct Planned {
    pub item: Item,
    pub action: Option<Action>,
}

/// `package.json` fields the installer reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackageInfo {
    pub name: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
    /// It declares runtime `dependencies`, which Pi installs and Wizard does
    /// not.
    pub has_dependencies: bool,
    /// The `pi` manifest's resource lists, when there is one.
    pub manifest: Option<Manifest>,
}

/// The `pi` key: per resource type, the entries it declares (`None` for a
/// type it leaves out, which then contributes nothing).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Manifest {
    pub entries: [Option<Vec<String>>; 4],
}

impl Manifest {
    fn get(&self, kind: ItemKind) -> Option<&Vec<String>> {
        let index = ItemKind::ALL.iter().position(|k| *k == kind)?;
        self.entries[index].as_ref()
    }
}

pub fn read_package_info(root: &Path) -> PackageInfo {
    let Some(value) = std::fs::read_to_string(root.join("package.json"))
        .ok()
        .and_then(|text| {
            serde_json::from_str::<serde_json::Value>(text.trim_start_matches('\u{feff}')).ok()
        })
    else {
        return PackageInfo::default();
    };
    let text = |key: &str| {
        value
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    let manifest = value.get("pi").and_then(|pi| pi.as_object()).map(|pi| {
        let mut manifest = Manifest::default();
        for (index, kind) in ItemKind::ALL.iter().enumerate() {
            manifest.entries[index] = pi
                .get(kind.key())
                .and_then(|v| v.as_array())
                .filter(|list| list.iter().all(|entry| entry.is_string()))
                .map(|list| {
                    list.iter()
                        .filter_map(|entry| entry.as_str().map(str::to_string))
                        .collect()
                });
        }
        manifest
    });
    PackageInfo {
        name: text("name"),
        version: text("version"),
        description: text("description"),
        has_dependencies: value
            .get("dependencies")
            .and_then(|d| d.as_object())
            .is_some_and(|d| !d.is_empty()),
        manifest,
    }
}

/// Every resource file of `kind` the package exposes, sorted. For skills
/// these are `SKILL.md` files and loose one-file skills.
pub fn discover(root: &Path, info: &PackageInfo, kind: ItemKind) -> Vec<PathBuf> {
    let mut files = match &info.manifest {
        Some(manifest) => match manifest.get(kind) {
            Some(entries) => from_manifest(root, entries, kind),
            None => Vec::new(),
        },
        None => {
            let dir = root.join(kind.key());
            if dir.is_dir() {
                collect(&dir, kind)
            } else {
                Vec::new()
            }
        }
    };
    files.sort();
    files.dedup();
    files
}

fn from_manifest(root: &Path, entries: &[String], kind: ItemKind) -> Vec<PathBuf> {
    let is_override = |e: &str| e.starts_with(['!', '+', '-']);
    let mut paths = Vec::new();
    for entry in entries.iter().filter(|e| !is_override(e)) {
        if entry.contains(['*', '?']) {
            paths.extend(expand_glob(root, entry));
        } else if let Some(path) = inside(root, entry) {
            paths.push(path);
        }
    }
    let mut files = Vec::new();
    for path in paths {
        if path.is_file() {
            files.push(path);
        } else if path.is_dir() {
            files.extend(collect(&path, kind));
        }
    }
    let overrides: Vec<&str> = entries
        .iter()
        .map(String::as_str)
        .filter(|e| is_override(e))
        .collect();
    apply_overrides(files, &overrides, root)
}

/// `root/entry`, unless it climbs out of the package.
fn inside(root: &Path, entry: &str) -> Option<PathBuf> {
    let entry = entry.strip_prefix("./").unwrap_or(entry);
    let path = Path::new(entry);
    if path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return None;
    }
    Some(root.join(path))
}

fn matcher(pattern: &str) -> Option<GlobMatcher> {
    let pattern = pattern.strip_prefix("./").unwrap_or(pattern);
    Glob::new(pattern).ok().map(|glob| glob.compile_matcher())
}

/// Paths under `root` a manifest glob matches, skipping dot segments the
/// way Pi's `globSync` filter does.
fn expand_glob(root: &Path, pattern: &str) -> Vec<PathBuf> {
    let Some(glob) = matcher(pattern) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    walk(root, root, &mut |path, rel| {
        if glob.is_match(rel) {
            out.push(path.to_path_buf());
        }
    });
    out
}

/// Visit every non-dot path under `dir` (outside `node_modules`), files and
/// directories both, with its `/`-separated path relative to `root`.
fn walk(root: &Path, dir: &Path, visit: &mut dyn FnMut(&Path, &str)) {
    for path in entries(dir) {
        let rel = posix(path.strip_prefix(root).unwrap_or(&path));
        visit(&path, &rel);
        if path.is_dir() {
            walk(root, &path, visit);
        }
    }
}

/// A directory's entries, sorted, without dot files, `node_modules` or
/// symlinks (a link could point anywhere on the machine).
fn entries(dir: &Path) -> Vec<PathBuf> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = read
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            !name.starts_with('.')
                && name != "node_modules"
                && entry.file_type().is_ok_and(|t| !t.is_symlink())
        })
        .map(|entry| entry.path())
        .collect();
    out.sort();
    out
}

pub(crate) fn posix(path: &Path) -> String {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Pi's `collectResourceFiles` for one directory.
fn collect(dir: &Path, kind: ItemKind) -> Vec<PathBuf> {
    match kind {
        ItemKind::Skill => skill_entries(dir, dir),
        ItemKind::Extension => extension_entries(dir),
        ItemKind::Prompt => files_with(dir, "md"),
        ItemKind::Theme => files_with(dir, "json"),
    }
}

fn skill_entries(dir: &Path, root: &Path) -> Vec<PathBuf> {
    let manifest = dir.join("SKILL.md");
    if manifest.is_file() {
        return vec![manifest];
    }
    let mut out = Vec::new();
    for path in entries(dir) {
        if path.is_dir() {
            out.extend(skill_entries(&path, root));
        } else if dir == root && path.extension().is_some_and(|e| e == "md") {
            out.push(path);
        }
    }
    out
}

fn extension_entry(dir: &Path) -> Option<Vec<PathBuf>> {
    let info = read_package_info(dir);
    if let Some(entries) = info
        .manifest
        .as_ref()
        .and_then(|m| m.get(ItemKind::Extension))
    {
        let found: Vec<PathBuf> = entries
            .iter()
            .filter_map(|e| inside(dir, e))
            .filter(|p| p.exists())
            .collect();
        if !found.is_empty() {
            return Some(found);
        }
    }
    ["index.ts", "index.js"]
        .iter()
        .map(|name| dir.join(name))
        .find(|p| p.is_file())
        .map(|p| vec![p])
}

fn extension_entries(dir: &Path) -> Vec<PathBuf> {
    if let Some(found) = extension_entry(dir) {
        return found;
    }
    let mut out = Vec::new();
    for path in entries(dir) {
        if path.is_file() && path.extension().is_some_and(|e| e == "ts" || e == "js") {
            out.push(path);
        } else if path.is_dir()
            && let Some(found) = extension_entry(&path)
        {
            out.extend(found);
        }
    }
    out
}

fn files_with(dir: &Path, extension: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for path in entries(dir) {
        if path.is_dir() {
            out.extend(files_with(&path, extension));
        } else if path.extension().is_some_and(|e| e == extension) {
            out.push(path);
        }
    }
    out
}

/// Pi's `applyPatterns` with no plain includes: `!glob` drops matches,
/// `+path` puts an exact path back, `-path` drops an exact path.
fn apply_overrides(files: Vec<PathBuf>, overrides: &[&str], root: &Path) -> Vec<PathBuf> {
    if overrides.is_empty() {
        return files;
    }
    let excludes: Vec<GlobMatcher> = overrides
        .iter()
        .filter_map(|o| o.strip_prefix('!'))
        .filter_map(matcher)
        .collect();
    let exact = |prefix: char| -> Vec<String> {
        overrides
            .iter()
            .filter_map(|o| o.strip_prefix(prefix))
            .map(|o| o.strip_prefix("./").unwrap_or(o).to_string())
            .collect()
    };
    let force_in = exact('+');
    let force_out = exact('-');
    // The names a pattern may match a file by: its path in the package, its
    // file name, and for a skill its directory's path and name too.
    let names = |file: &Path| -> Vec<String> {
        let rel = posix(file.strip_prefix(root).unwrap_or(file));
        let mut names = vec![rel];
        if let Some(name) = file.file_name() {
            names.push(name.to_string_lossy().into_owned());
        }
        if file.file_name().is_some_and(|n| n == "SKILL.md")
            && let Some(parent) = file.parent()
        {
            names.push(posix(parent.strip_prefix(root).unwrap_or(parent)));
            if let Some(name) = parent.file_name() {
                names.push(name.to_string_lossy().into_owned());
            }
        }
        names
    };
    let exact_hit = |file: &Path, list: &[String]| {
        let rel = posix(file.strip_prefix(root).unwrap_or(file));
        let parent = (file.file_name().is_some_and(|n| n == "SKILL.md"))
            .then(|| file.parent())
            .flatten()
            .map(|p| posix(p.strip_prefix(root).unwrap_or(p)));
        list.iter()
            .any(|p| *p == rel || parent.as_deref() == Some(p.as_str()))
    };
    let mut kept: Vec<PathBuf> = files
        .iter()
        .filter(|file| {
            let names = names(file);
            !excludes
                .iter()
                .any(|glob| names.iter().any(|n| glob.is_match(n)))
        })
        .cloned()
        .collect();
    for file in &files {
        if !kept.contains(file) && exact_hit(file, &force_in) {
            kept.push(file.clone());
        }
    }
    kept.retain(|file| !exact_hit(file, &force_out));
    kept
}

/// A skill or command name that is safe as one path component.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with(['.', '-'])
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Top-level `key: value` pairs of a `---` frontmatter block, raw. Used for
/// the Pi-only keys [`crate::skills::SkillMeta`] doesn't carry.
pub fn frontmatter_pairs(raw: &str) -> Vec<(String, String)> {
    let mut lines = raw.lines();
    if lines.next().map(str::trim_end) != Some("---") {
        return Vec::new();
    }
    let mut out = Vec::new();
    for line in lines {
        if line.trim_end() == "---" {
            return out;
        }
        if line.starts_with([' ', '\t']) || line.trim_start().starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once(':') {
            let value = value.trim().trim_matches(['"', '\'']);
            out.push((key.trim().to_string(), value.to_string()));
        }
    }
    Vec::new()
}

fn truthy(value: &str) -> bool {
    matches!(value.to_ascii_lowercase().as_str(), "true" | "yes" | "1")
}

/// How one skill maps into `~/.wizard/skills/`.
pub fn plan_skill(root: &Path, file: &Path) -> Planned {
    let from = posix(file.strip_prefix(root).unwrap_or(file));
    let standalone = file.file_name().is_none_or(|n| n != "SKILL.md");
    let raw = std::fs::read_to_string(file).unwrap_or_default();
    let (meta, _body) = crate::skills::split_frontmatter(&raw);
    let fallback = if standalone {
        file.file_stem()
    } else {
        file.parent().and_then(Path::file_name)
    }
    .map(|n| n.to_string_lossy().into_owned())
    .unwrap_or_default();
    let name = meta
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .unwrap_or(fallback);
    let mut item = Item {
        kind: ItemKind::Skill,
        name: name.clone(),
        status: ItemStatus::Ready,
        reason: None,
        notes: skill_notes(&raw, meta.description.as_deref()),
        from,
        path: Some(format!("skills/{name}")),
        loaded: None,
    };
    if !valid_name(&name) {
        item.status = ItemStatus::Skipped;
        item.reason = Some(format!(
            "`{name}` isn't a usable skill directory name (letters, digits, - _ . only)"
        ));
        item.path = None;
        return Planned { item, action: None };
    }
    let action = if standalone {
        Action::Write(raw)
    } else {
        Action::CopyDir(file.parent().unwrap_or(root).to_path_buf())
    };
    Planned {
        item,
        action: Some(action),
    }
}

/// What reads differently in Wizard for a skill that otherwise works.
fn skill_notes(raw: &str, description: Option<&str>) -> Vec<String> {
    let pairs = frontmatter_pairs(raw);
    let has = |key: &str| {
        pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    };
    let mut notes = Vec::new();
    if description.is_none_or(|d| d.trim().is_empty()) {
        notes.push(
            "no description: Pi won't load it, and Wizard lists it to the model by name only"
                .to_string(),
        );
    }
    if has("disable-model-invocation").is_some_and(truthy) {
        notes.push(
            "disable-model-invocation is Pi-only: Wizard still lists the skill to the model, \
             and there is no /skill: command"
                .to_string(),
        );
    }
    if has("always").is_some_and(truthy) {
        notes.push("always: true inlines the whole body into every Wizard prompt".to_string());
    }
    notes
}

/// How one prompt template maps to a Wizard custom command.
pub fn plan_prompt(root: &Path, file: &Path) -> Planned {
    let from = posix(file.strip_prefix(root).unwrap_or(file));
    let name = file
        .file_stem()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let item = Item {
        kind: ItemKind::Prompt,
        name: name.clone(),
        status: ItemStatus::Ready,
        reason: None,
        notes: Vec::new(),
        from,
        path: Some(format!("commands/{name}.md")),
        loaded: None,
    };
    let refuse = |mut item: Item, reason: String| {
        item.status = ItemStatus::Skipped;
        item.reason = Some(reason);
        item.path = None;
        Planned { item, action: None }
    };
    if !valid_name(&name) {
        return refuse(
            item,
            format!("`{name}` isn't a usable command name (letters, digits, - _ . only)"),
        );
    }
    if crate::commands::is_known(&name) {
        return refuse(
            item,
            format!("Wizard has a built-in /{name}, which would always win"),
        );
    }
    let raw = match std::fs::read_to_string(file) {
        Ok(raw) => raw,
        Err(err) => return refuse(item, format!("couldn't read it: {err}")),
    };
    Planned {
        action: Some(Action::Write(to_command(&raw))),
        item,
    }
}

/// A Pi prompt template rewritten as a Wizard command file: its description
/// (or Pi's fallback, the first line) and `syntax: pi` so the command expands
/// Pi's placeholders.
pub fn to_command(raw: &str) -> String {
    let (meta, body) = crate::skills::split_frontmatter(raw);
    let description = meta
        .description
        .map(|d| d.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|d| !d.is_empty())
        .or_else(|| {
            body.lines().find(|l| !l.trim().is_empty()).map(|line| {
                let line = line.trim();
                let short: String = line.chars().take(60).collect();
                if line.chars().count() > 60 {
                    format!("{short}...")
                } else {
                    short
                }
            })
        });
    let mut out = String::from("---\n");
    if let Some(description) = description {
        out.push_str("description: ");
        out.push_str(&description);
        out.push('\n');
    }
    out.push_str("syntax: pi\n---\n");
    out.push_str(&body);
    out.push('\n');
    out
}

/// An item Wizard can't run.
pub fn plan_unsupported(root: &Path, file: &Path, kind: ItemKind) -> Planned {
    let from = posix(file.strip_prefix(root).unwrap_or(file));
    let reason = match kind {
        ItemKind::Extension => "not supported by Wizard yet: Pi extensions are TypeScript \
                                written against Pi's own runtime"
            .to_string(),
        ItemKind::Theme => {
            "not supported by Wizard yet: Pi themes color Pi's TUI, and Wizard's skins \
             use a different format"
                .to_string()
        }
        _ => "not supported by Wizard yet".to_string(),
    };
    Planned {
        item: Item {
            kind,
            name: from.clone(),
            status: ItemStatus::Unsupported,
            reason: Some(reason),
            notes: Vec::new(),
            from,
            path: None,
            loaded: None,
        },
        action: None,
    }
}

/// Every item of the package at `root`, planned. Two resources that would
/// land on the same Wizard name keep the first; the rest are skipped.
pub fn plan(root: &Path, info: &PackageInfo) -> Vec<Planned> {
    let mut out = Vec::new();
    let mut taken: BTreeSet<(ItemKind, String)> = BTreeSet::new();
    for kind in ItemKind::ALL {
        for file in discover(root, info, kind) {
            let mut planned = match kind {
                ItemKind::Skill => plan_skill(root, &file),
                ItemKind::Prompt => plan_prompt(root, &file),
                _ => plan_unsupported(root, &file, kind),
            };
            if planned.item.status == ItemStatus::Ready
                && !taken.insert((kind, planned.item.name.clone()))
            {
                planned.item.status = ItemStatus::Skipped;
                planned.item.reason = Some(format!(
                    "another {} in this package is also named {}",
                    kind.noun(1),
                    planned.item.name
                ));
                planned.item.path = None;
                planned.action = None;
            }
            out.push(planned);
        }
    }
    out
}
