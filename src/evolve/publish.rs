//! Fork-and-distribute: push `~/.wizard/src` to the user's GitHub fork and
//! emit a one-line installer for their Wizard variant.
//!
//! See `docs/market.md` for the full feature description.

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::config::Config;
use crate::evolve::Evolver;

/// The upstream slug this module forks from.
const UPSTREAM_SLUG: &str = "teddytennant/wizard";

/// What `/publish` / the publish tool was asked to do.
pub struct PublishRequest {
    /// Branch to push to on the fork (default `"main"` when `None`).
    pub branch: Option<String>,
}

/// Result of a successful publish.
#[derive(Debug, Clone)]
pub struct PublishOutcome {
    /// `"owner/wizard"`
    pub fork_repo: String,
    /// `"https://github.com/owner/wizard"`
    pub fork_url: String,
    /// Branch pushed, e.g. `"main"`.
    pub branch: String,
    /// The `curl | bash` one-liner installers can copy.
    pub install_one_liner: String,
    /// Short SHA pushed to the fork, if it could be read.
    pub commit: Option<String>,
}

/// JSONL record written to `~/.wizard/evolution.jsonl` for each publish.
/// Intentionally a separate type from [`crate::evolve::EvolutionEvent`] so
/// existing `EvolutionEvent` deserialization is not affected.
#[derive(Debug, Serialize)]
struct PublishEvent {
    /// Fixed discriminator so readers can distinguish this from
    /// `EvolutionEvent` lines (which carry `"tier"` instead of `"event"`).
    event: &'static str,
    timestamp: DateTime<Utc>,
    fork_repo: String,
    fork_url: String,
    branch: String,
    install_one_liner: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    commit: Option<String>,
}

/// Fork the upstream Wizard repo to the authed GitHub user, push
/// `~/.wizard/src` to `branch`, and return an [`PublishOutcome`] with the
/// install one-liner.
///
/// Requires:
/// - `gh` CLI installed and authenticated (`gh auth login`).
/// - `~/.wizard/src` is a committed Wizard checkout (created automatically by
///   deep evolve, or on demand via [`Evolver::ensure_source`]).
///
/// `verbose` prints progress lines to stdout (used by `--publish` CLI mode).
pub async fn publish(
    config: &Config,
    req: PublishRequest,
    verbose: bool,
) -> anyhow::Result<PublishOutcome> {
    // 1. Ensure ~/.wizard/src exists (clone if needed).
    let evolver = Evolver::new(config.clone()).with_verbose(verbose);
    let source_dir = evolver
        .ensure_source()
        .context("ensuring Wizard source checkout at ~/.wizard/src")?;

    // 2. Verify gh is installed and authenticated.
    if !command_exists("gh") {
        bail!(
            "`gh` (the GitHub CLI) is required to publish; \
             install it from https://cli.github.com and run `gh auth login`"
        );
    }
    let auth_ok = Command::new("gh")
        .args(["auth", "status"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("running `gh auth status`")?
        .success();
    if !auth_ok {
        bail!(
            "not authenticated with GitHub — run `gh auth login` first, \
             then retry `wizard --publish`"
        );
    }

    // 3. Determine the authed user's login.
    let user_json = run_command_stdout(&["gh", "api", "user"])
        .context("fetching GitHub user info with `gh api user`")?;
    let login = parse_gh_login(&user_json).context("parsing login from `gh api user` response")?;

    let branch = req.branch.unwrap_or_else(|| "main".to_string());
    let fork_repo = fork_slug(&login);
    let fork_url = format!("https://github.com/{fork_repo}");

    // 4. Fork upstream (idempotent — treat "already exists" as success).
    if verbose {
        println!("Forking {UPSTREAM_SLUG} to {fork_repo}…");
    }
    let fork_output = Command::new("gh")
        .args(["repo", "fork", UPSTREAM_SLUG, "--clone=false"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("running `gh repo fork`")?;
    if !fork_output.status.success() {
        let stderr = String::from_utf8_lossy(&fork_output.stderr);
        // gh exits non-zero with "already exists" when the fork is present.
        if !stderr.contains("already exists") {
            bail!("`gh repo fork {UPSTREAM_SLUG}` failed: {}", stderr.trim());
        }
        if verbose {
            println!("Fork already exists — verifying it is accessible…");
        }
    }

    // 5. Verify the fork is accessible (catches auth/visibility issues).
    let view_ok = Command::new("gh")
        .args(["repo", "view", &fork_repo])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("running `gh repo view` to verify fork")?
        .success();
    if !view_ok {
        bail!(
            "fork `{fork_repo}` could not be accessed after forking; \
             run `gh repo view {fork_repo}` to diagnose"
        );
    }

    // 6. Capture the HEAD commit SHA before push.
    let commit = git_short_sha(&source_dir);

    // 7. Add (or update) the "fork" remote.
    let fork_remote_url = format!("https://github.com/{fork_repo}.git");
    let remote_exists = Command::new("git")
        .arg("-C")
        .arg(&source_dir)
        .args(["remote", "get-url", "fork"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if remote_exists {
        // Update the URL in case the owner changed (e.g. post re-fork).
        let set_output = Command::new("git")
            .arg("-C")
            .arg(&source_dir)
            .args(["remote", "set-url", "fork", &fork_remote_url])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .context("running `git remote set-url fork`")?;
        if !set_output.status.success() {
            bail!(
                "`git remote set-url fork {fork_remote_url}` failed: {}",
                String::from_utf8_lossy(&set_output.stderr).trim()
            );
        }
    } else {
        let add_output = Command::new("git")
            .arg("-C")
            .arg(&source_dir)
            .args(["remote", "add", "fork", &fork_remote_url])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .context("running `git remote add fork`")?;
        if !add_output.status.success() {
            bail!(
                "`git remote add fork {fork_remote_url}` failed: {}",
                String::from_utf8_lossy(&add_output.stderr).trim()
            );
        }
    }

    // 8. Push to the fork.
    if verbose {
        println!("Pushing to {fork_repo} branch {branch}…");
    }
    let refspec = format!("HEAD:{branch}");
    let push_output = Command::new("git")
        .arg("-C")
        .arg(&source_dir)
        .args(["push", "fork", &refspec])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("running `git push fork`")?;
    if !push_output.status.success() {
        bail!(
            "`git push fork HEAD:{branch}` failed: {}",
            String::from_utf8_lossy(&push_output.stderr).trim()
        );
    }

    let one_liner = install_one_liner(&login, "wizard", &branch);

    let outcome = PublishOutcome {
        fork_repo: fork_repo.clone(),
        fork_url: fork_url.clone(),
        branch: branch.clone(),
        install_one_liner: one_liner.clone(),
        commit: commit.clone(),
    };

    // 9. Append a publish record to evolution.jsonl (best-effort).
    let log_result = (|| -> Result<()> {
        let path = Config::evolution_log_path()?;
        let event = PublishEvent {
            event: "publish",
            timestamp: Utc::now(),
            fork_repo: fork_repo.clone(),
            fork_url: fork_url.clone(),
            branch: branch.clone(),
            install_one_liner: one_liner.clone(),
            commit: commit.clone(),
        };
        append_publish_event(&path, &event)
    })();
    if let Err(err) = log_result {
        tracing::warn!("could not append publish event to evolution.jsonl: {err:#}");
    }

    if verbose {
        println!("Published to {fork_url}");
        println!("Install one-liner:\n{one_liner}");
    }

    Ok(outcome)
}

// ---------------------------------------------------------------------------
// Pure helpers (all have unit tests below)
// ---------------------------------------------------------------------------

/// Build the canonical install one-liner for a Wizard fork.
///
/// The format is:
/// ```text
/// curl -fsSL https://raw.githubusercontent.com/<owner>/<repo>/<ref_>/install.sh | WIZARD_REPO=<owner>/<repo> WIZARD_REF=<ref_> WIZARD_BUILD_FROM_SOURCE=1 bash
/// ```
pub fn install_one_liner(owner: &str, repo: &str, ref_: &str) -> String {
    format!(
        "curl -fsSL https://raw.githubusercontent.com/{owner}/{repo}/{ref_}/install.sh | \
         WIZARD_REPO={owner}/{repo} WIZARD_REF={ref_} WIZARD_BUILD_FROM_SOURCE=1 bash"
    )
}

/// Returns `"<owner>/wizard"` — the expected fork slug for a given GitHub
/// owner. The repo name is always `"wizard"` (the canonical upstream name).
pub fn fork_slug(owner: &str) -> String {
    format!("{owner}/wizard")
}

/// Extract the `.login` field from the JSON returned by `gh api user`.
pub fn parse_gh_login(json: &str) -> Result<String> {
    let value: serde_json::Value =
        serde_json::from_str(json).context("parsing `gh api user` JSON")?;
    value
        .get("login")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("`.login` field not found in `gh api user` response"))
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// `true` when `cmd --version` exits successfully.
fn command_exists(cmd: &str) -> bool {
    Command::new(cmd)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run `args[0] args[1..]`, capturing stdout, and return it on success.
fn run_command_stdout(args: &[&str]) -> Result<String> {
    let (&cmd, rest) = args.split_first().ok_or_else(|| anyhow!("empty command"))?;
    let output = Command::new(cmd)
        .args(rest)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("spawning `{}`", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "`{}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Return the short SHA of HEAD in `dir`, or `None` when git is unavailable
/// or the checkout has no commits.
fn git_short_sha(dir: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--short", "HEAD"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!sha.is_empty()).then_some(sha)
}

/// Append a [`PublishEvent`] as a JSONL line to `path`, creating parent
/// directories if needed.
fn append_publish_event(path: &Path, event: &PublishEvent) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let line = serde_json::to_string(event).context("serializing publish event")?;
    writeln!(file, "{line}").with_context(|| format!("writing to {}", path.display()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests (pure helpers only — no network, no filesystem writes to real paths)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_one_liner_matches_spec_exactly() {
        let line = install_one_liner("alice", "wizard", "main");
        assert_eq!(
            line,
            "curl -fsSL https://raw.githubusercontent.com/alice/wizard/main/install.sh | \
             WIZARD_REPO=alice/wizard WIZARD_REF=main WIZARD_BUILD_FROM_SOURCE=1 bash"
        );
    }

    #[test]
    fn install_one_liner_uses_custom_ref() {
        let line = install_one_liner("bob", "wizard", "my-feature");
        assert!(line.contains("WIZARD_REF=my-feature"), "ref in env var");
        assert!(line.contains("WIZARD_REPO=bob/wizard"), "repo in env var");
        assert!(
            line.contains("WIZARD_BUILD_FROM_SOURCE=1"),
            "build flag set"
        );
        assert!(
            line.contains("/bob/wizard/my-feature/install.sh"),
            "ref in URL"
        );
    }

    #[test]
    fn fork_slug_is_owner_slash_wizard() {
        assert_eq!(fork_slug("alice"), "alice/wizard");
        assert_eq!(fork_slug("teddytennant"), "teddytennant/wizard");
        assert_eq!(fork_slug("org-name"), "org-name/wizard");
    }

    #[test]
    fn parse_gh_login_extracts_login_field() {
        let json = r#"{"id":12345,"login":"alice","name":"Alice Smith","email":null}"#;
        assert_eq!(parse_gh_login(json).unwrap(), "alice");
    }

    #[test]
    fn parse_gh_login_with_extra_fields() {
        let json = r#"{"login":"teddytennant","node_id":"MDQ6VXNlcjE5MjY0NzY0MQ==","avatar_url":"https://avatars.githubusercontent.com/u/192647641?v=4"}"#;
        assert_eq!(parse_gh_login(json).unwrap(), "teddytennant");
    }

    #[test]
    fn parse_gh_login_rejects_missing_field() {
        let json = r#"{"id":1,"name":"No Login Here"}"#;
        assert!(
            parse_gh_login(json).is_err(),
            "should fail when login is absent"
        );
    }

    #[test]
    fn parse_gh_login_rejects_invalid_json() {
        assert!(
            parse_gh_login("not json at all").is_err(),
            "should fail on non-JSON"
        );
        assert!(parse_gh_login("").is_err(), "should fail on empty string");
    }

    #[test]
    fn install_one_liner_env_vars_are_exact_names() {
        // The env var names must match the install.sh contract exactly.
        let line = install_one_liner("x", "wizard", "main");
        assert!(line.contains("WIZARD_REPO="), "WIZARD_REPO env var present");
        assert!(line.contains("WIZARD_REF="), "WIZARD_REF env var present");
        assert!(
            line.contains("WIZARD_BUILD_FROM_SOURCE=1"),
            "WIZARD_BUILD_FROM_SOURCE=1 present"
        );
    }
}
