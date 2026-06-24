//! Replay engine: run benchmark cases against a harness command in isolated
//! git worktrees and score each with its check command.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::Utc;

use super::{BenchCase, CaseResult, git, load_cases, results_dir, summarize};

/// Replay cases (`wizard bench run`). See module docs in [`super`].
pub async fn run_cases(
    runner: Option<String>,
    label: String,
    filter: Vec<String>,
    keep_worktrees: bool,
) -> Result<()> {
    let root = std::env::current_dir().context("determining current directory")?;
    let mut cases = load_cases(&root)?;
    if !filter.is_empty() {
        for wanted in &filter {
            if !cases.iter().any(|case| &case.id == wanted) {
                bail!("no such case '{wanted}' — see `wizard bench list`");
            }
        }
        cases.retain(|case| filter.contains(&case.id));
    }
    if cases.is_empty() {
        bail!("no cases to run — create one with `wizard bench add` or `wizard bench promote`");
    }

    let template = match runner {
        Some(template) => template,
        None => {
            let exe =
                std::env::current_exe().context("locating this binary for the default runner")?;
            format!("{} --mode sovereign -p {{prompt}}", exe.display())
        }
    };

    let parent = std::env::temp_dir().join(format!("wizard-bench-{}", std::process::id()));
    std::fs::create_dir_all(&parent)
        .with_context(|| format!("creating worktree parent {}", parent.display()))?;

    let id_width = cases.iter().map(|case| case.id.len()).max().unwrap_or(0);
    // Hidden automatically when stderr is not a terminal; the per-case lines
    // below go to stdout either way.
    let bar = crate::progress::bar(cases.len() as u64);
    let mut results = Vec::with_capacity(cases.len());
    for case in &cases {
        bar.set_message(case.id.clone());
        let result = run_case(&root, case, &template, &parent, keep_worktrees).await;
        bar.suspend(|| {
            println!(
                "{:<id_width$}  {:<7}  harness {:.1}s  check {:.1}s",
                result.id,
                result.status.to_uppercase(),
                result.harness_secs,
                result.check_secs
            );
            if let Some(error) = &result.error {
                println!("{:id_width$}  ({error})", "");
            }
            if keep_worktrees {
                println!("kept worktree: {}", parent.join(&case.id).display());
            }
        });
        bar.inc(1);
        results.push(result);
    }
    bar.finish_and_clear();

    if !keep_worktrees {
        // Best-effort: worktrees themselves were already removed per case.
        let _ = std::fs::remove_dir_all(&parent);
    }

    let run = summarize(label, template, results);
    let dir = results_dir(&root);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(format!("{}-{}.json", run.label, Utc::now().timestamp()));
    let json = serde_json::to_string_pretty(&run).context("serializing results")?;
    std::fs::write(&path, json).with_context(|| format!("writing {}", path.display()))?;
    println!(
        "{}/{} passed ({:.0}%) — results: {}",
        run.passed,
        run.total,
        run.pass_rate * 100.0,
        path.display()
    );
    Ok(())
}

/// Replay one case: fresh worktree at `base_ref`, harness, then check.
/// Infrastructure failures (bad ref, worktree, spawn) become status "error"
/// rather than aborting the whole run.
async fn run_case(
    root: &Path,
    case: &BenchCase,
    template: &str,
    parent: &Path,
    keep_worktree: bool,
) -> CaseResult {
    let mut result = CaseResult {
        id: case.id.clone(),
        status: "error".to_string(),
        passed: false,
        harness_exit: None,
        check_exit: None,
        harness_secs: 0.0,
        check_secs: 0.0,
        error: None,
    };

    // Re-verify the stored sha up front: it catches refs lost to GC or a
    // copied case file with a clearer message than a worktree failure.
    if let Err(err) = git::rev_parse(root, &case.base_ref).await {
        result.error = Some(format!("base ref not resolvable: {err:#}"));
        return result;
    }

    let worktree = parent.join(&case.id);
    if let Err(err) = git::worktree_add(root, &worktree, &case.base_ref).await {
        result.error = Some(format!("worktree setup failed: {err:#}"));
        return result;
    }

    let command = render_template(template, &case.prompt);
    match run_shell(&command, &worktree, case.timeout_secs).await {
        ShellOutcome::Finished { exit, secs } => {
            result.harness_exit = exit;
            result.harness_secs = secs;
            // A nonzero harness exit still proceeds to the check: some
            // harnesses exit nonzero on benign conditions.
            match run_shell(&case.check, &worktree, case.check_timeout_secs).await {
                ShellOutcome::Finished { exit, secs } => {
                    result.check_exit = exit;
                    result.check_secs = secs;
                    if exit == Some(0) {
                        result.status = "pass".to_string();
                        result.passed = true;
                    } else {
                        result.status = "fail".to_string();
                    }
                }
                ShellOutcome::TimedOut { secs } => {
                    result.check_secs = secs;
                    result.status = "timeout".to_string();
                }
                ShellOutcome::SpawnFailed(err) => {
                    result.error = Some(format!("spawning check: {err}"));
                }
            }
        }
        ShellOutcome::TimedOut { secs } => {
            result.harness_secs = secs;
            result.status = "timeout".to_string();
        }
        ShellOutcome::SpawnFailed(err) => {
            result.error = Some(format!("spawning harness: {err}"));
        }
    }

    // Kept worktrees are reported by the caller (under the progress bar).
    if !keep_worktree {
        git::worktree_remove(root, &worktree).await;
    }
    result
}

/// How one `sh -c` invocation ended.
enum ShellOutcome {
    Finished { exit: Option<i32>, secs: f64 },
    TimedOut { secs: f64 },
    SpawnFailed(String),
}

/// Run `sh -c <command>` in `cwd` with a wall-clock bound. `WIZARD_BENCH=1`
/// marks the process tree as a bench replay so the trajectory recorder stays
/// silent inside it. Output is captured (not inherited) and discarded; the
/// exit code and timing are the signal.
async fn run_shell(command: &str, cwd: &Path, timeout_secs: u64) -> ShellOutcome {
    let started = Instant::now();
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .env("WIZARD_BENCH", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => return ShellOutcome::SpawnFailed(err.to_string()),
    };
    match tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait_with_output()).await {
        Ok(Ok(output)) => ShellOutcome::Finished {
            exit: output.status.code(),
            secs: started.elapsed().as_secs_f64(),
        },
        Ok(Err(err)) => ShellOutcome::SpawnFailed(err.to_string()),
        // The dropped child future kills the process (kill_on_drop).
        Err(_elapsed) => ShellOutcome::TimedOut {
            secs: started.elapsed().as_secs_f64(),
        },
    }
}

/// Wrap `s` in single quotes for `sh -c`, escaping embedded single quotes
/// with the standard `'\''` dance.
fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Substitute `{prompt}` in a runner template with the shell-quoted prompt.
/// Templates without the placeholder are used verbatim.
fn render_template(template: &str, prompt: &str) -> String {
    template.replace("{prompt}", &shell_escape(prompt))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_escape_plain_string() {
        assert_eq!(shell_escape("hello world"), "'hello world'");
    }

    #[test]
    fn shell_escape_single_quotes() {
        assert_eq!(shell_escape("it's a test"), r"'it'\''s a test'");
    }

    #[test]
    fn shell_escape_empty_string() {
        assert_eq!(shell_escape(""), "''");
    }

    #[test]
    fn render_template_substitutes_prompt() {
        assert_eq!(
            render_template("wizard -p {prompt}", "add tests"),
            "wizard -p 'add tests'"
        );
    }

    #[test]
    fn render_template_without_placeholder_is_unchanged() {
        assert_eq!(
            render_template("touch marker.txt", "ignored prompt"),
            "touch marker.txt"
        );
    }
}
