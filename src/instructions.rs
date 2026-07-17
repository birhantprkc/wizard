//! Hierarchical project instructions.
//!
//! Wizard composes its "project instructions" prompt section from every
//! directory between the filesystem root and the project root: in each
//! directory the first of `WIZARD.md` > `AGENTS.md` > `CLAUDE.md` is taken,
//! plus the global `~/.wizard/WIZARD.md`. Files are concatenated outermost
//! first (global, then root-down), so the project root's file has the last
//! word. Each file may pull in extra context with `@relative/path` lines
//! (one level deep, capped per include); the assembled block is capped as a
//! whole so a sprawling hierarchy cannot flood the context window.

use std::path::{Path, PathBuf};

use crate::tools::truncate_output;

/// Instruction file names, in per-directory priority order (the first one
/// that exists wins for that directory).
const FILE_NAMES: [&str; 3] = ["WIZARD.md", "AGENTS.md", "CLAUDE.md"];

/// Byte cap applied to each `@path` include.
const INCLUDE_CAP: usize = 10_000;

/// Byte cap applied to the fully assembled instruction block.
const TOTAL_CAP: usize = 40_000;

/// Load the full instruction hierarchy for `project_root`: the global
/// `~/.wizard/WIZARD.md` plus one instruction file per ancestor directory,
/// concatenated outermost-first. `None` when nothing exists.
pub fn load(project_root: &Path) -> Option<String> {
    let global = crate::config::Config::wizard_dir()
        .ok()
        .map(|dir| dir.join("WIZARD.md"));
    load_with_global(project_root, global.as_deref())
}

/// Testable core of [`load`]: `global` is the path of the global
/// instruction file (normally `~/.wizard/WIZARD.md`), checked first.
fn load_with_global(project_root: &Path, global: Option<&Path>) -> Option<String> {
    let mut files: Vec<PathBuf> = Vec::new();
    if let Some(global) = global
        && global.is_file()
    {
        files.push(global.to_path_buf());
    }

    // ancestors() walks project root → filesystem root; reverse so the
    // outermost file comes first and the project root's file last.
    let mut chain: Vec<PathBuf> = project_root
        .ancestors()
        .filter_map(first_instruction_file)
        .collect();
    chain.reverse();
    // The global file may also sit on the ancestor chain (project under
    // ~/.wizard); never include it twice.
    chain.retain(|path| files.first() != Some(path));
    files.extend(chain);

    // Read everything first (still outermost-first order), then budget from
    // the INNERMOST file outward: the project root's own instructions have
    // the highest priority, so when TOTAL_CAP hits it is the outer files
    // that get trimmed or dropped, never the innermost ones.
    let mut sections: Vec<(PathBuf, String)> = files
        .into_iter()
        .filter_map(|path| {
            let content = read_with_includes(&path)?;
            let trimmed = content.trim_end();
            (!trimmed.trim().is_empty()).then(|| (path, trimmed.to_string()))
        })
        .collect();

    let mut budget = TOTAL_CAP;
    let mut keep = vec![false; sections.len()];
    for (i, (path, content)) in sections.iter_mut().enumerate().rev() {
        let header = format!("<!-- instructions from {} -->\n", path.display());
        let overhead = header.len() + 2; // "\n\n" separator
        if budget <= overhead {
            break;
        }
        let room = budget - overhead;
        if content.len() > room {
            // This (and everything further out) does not fit whole. Trim it
            // to what remains and stop: including a smaller far-outer file
            // while dropping a nearer one would invert the priority order.
            *content = truncate_output(std::mem::take(content), room);
            keep[i] = true;
            break;
        }
        budget -= overhead + content.len();
        keep[i] = true;
    }

    let mut out = String::new();
    for (i, (path, content)) in sections.iter().enumerate() {
        if !keep[i] {
            continue;
        }
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&format!("<!-- instructions from {} -->\n", path.display()));
        out.push_str(content);
    }

    (!out.is_empty()).then_some(out)
}

/// The highest-priority instruction file present in `dir`, if any.
fn first_instruction_file(dir: &Path) -> Option<PathBuf> {
    FILE_NAMES
        .iter()
        .map(|name| dir.join(name))
        .find(|path| path.is_file())
}

/// Read one instruction file, expanding `@path` include lines one level
/// deep: a line that is exactly `@` followed by a path inlines that file
/// (resolved relative to the including file's directory, capped at
/// [`INCLUDE_CAP`]). Includes inside included files are not expanded.
/// An unreadable include keeps the `@` line verbatim; an unreadable
/// instruction file yields `None`.
fn read_with_includes(path: &Path) -> Option<String> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!("could not read {}: {err}", path.display());
            }
            return None;
        }
    };
    let dir = path.parent().unwrap_or(Path::new("."));

    let mut out = String::new();
    for line in raw.lines() {
        if let Some(target) = include_target(line) {
            let include_path = if Path::new(target).is_absolute() {
                PathBuf::from(target)
            } else {
                dir.join(target)
            };
            match std::fs::read_to_string(&include_path) {
                Ok(content) => {
                    out.push_str(&format!("<!-- include {} -->\n", include_path.display()));
                    out.push_str(truncate_output(content, INCLUDE_CAP).trim_end());
                    out.push('\n');
                    continue;
                }
                Err(err) => {
                    tracing::warn!(
                        "instruction include {} unreadable: {err}",
                        include_path.display()
                    );
                }
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    Some(out)
}

/// The include path of an `@path` line, or `None` when the line is ordinary
/// content. The whole (trimmed) line must be `@` followed by a single path —
/// `@` mid-line or a line with multiple words is left alone.
fn include_target(line: &str) -> Option<&str> {
    let target = line.trim().strip_prefix('@')?;
    if target.is_empty() || target.contains(char::is_whitespace) {
        return None;
    }
    Some(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Temp directory tree removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("wizard-instr-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().expect("has parent")).expect("mkdir");
        std::fs::write(path, content).expect("write");
    }

    #[test]
    fn hierarchy_orders_outermost_first_and_project_root_last() {
        let tmp = TempDir::new();
        let project = tmp.0.join("group").join("proj");
        write(&tmp.0.join("CLAUDE.md"), "outermost rules");
        write(&tmp.0.join("group").join("AGENTS.md"), "group rules");
        write(&project.join("WIZARD.md"), "project rules");
        let global = tmp.0.join("global").join("WIZARD.md");
        write(&global, "global rules");

        let out = load_with_global(&project, Some(&global)).expect("instructions found");
        let pos = |needle: &str| {
            out.find(needle)
                .unwrap_or_else(|| panic!("missing {needle}"))
        };
        assert!(pos("global rules") < pos("outermost rules"));
        assert!(pos("outermost rules") < pos("group rules"));
        assert!(pos("group rules") < pos("project rules"));
        // Each file is prefixed with a comment naming its path.
        assert!(out.contains(&format!(
            "<!-- instructions from {} -->",
            project.join("WIZARD.md").display()
        )));
    }

    #[test]
    fn wizard_md_beats_agents_md_beats_claude_md_per_directory() {
        let tmp = TempDir::new();
        let project = tmp.0.join("proj");
        write(&project.join("CLAUDE.md"), "claude content");
        write(&project.join("AGENTS.md"), "agents content");
        let out = load_with_global(&project, None).expect("found");
        assert!(out.contains("agents content"));
        assert!(
            !out.contains("claude content"),
            "AGENTS.md shadows CLAUDE.md"
        );

        write(&project.join("WIZARD.md"), "wizard content");
        let out = load_with_global(&project, None).expect("found");
        assert!(out.contains("wizard content"));
        assert!(
            !out.contains("agents content"),
            "WIZARD.md shadows AGENTS.md"
        );
    }

    #[test]
    fn global_file_on_the_ancestor_chain_is_included_once() {
        let tmp = TempDir::new();
        let project = tmp.0.join("proj");
        let global = project.join("WIZARD.md");
        write(&global, "solo rules");
        let out = load_with_global(&project, Some(&global)).expect("found");
        assert_eq!(out.matches("solo rules").count(), 1, "no duplicate: {out}");
    }

    #[test]
    fn missing_everything_yields_none() {
        let tmp = TempDir::new();
        let project = tmp.0.join("empty");
        std::fs::create_dir_all(&project).unwrap();
        // The walk may pick up stray files in /tmp's ancestors only if they
        // exist; the temp tree itself has none.
        let absent = tmp.0.join("nope").join("WIZARD.md");
        let out = load_with_global(&project, Some(&absent));
        // No file in the temp tree: anything found must come from outside it.
        if let Some(out) = &out {
            assert!(
                !out.contains(&tmp.0.display().to_string()),
                "nothing from the temp tree: {out}"
            );
        }
    }

    #[test]
    fn at_lines_inline_the_referenced_file_one_level_deep() {
        let tmp = TempDir::new();
        let project = tmp.0.join("proj");
        write(
            &project.join("WIZARD.md"),
            "before\n@docs/extra.md\nafter\n",
        );
        write(
            &project.join("docs").join("extra.md"),
            "extra context\n@docs/deeper.md\n",
        );
        write(&project.join("docs").join("deeper.md"), "too deep");

        let out = load_with_global(&project, None).expect("found");
        assert!(out.contains("before"));
        assert!(out.contains("extra context"), "include inlined: {out}");
        assert!(out.contains("after"));
        assert!(
            !out.contains("too deep"),
            "includes are one level deep only: {out}"
        );
        // The nested @ line survives verbatim inside the inlined content.
        assert!(out.contains("@docs/deeper.md"));
    }

    #[test]
    fn unreadable_include_keeps_the_line_verbatim() {
        let tmp = TempDir::new();
        let project = tmp.0.join("proj");
        write(&project.join("WIZARD.md"), "@missing/file.md\nrest\n");
        let out = load_with_global(&project, None).expect("found");
        assert!(out.contains("@missing/file.md"));
        assert!(out.contains("rest"));
    }

    #[test]
    fn at_mid_line_or_with_spaces_is_not_an_include() {
        assert_eq!(include_target("@docs/extra.md"), Some("docs/extra.md"));
        assert_eq!(include_target("  @docs/extra.md  "), Some("docs/extra.md"));
        assert_eq!(include_target("email me @example"), None);
        assert_eq!(include_target("@"), None);
        assert_eq!(include_target("@two words"), None);
        assert_eq!(include_target("plain"), None);
    }

    #[test]
    fn includes_are_capped_per_file() {
        let tmp = TempDir::new();
        let project = tmp.0.join("proj");
        write(&project.join("WIZARD.md"), "@big.md\n");
        write(&project.join("big.md"), &"x".repeat(INCLUDE_CAP * 2));
        let out = load_with_global(&project, None).expect("found");
        assert!(
            out.len() < INCLUDE_CAP + 1_000,
            "capped: {} bytes",
            out.len()
        );
        assert!(out.contains("[output truncated]"));
    }

    #[test]
    fn total_is_capped() {
        let tmp = TempDir::new();
        let project = tmp.0.join("a").join("b");
        write(&tmp.0.join("WIZARD.md"), &"r".repeat(TOTAL_CAP));
        write(&tmp.0.join("a").join("WIZARD.md"), &"m".repeat(TOTAL_CAP));
        write(&project.join("WIZARD.md"), "project rules win");
        let out = load_with_global(&project, None).expect("found");
        assert!(
            out.len() <= TOTAL_CAP + 200,
            "total capped: {} bytes",
            out.len()
        );
        assert!(out.contains("[output truncated]"));
    }

    #[test]
    fn cap_trims_outer_files_and_keeps_the_project_root_intact() {
        let tmp = TempDir::new();
        let project = tmp.0.join("a").join("b");
        // Outermost and middle files are each big enough to blow the cap on
        // their own; the project root's file is small and highest priority.
        write(
            &tmp.0.join("WIZARD.md"),
            &"OUTERMOST\n".repeat(TOTAL_CAP / 10),
        );
        write(
            &tmp.0.join("a").join("WIZARD.md"),
            &"MIDDLE\n".repeat(TOTAL_CAP / 7),
        );
        write(&project.join("WIZARD.md"), "project rules win");

        let out = load_with_global(&project, None).expect("found");
        assert!(
            out.contains("project rules win"),
            "the innermost file survives the cap intact"
        );
        // The middle file gets whatever budget remains (truncated); the
        // outermost is dropped entirely.
        assert!(out.contains("MIDDLE"), "middle file partially included");
        assert!(
            !out.contains("OUTERMOST"),
            "outermost file dropped: no budget left"
        );
        // Order is still outermost-first among what was kept.
        let mid = out.find("MIDDLE").expect("middle content present");
        let inner = out.find("project rules win").expect("inner present");
        assert!(mid < inner, "outer content still precedes inner content");
    }

    #[test]
    fn small_hierarchies_are_untouched_by_the_budget() {
        let tmp = TempDir::new();
        let project = tmp.0.join("proj");
        write(&tmp.0.join("WIZARD.md"), "outer rules");
        write(&project.join("WIZARD.md"), "inner rules");
        let out = load_with_global(&project, None).expect("found");
        assert!(out.contains("outer rules"));
        assert!(out.contains("inner rules"));
        assert!(!out.contains("[output truncated]"));
    }
}
