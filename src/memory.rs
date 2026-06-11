//! Persistent per-project memory under `~/.wizard/memory/<project-slug>/`.
//!
//! Each memory is one markdown file with a frontmatter header (name +
//! one-line description); `MEMORY.md` is an index regenerated from the entry
//! files on every save/delete. The index is injected into the system prompt
//! so the model can recall saved facts across sessions via the `memory`
//! tool.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::config::Config;

/// Filename of the regenerated index inside a project's memory dir.
const INDEX_FILE: &str = "MEMORY.md";

/// One saved memory, as listed in the index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryEntry {
    /// Kebab-case slug; the file is `<name>.md`.
    pub name: String,
    /// One-line summary shown in the index.
    pub description: String,
}

/// Handle to one project's memory directory. The directory is created
/// lazily on first write, so opening a store never touches the disk.
#[derive(Debug, Clone)]
pub struct MemoryStore {
    dir: PathBuf,
}

impl MemoryStore {
    /// Open the memory store for `project_root`:
    /// `~/.wizard/memory/<slug>/`, where the slug is the canonicalized root
    /// path with every non-alphanumeric character replaced by `-` (e.g.
    /// `-home-user-projects-app`).
    pub fn open(project_root: &Path) -> Result<Self> {
        let canonical = project_root
            .canonicalize()
            .unwrap_or_else(|_| project_root.to_path_buf());
        let dir = Config::memory_dir()?.join(project_slug(&canonical));
        Ok(Self { dir })
    }

    /// Directory this store reads and writes (may not exist yet).
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Write (or overwrite) memory `name` and regenerate the index.
    /// `description` is flattened to one line.
    pub fn save(&self, name: &str, description: &str, content: &str) -> Result<()> {
        validate_name(name)?;
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        let description = flatten(description);
        let body = format!(
            "---\nname: {name}\ndescription: {description}\n---\n\n{}\n",
            content.trim()
        );
        let path = self.entry_path(name);
        std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
        self.regenerate_index()
    }

    /// Full file contents of memory `name` (frontmatter included).
    pub fn read(&self, name: &str) -> Result<String> {
        validate_name(name)?;
        let path = self.entry_path(name);
        std::fs::read_to_string(&path)
            .with_context(|| format!("no memory named '{name}' ({})", path.display()))
    }

    /// Remove memory `name` and regenerate the index.
    pub fn delete(&self, name: &str) -> Result<()> {
        validate_name(name)?;
        let path = self.entry_path(name);
        match std::fs::remove_file(&path) {
            Ok(()) => self.regenerate_index(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                bail!("no memory named '{name}' ({})", path.display())
            }
            Err(err) => Err(err).with_context(|| format!("removing {}", path.display())),
        }
    }

    /// All saved memories (name + description), sorted by name. An absent
    /// memory dir simply means no memories yet.
    pub fn list(&self) -> Result<Vec<MemoryEntry>> {
        let dir = match std::fs::read_dir(&self.dir) {
            Ok(dir) => dir,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => {
                return Err(err).with_context(|| format!("reading {}", self.dir.display()));
            }
        };
        let mut entries = Vec::new();
        for entry in dir {
            let path = entry?.path();
            let Some(stem) = path.file_stem().map(|stem| stem.to_string_lossy()) else {
                continue;
            };
            if path.extension().is_none_or(|ext| ext != "md")
                || path.file_name().is_some_and(|file| file == INDEX_FILE)
            {
                continue;
            }
            let contents = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            entries.push(MemoryEntry {
                name: stem.into_owned(),
                description: parse_description(&contents).unwrap_or_default(),
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    /// Contents of `MEMORY.md`, or `None` when it is absent or empty.
    pub fn index(&self) -> Result<Option<String>> {
        let path = self.dir.join(INDEX_FILE);
        match std::fs::read_to_string(&path) {
            Ok(contents) if contents.trim().is_empty() => Ok(None),
            Ok(contents) => Ok(Some(contents)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
        }
    }

    fn entry_path(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.md"))
    }

    /// Rebuild `MEMORY.md` from the entry files: one
    /// `- [name](name.md) — description` line per memory.
    fn regenerate_index(&self) -> Result<()> {
        let mut index = String::new();
        for entry in self.list()? {
            index.push_str(&format!(
                "- [{0}]({0}.md) — {1}\n",
                entry.name, entry.description
            ));
        }
        let path = self.dir.join(INDEX_FILE);
        std::fs::write(&path, index).with_context(|| format!("writing {}", path.display()))
    }
}

/// Project root path → directory slug: every character that is not ASCII
/// alphanumeric becomes `-` (so `/home/user/app` → `-home-user-app`).
fn project_slug(root: &Path) -> String {
    root.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Reject anything that is not a kebab-case slug. This doubles as path
/// traversal protection: `/`, `\`, and `.` are not in the allowed set.
fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("memory name must not be empty");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        bail!("memory name '{name}' must be kebab-case (lowercase letters, digits, and hyphens)");
    }
    Ok(())
}

/// Collapse a description to a single trimmed line for the frontmatter and
/// the index.
fn flatten(description: &str) -> String {
    description.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Pull `description:` out of the frontmatter block of an entry file.
fn parse_description(contents: &str) -> Option<String> {
    let mut lines = contents.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    for line in lines {
        if line.trim() == "---" {
            break;
        }
        if let Some(rest) = line.strip_prefix("description:") {
            return Some(rest.trim().to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Temp memory dir removed on drop.
    struct TempStore {
        store: MemoryStore,
    }

    impl TempStore {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("wizard-test-{}", uuid::Uuid::new_v4()));
            Self {
                store: MemoryStore { dir },
            }
        }
    }

    impl Drop for TempStore {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.store.dir());
        }
    }

    #[test]
    fn save_read_delete_round_trip() {
        let tmp = TempStore::new();
        let store = &tmp.store;
        store
            .save(
                "build-system",
                "uses cargo with lto",
                "Release builds use lto = true.",
            )
            .unwrap();

        let contents = store.read("build-system").unwrap();
        assert!(contents.starts_with("---\nname: build-system\n"));
        assert!(contents.contains("description: uses cargo with lto"));
        assert!(contents.contains("Release builds use lto = true."));

        let entries = store.list().unwrap();
        assert_eq!(
            entries,
            [MemoryEntry {
                name: "build-system".to_string(),
                description: "uses cargo with lto".to_string(),
            }]
        );

        store.delete("build-system").unwrap();
        assert!(store.list().unwrap().is_empty());
        assert!(
            store.read("build-system").is_err(),
            "deleted memory is gone"
        );
    }

    #[test]
    fn index_is_regenerated_on_save_and_delete() {
        let tmp = TempStore::new();
        let store = &tmp.store;
        assert_eq!(store.index().unwrap(), None, "no index before first save");

        store.save("alpha", "first fact", "A.").unwrap();
        store.save("beta", "second fact", "B.").unwrap();
        let index = store.index().unwrap().expect("index exists");
        assert_eq!(
            index,
            "- [alpha](alpha.md) — first fact\n- [beta](beta.md) — second fact\n"
        );

        store.delete("alpha").unwrap();
        let index = store.index().unwrap().expect("index still exists");
        assert_eq!(index, "- [beta](beta.md) — second fact\n");

        store.delete("beta").unwrap();
        assert_eq!(store.index().unwrap(), None, "empty index reads as None");
    }

    #[test]
    fn save_overwrites_without_duplicating_index_lines() {
        let tmp = TempStore::new();
        let store = &tmp.store;
        store.save("pref", "old description", "old").unwrap();
        store.save("pref", "new description", "new").unwrap();

        let entries = store.list().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].description, "new description");
        let index = store.index().unwrap().expect("index exists");
        assert_eq!(index.lines().count(), 1);
    }

    #[test]
    fn multiline_descriptions_are_flattened() {
        let tmp = TempStore::new();
        let store = &tmp.store;
        store.save("style", "line one\nline two", "body").unwrap();
        assert_eq!(store.list().unwrap()[0].description, "line one line two");
    }

    #[test]
    fn names_must_be_kebab_case() {
        let tmp = TempStore::new();
        let store = &tmp.store;
        for bad in ["", "../evil", "UPPER case", "with space", "dot.md", "a/b"] {
            assert!(store.save(bad, "d", "c").is_err(), "must reject '{bad}'");
            assert!(store.read(bad).is_err(), "read must reject '{bad}'");
            assert!(store.delete(bad).is_err(), "delete must reject '{bad}'");
        }
        store.save("kebab-case-2", "fine", "ok").unwrap();
    }

    #[test]
    fn delete_missing_memory_is_a_clear_error() {
        let tmp = TempStore::new();
        let err = tmp.store.delete("nope").expect_err("missing must fail");
        assert!(err.to_string().contains("no memory named 'nope'"));
    }

    #[test]
    fn slug_replaces_non_alphanumerics() {
        assert_eq!(
            project_slug(Path::new("/home/user/projects/my_app")),
            "-home-user-projects-my-app"
        );
    }
}
