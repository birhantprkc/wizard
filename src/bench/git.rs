//! Minimal async git helpers for the bench harness: ref resolution, dirty
//! detection for the recorder, and worktree lifecycle for replays.

use std::path::Path;

use anyhow::{Context, Result, bail};
use tokio::process::Command;

/// Resolve `ref_` to a full commit sha in `repo`
/// (`git rev-parse --verify <ref>^{commit}`).
pub async fn rev_parse(repo: &Path, ref_: &str) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", &format!("{ref_}^{{commit}}")])
        .output()
        .await
        .context("running git rev-parse")?;
    if !output.status.success() {
        bail!(
            "git rev-parse --verify {ref_:?} failed in {}: {}",
            repo.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// HEAD sha plus whether the working tree is dirty (`git status --porcelain`
/// non-empty); `None` outside a git repo or on any git failure. Never errors
/// because it backs the trajectory recorder, which must not be able to fail.
pub async fn head_and_dirty(repo: &Path) -> Option<(String, bool)> {
    let head = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "HEAD^{commit}"])
        .output()
        .await
        .ok()?;
    if !head.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&head.stdout).trim().to_string();

    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["status", "--porcelain"])
        .output()
        .await
        .ok()?;
    if !status.status.success() {
        return None;
    }
    let dirty = !String::from_utf8_lossy(&status.stdout).trim().is_empty();
    Some((sha, dirty))
}

/// Create a detached worktree of `ref_` at `dest`.
pub async fn worktree_add(repo: &Path, dest: &Path, ref_: &str) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "add", "--detach"])
        .arg(dest)
        .arg(ref_)
        .output()
        .await
        .context("running git worktree add")?;
    if !output.status.success() {
        bail!(
            "git worktree add {} {ref_} failed: {}",
            dest.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Remove the worktree at `dest`, best-effort: replay cleanup must never
/// mask the actual case result.
pub async fn worktree_remove(repo: &Path, dest: &Path) {
    let result = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "remove", "--force"])
        .arg(dest)
        .output()
        .await;
    match result {
        Ok(output) if output.status.success() => {}
        Ok(output) => tracing::warn!(
            "git worktree remove {} failed: {}",
            dest.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(err) => tracing::warn!("git worktree remove {}: {err}", dest.display()),
    }
}
