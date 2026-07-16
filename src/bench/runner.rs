//! Replay engine: run benchmark cases against a harness command in isolated
//! git worktrees and score each with its check command.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::Utc;

use super::{BenchCase, CaseResult, git, load_cases, logs_dir, results_dir, summarize};

/// Keep the selected cases: those matching any `--case` id or carrying any
/// `--tag` (a union). No filters selects everything; a filter that matches
/// nothing is an error (it is almost certainly a typo).
fn select_cases(
    mut cases: Vec<BenchCase>,
    ids: &[String],
    tags: &[String],
) -> Result<Vec<BenchCase>> {
    if ids.is_empty() && tags.is_empty() {
        return Ok(cases);
    }
    for wanted in ids {
        if !cases.iter().any(|case| &case.id == wanted) {
            bail!("no such case '{wanted}' — see `wizard bench list`");
        }
    }
    for wanted in tags {
        if !cases.iter().any(|case| case.tags.contains(wanted)) {
            bail!("no case carries tag '{wanted}' — see `wizard bench list`");
        }
    }
    cases.retain(|case| ids.contains(&case.id) || case.tags.iter().any(|t| tags.contains(t)));
    Ok(cases)
}

/// Replay cases (`wizard bench run`). See module docs in [`super`]. Returns
/// the process exit code: 0 when every selected case passed, 1 otherwise —
/// so a bench run can gate CI.
pub async fn run_cases(
    runner: Option<String>,
    label: String,
    filter: Vec<String>,
    tags: Vec<String>,
    keep_worktrees: bool,
    harness_json: bool,
) -> Result<i32> {
    let root = std::env::current_dir().context("determining current directory")?;
    let cases = select_cases(load_cases(&root)?, &filter, &tags)?;
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
    let log_dir = logs_dir(&root).join(&label);
    std::fs::create_dir_all(&log_dir).with_context(|| format!("creating {}", log_dir.display()))?;

    let id_width = cases.iter().map(|case| case.id.len()).max().unwrap_or(0);
    // Hidden automatically when stderr is not a terminal; the per-case lines
    // below go to stdout either way.
    let bar = crate::progress::bar(cases.len() as u64);
    let mut results = Vec::with_capacity(cases.len());
    for case in &cases {
        bar.set_message(case.id.clone());
        let result = run_case(
            &root,
            case,
            &template,
            &parent,
            &log_dir,
            keep_worktrees,
            harness_json,
        )
        .await;
        bar.suspend(|| {
            let tokens = result
                .harness_tokens
                .map(|t| format!("  {t} tok"))
                .unwrap_or_default();
            println!(
                "{:<id_width$}  {:<7}  harness {:.1}s  check {:.1}s{}",
                result.id,
                result.status.to_uppercase(),
                result.harness_secs,
                result.check_secs,
                tokens
            );
            if let Some(error) = &result.error {
                println!("{:id_width$}  ({error})", "");
            }
            if !result.passed {
                println!(
                    "{:id_width$}  logs: {}",
                    "",
                    log_dir
                        .join(format!("{}.{{harness,check}}.log", case.id))
                        .display()
                );
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
    Ok(if run.passed == run.total { 0 } else { 1 })
}

/// Replay one case: fresh worktree at `base_ref`, harness, then check.
/// Infrastructure failures (bad ref, worktree, spawn) become status "error"
/// rather than aborting the whole run.
async fn run_case(
    root: &Path,
    case: &BenchCase,
    template: &str,
    parent: &Path,
    log_dir: &Path,
    keep_worktree: bool,
    harness_json: bool,
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
        harness_tokens: None,
        harness_stop: None,
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
    let harness_log = log_dir.join(format!("{}.harness.log", case.id));
    let check_log = log_dir.join(format!("{}.check.log", case.id));
    match run_shell(&command, &worktree, case.timeout_secs, &harness_log).await {
        ShellOutcome::Finished { exit, secs, stdout } => {
            result.harness_exit = exit;
            result.harness_secs = secs;
            // Record the harness's own token usage and stop reason when asked
            // (and when it emitted a JSON usage object), for cross-agent cost
            // comparison. Never fails the case: a harness that emits no usage
            // just leaves these None.
            if harness_json {
                let (tokens, stop) = parse_harness_usage(&stdout);
                result.harness_tokens = tokens;
                result.harness_stop = stop;
            }
            // A nonzero harness exit still proceeds to the check: some
            // harnesses exit nonzero on benign conditions.
            match run_shell(&case.check, &worktree, case.check_timeout_secs, &check_log).await {
                ShellOutcome::Finished { exit, secs, .. } => {
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
    Finished {
        exit: Option<i32>,
        secs: f64,
        /// Captured stdout, parsed for token usage under `--harness-json`.
        stdout: String,
    },
    TimedOut {
        secs: f64,
    },
    SpawnFailed(String),
}

/// Extract `(total_tokens, stop_reason)` from a harness's JSON stdout. Handles
/// both a single buffered object (`--output-format json`) and NDJSON
/// (`streaming-json`, where the last usage-bearing line wins), and the usage
/// shapes emitted by Wizard (`prompt_tokens`/`completion_tokens`), Grok Build /
/// Anthropic (`input_tokens`/`output_tokens`), and OpenAI (`total_tokens`).
fn parse_harness_usage(stdout: &str) -> (Option<u64>, Option<String>) {
    // The whole stdout as one JSON object (possibly pretty-printed).
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(stdout.trim()) {
        let fields = extract_usage(&value);
        if fields.0.is_some() || fields.1.is_some() {
            return fields;
        }
    }
    // NDJSON: scan from the end for the last line carrying usage or a stop.
    for line in stdout.lines().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
            let fields = extract_usage(&value);
            if fields.0.is_some() || fields.1.is_some() {
                return fields;
            }
        }
    }
    (None, None)
}

fn extract_usage(value: &serde_json::Value) -> (Option<u64>, Option<String>) {
    let tokens = value.get("usage").and_then(token_count);
    let stop = ["stopReason", "stop_reason", "reason"]
        .iter()
        .find_map(|key| value.get(key).and_then(serde_json::Value::as_str))
        .map(str::to_string);
    (tokens, stop)
}

/// Total tokens from a usage object across the common vendor shapes.
fn token_count(usage: &serde_json::Value) -> Option<u64> {
    use serde_json::Value;
    if let Some(total) = usage.get("total_tokens").and_then(Value::as_u64) {
        return Some(total);
    }
    let pair = |a: &str, b: &str| -> Option<u64> {
        let x = usage.get(a).and_then(Value::as_u64);
        let y = usage.get(b).and_then(Value::as_u64);
        match (x, y) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
        }
    };
    pair("prompt_tokens", "completion_tokens").or_else(|| pair("input_tokens", "output_tokens"))
}

/// Run `sh -c <command>` in `cwd` with a wall-clock bound. `WIZARD_BENCH=1`
/// marks the process tree as a bench replay so the trajectory recorder stays
/// silent inside it. Output is captured and teed to `log_path` so failures
/// stay diagnosable; the exit code and timing remain the signal.
async fn run_shell(command: &str, cwd: &Path, timeout_secs: u64, log_path: &Path) -> ShellOutcome {
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
        Ok(Ok(output)) => {
            write_shell_log(log_path, command, Some(&output));
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            ShellOutcome::Finished {
                exit: output.status.code(),
                secs: started.elapsed().as_secs_f64(),
                stdout,
            }
        }
        Ok(Err(err)) => ShellOutcome::SpawnFailed(err.to_string()),
        // The dropped child future kills the process (kill_on_drop).
        Err(_elapsed) => {
            write_shell_log(log_path, command, None);
            ShellOutcome::TimedOut {
                secs: started.elapsed().as_secs_f64(),
            }
        }
    }
}

/// Persist one shell invocation's output to `log_path` (best-effort: a
/// logging failure must never fail the case). `None` output = timeout, in
/// which case the captured output died with the killed process.
fn write_shell_log(log_path: &Path, command: &str, output: Option<&std::process::Output>) {
    let mut text = format!("$ {command}\n");
    match output {
        Some(output) => {
            text.push_str(&format!("exit: {:?}\n", output.status.code()));
            text.push_str("--- stdout ---\n");
            text.push_str(&String::from_utf8_lossy(&output.stdout));
            text.push_str("\n--- stderr ---\n");
            text.push_str(&String::from_utf8_lossy(&output.stderr));
            text.push('\n');
        }
        None => text.push_str("(timed out — captured output was lost with the killed process)\n"),
    }
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(err) = std::fs::write(log_path, text) {
        tracing::warn!("could not write bench log {}: {err}", log_path.display());
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

    fn case(id: &str, tags: &[&str]) -> BenchCase {
        BenchCase {
            id: id.to_string(),
            prompt: "p".to_string(),
            base_ref: "deadbeef".to_string(),
            check: "true".to_string(),
            timeout_secs: 10,
            check_timeout_secs: 10,
            tags: tags.iter().map(|t| (*t).to_string()).collect(),
            source: "manual".to_string(),
            created: chrono::Utc::now(),
            notes: None,
        }
    }

    fn ids(cases: &[BenchCase]) -> Vec<&str> {
        cases.iter().map(|c| c.id.as_str()).collect()
    }

    #[test]
    fn select_cases_no_filter_keeps_everything() {
        let cases = vec![case("a", &[]), case("b", &["rust"])];
        let out = select_cases(cases, &[], &[]).expect("selects");
        assert_eq!(ids(&out), vec!["a", "b"]);
    }

    #[test]
    fn parse_harness_usage_reads_wizards_own_json() {
        // Wizard's `--output-format json`: prompt/completion + a `reason`.
        let out = r#"{"result":"done","reason":"Completed","usage":{"prompt_tokens":1200,"completion_tokens":300}}"#;
        assert_eq!(
            parse_harness_usage(out),
            (Some(1500), Some("Completed".to_string()))
        );
    }

    #[test]
    fn parse_harness_usage_reads_grok_anthropic_shape() {
        // input/output tokens + camelCase stopReason (Grok Build / Anthropic).
        let out = r#"{"text":"hi","stopReason":"end_turn","usage":{"input_tokens":900,"output_tokens":100}}"#;
        assert_eq!(
            parse_harness_usage(out),
            (Some(1000), Some("end_turn".to_string()))
        );
    }

    #[test]
    fn parse_harness_usage_prefers_total_tokens_and_reads_ndjson() {
        // streaming-json: the last usage-bearing line wins; total_tokens wins.
        let out = "{\"type\":\"text\",\"text\":\"working\"}\n{\"type\":\"end\",\"usage\":{\"total_tokens\":4096}}\n";
        assert_eq!(parse_harness_usage(out), (Some(4096), None));
    }

    #[test]
    fn parse_harness_usage_is_none_for_plain_output() {
        assert_eq!(parse_harness_usage("just some text, no json\n"), (None, None));
    }

    #[test]
    fn select_cases_by_id_tag_and_their_union() {
        let all = || {
            vec![
                case("a", &["rust"]),
                case("b", &["docs"]),
                case("c", &["rust", "slow"]),
            ]
        };
        let out = select_cases(all(), &["b".to_string()], &[]).expect("id filter");
        assert_eq!(ids(&out), vec!["b"]);

        let out = select_cases(all(), &[], &["rust".to_string()]).expect("tag filter");
        assert_eq!(ids(&out), vec!["a", "c"]);

        let out = select_cases(all(), &["b".to_string()], &["slow".to_string()])
            .expect("union of id and tag");
        assert_eq!(ids(&out), vec!["b", "c"]);
    }

    #[test]
    fn select_cases_rejects_unknown_id_or_tag() {
        let cases = vec![case("a", &["rust"])];
        let err = select_cases(cases.clone(), &["nope".to_string()], &[]).unwrap_err();
        assert!(err.to_string().contains("no such case"), "{err}");
        let err = select_cases(cases, &[], &["nope".to_string()]).unwrap_err();
        assert!(err.to_string().contains("no case carries tag"), "{err}");
    }

    #[test]
    fn shell_log_records_command_output_and_timeouts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("nested").join("case.harness.log");

        let output = std::process::Command::new("sh")
            .args(["-c", "echo out; echo err >&2; exit 3"])
            .output()
            .expect("sh runs");
        write_shell_log(&log, "echo out; echo err >&2; exit 3", Some(&output));
        let text = std::fs::read_to_string(&log).expect("log written");
        assert!(text.starts_with("$ echo out"), "{text}");
        assert!(text.contains("exit: Some(3)"), "{text}");
        assert!(text.contains("out\n"), "{text}");
        assert!(text.contains("err\n"), "{text}");

        write_shell_log(&log, "sleep 999", None);
        let text = std::fs::read_to_string(&log).expect("log written");
        assert!(text.contains("timed out"), "{text}");
    }

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
