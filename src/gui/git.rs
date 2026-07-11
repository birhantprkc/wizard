//! Git status/diffstat/commit for the GUI's git panel.
//!
//! Shells out to `git` in the task's workspace (`tokio::process`, never the
//! server's own cwd). Semantics match the TUI's `/diff` sidebar: unstaged +
//! staged numstat merged per path, untracked files counted as pure
//! additions, and Wizard's own `.wizard/` state skipped throughout.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

/// Response shape of `GET /api/git`.
#[derive(Debug, Serialize)]
pub struct GitStatus {
    pub branch: String,
    pub dirty: bool,
    pub additions: u64,
    pub deletions: u64,
    pub files: Vec<GitFile>,
}

/// One changed file: `status` is `M` (modified/renamed), `A` (added),
/// `D` (deleted), or `?` (untracked).
#[derive(Debug, Serialize)]
pub struct GitFile {
    pub path: String,
    pub status: char,
    pub additions: u64,
    pub deletions: u64,
}

/// Compose the git panel for `root`: branch and per-file diffstat from
/// `git status --porcelain=v1 -b` plus unstaged and staged `git diff
/// --numstat`. Untracked files are invisible to `git diff`, so their line
/// counts are read from disk as pure additions.
pub async fn status(root: &Path) -> Result<GitStatus> {
    // `--untracked-files=all` lists every file inside an untracked
    // directory instead of the collapsed `dir/` entry, so new directories
    // count line additions like `git_diff_text`'s `ls-files --others` does.
    let porcelain = git_output(
        root,
        &["status", "--porcelain=v1", "-b", "--untracked-files=all"],
    )
    .await?;
    let (branch, entries) = parse_porcelain(&porcelain);

    let mut counts: HashMap<String, (u64, u64)> = HashMap::new();
    let unstaged = git_output(root, &["diff", "--numstat"]).await?;
    let staged = git_output(root, &["diff", "--numstat", "--cached"]).await?;
    for (path, additions, deletions) in parse_numstat(&unstaged)
        .into_iter()
        .chain(parse_numstat(&staged))
    {
        let entry = counts.entry(path).or_default();
        entry.0 += additions;
        entry.1 += deletions;
    }

    let mut files = Vec::new();
    let mut total = (0u64, 0u64);
    for (path, status) in entries {
        if is_wizard_state_path(&path) {
            continue;
        }
        let (additions, deletions) = if status == '?' {
            // Untracked: the whole file is an addition.
            let bytes = tokio::fs::read(root.join(&path)).await.unwrap_or_default();
            (added_lines(&bytes), 0)
        } else {
            counts.get(&path).copied().unwrap_or((0, 0))
        };
        total.0 += additions;
        total.1 += deletions;
        files.push(GitFile {
            path,
            status,
            additions,
            deletions,
        });
    }

    Ok(GitStatus {
        branch,
        dirty: !files.is_empty(),
        additions: total.0,
        deletions: total.1,
        files,
    })
}

/// `POST /api/git/commit`: stage everything and commit, returning the new
/// HEAD sha. Errors surface git's own stderr (nothing to commit, missing
/// identity, ...).
pub async fn commit(root: &Path, message: &str) -> Result<String> {
    git_output(root, &["add", "-A"]).await?;
    git_output(root, &["commit", "-m", message]).await?;
    let sha = git_output(root, &["rev-parse", "HEAD"]).await?;
    Ok(sha.trim().to_string())
}

/// Run `git <args>` in `root` and return stdout; a nonzero exit is an error
/// carrying git's stderr.
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

/// Parse `git status --porcelain=v1 -b`: the branch name from the `##`
/// header and one `(path, status)` per entry. Renames report the new path;
/// the status char folds the XY pair down to the protocol's `M|A|D|?`.
fn parse_porcelain(text: &str) -> (String, Vec<(String, char)>) {
    let mut branch = String::new();
    let mut entries = Vec::new();
    for line in text.lines() {
        if let Some(header) = line.strip_prefix("## ") {
            branch = parse_branch(header);
            continue;
        }
        if line.len() < 4 {
            continue;
        }
        let xy = &line[..2];
        let mut path = line[3..].to_string();
        // Renames/copies list `old -> new`; the new path is the live one.
        if let Some((_, new)) = path.split_once(" -> ") {
            path = new.to_string();
        }
        let status = if xy == "??" {
            '?'
        } else if xy.contains('D') {
            'D'
        } else if xy.contains('A') {
            'A'
        } else {
            'M'
        };
        entries.push((path, status));
    }
    (branch, entries)
}

/// The branch name out of a porcelain `##` header: `main...origin/main
/// [ahead 1]` → `main`, `HEAD (no branch)` → `HEAD`, `No commits yet on
/// main` → `main`.
fn parse_branch(header: &str) -> String {
    if let Some(name) = header.strip_prefix("No commits yet on ") {
        return name.to_string();
    }
    let name = header.split("...").next().unwrap_or(header);
    if name.starts_with("HEAD") {
        return "HEAD".to_string();
    }
    name.to_string()
}

/// Parse `git diff --numstat` lines (`added<TAB>deleted<TAB>path`) into
/// `(path, additions, deletions)`. Binary files report `-` counts and map
/// to zero; rename paths (`old => new`, brace form included) resolve to the
/// new path.
fn parse_numstat(text: &str) -> Vec<(String, u64, u64)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut fields = line.splitn(3, '\t');
        let (Some(added), Some(deleted), Some(path)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let additions = added.trim().parse::<u64>().unwrap_or(0);
        let deletions = deleted.trim().parse::<u64>().unwrap_or(0);
        out.push((numstat_path(path), additions, deletions));
    }
    out
}

/// Resolve a numstat rename path to the post-rename name: the brace form
/// `src/{old => new}/mod.rs` substitutes in place, the plain form
/// `old.rs => new.rs` takes the right side. Plain paths pass through.
fn numstat_path(raw: &str) -> String {
    if let (Some(open), Some(close)) = (raw.find('{'), raw.find('}'))
        && open < close
        && let Some(arrow) = raw[open..close].find(" => ")
    {
        let new = &raw[open + arrow + 4..close];
        let joined = format!("{}{}{}", &raw[..open], new, &raw[close + 1..]);
        return joined.replace("//", "/");
    }
    if let Some((_, new)) = raw.split_once(" => ") {
        return new.to_string();
    }
    raw.to_string()
}

/// Is this repo-relative path inside Wizard's own state dir (`.wizard/`)?
/// Checkpoints and snapshots are Wizard internals, not the user's changes,
/// so the git panel omits them (same rule as the TUI's `/diff`).
fn is_wizard_state_path(path: &str) -> bool {
    let path = path.replace('\\', "/");
    path == ".wizard" || path.starts_with(".wizard/") || path.contains("/.wizard/")
}

/// Lines an untracked file adds: newline count, plus one for a final
/// unterminated line. Binary content (NUL byte) counts zero, mirroring
/// numstat's `-`.
fn added_lines(bytes: &[u8]) -> u64 {
    if bytes.contains(&0) {
        return 0;
    }
    let newlines = bytes.iter().filter(|byte| **byte == b'\n').count() as u64;
    if bytes.is_empty() || bytes.ends_with(b"\n") {
        newlines
    } else {
        newlines + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_parses_branch_and_entries() {
        let text = "## feat/gui...origin/feat/gui [ahead 2]\n\
                    M  src/lib.rs\n \
                    M src/cli.rs\n\
                    A  src/gui/mod.rs\n \
                    D old.rs\n\
                    ?? notes.txt\n\
                    R  old-name.rs -> new-name.rs\n";
        let (branch, entries) = parse_porcelain(text);
        assert_eq!(branch, "feat/gui");
        assert_eq!(
            entries,
            vec![
                ("src/lib.rs".to_string(), 'M'),
                ("src/cli.rs".to_string(), 'M'),
                ("src/gui/mod.rs".to_string(), 'A'),
                ("old.rs".to_string(), 'D'),
                ("notes.txt".to_string(), '?'),
                ("new-name.rs".to_string(), 'M'),
            ]
        );
    }

    #[test]
    fn porcelain_branch_headers_cover_detached_and_unborn() {
        assert_eq!(parse_branch("main...origin/main"), "main");
        assert_eq!(parse_branch("HEAD (no branch)"), "HEAD");
        assert_eq!(parse_branch("No commits yet on trunk"), "trunk");
        assert_eq!(parse_branch("feat/x"), "feat/x");
    }

    #[test]
    fn numstat_parses_counts_binaries_and_renames() {
        let text = "10\t2\tsrc/gui/mod.rs\n\
                    -\t-\tassets/logo.png\n\
                    3\t1\tsrc/{old => new}/mod.rs\n\
                    0\t0\ta.rs => b.rs\n";
        assert_eq!(
            parse_numstat(text),
            vec![
                ("src/gui/mod.rs".to_string(), 10, 2),
                ("assets/logo.png".to_string(), 0, 0),
                ("src/new/mod.rs".to_string(), 3, 1),
                ("b.rs".to_string(), 0, 0),
            ]
        );
    }

    #[test]
    fn numstat_rename_with_empty_segment_collapses_slashes() {
        assert_eq!(numstat_path("src/{gui => }/mod.rs"), "src/mod.rs");
        assert_eq!(numstat_path("plain/path.rs"), "plain/path.rs");
    }

    #[test]
    fn wizard_state_paths_are_recognized() {
        assert!(is_wizard_state_path(".wizard/checkpoints/1/0.snap"));
        assert!(is_wizard_state_path("sub/.wizard/x"));
        assert!(is_wizard_state_path(".wizard"));
        assert!(!is_wizard_state_path("src/wizard.rs"));
        assert!(!is_wizard_state_path(".wizardrc"));
    }

    #[test]
    fn added_lines_counts_text_and_skips_binary() {
        assert_eq!(added_lines(b"one\ntwo\n"), 2);
        assert_eq!(added_lines(b"one\ntwo"), 2);
        assert_eq!(added_lines(b""), 0);
        assert_eq!(added_lines(b"bin\0ary"), 0);
    }
}
