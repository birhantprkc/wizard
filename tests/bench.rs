//! Integration tests for `wizard bench` against the compiled binary.
//!
//! Every invocation runs with `HOME` pointed at a throwaway directory and a
//! fixture git repo as the working directory: no network, no LLM, and no
//! `~/.wizard` config — bench must work (and must never trigger onboarding)
//! on a completely fresh machine.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Temp dir removed on drop. Serves as the fake `$HOME`; the fixture git
/// repo lives in a subdirectory.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "wizard-bench-itest-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Run the compiled binary with `args` in `cwd`, an isolated `$HOME`, and
/// the wizard env overrides cleared.
fn run_wizard(home: &Path, cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_wizard"))
        .args(args)
        .env("HOME", home)
        .env_remove("WIZARD_MODEL")
        .env_remove("WIZARD_OLLAMA_HOST")
        .env_remove("WIZARD_BENCH")
        .current_dir(cwd)
        .output()
        .expect("binary runs")
}

/// Run git in `repo` with identity supplied via `-c` so no global config is
/// needed (HOME is an empty temp dir).
fn git(repo: &Path, args: &[&str]) -> Output {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "user.name=Bench Test",
            "-c",
            "user.email=bench@test.invalid",
        ])
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// `git init` + one commit; returns (repo path, HEAD sha).
fn fixture_repo(home: &Path) -> (PathBuf, String) {
    let repo = home.join("repo");
    std::fs::create_dir_all(&repo).expect("create repo dir");
    git(&repo, &["init", "--quiet"]);
    std::fs::write(repo.join("README.md"), "fixture\n").expect("write fixture file");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "--quiet", "-m", "init"]);
    let head = git(&repo, &["rev-parse", "HEAD"]);
    let sha = String::from_utf8_lossy(&head.stdout).trim().to_string();
    (repo, sha)
}

fn add_touch_case(home: &Path, repo: &Path) {
    let output = run_wizard(
        home,
        repo,
        &[
            "bench",
            "add",
            "--id",
            "touch-case",
            "--prompt",
            "create marker",
            "--check",
            "test -f marker.txt",
        ],
    );
    assert!(
        output.status.success(),
        "bench add must succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Find the single result JSON whose name starts with `label`.
fn result_file(repo: &Path, label: &str) -> PathBuf {
    let dir = repo.join(".wizard/bench/results");
    std::fs::read_dir(&dir)
        .expect("results dir exists")
        .filter_map(|entry| Some(entry.ok()?.path()))
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(label))
        })
        .unwrap_or_else(|| panic!("no result file for label {label} in {}", dir.display()))
}

#[test]
fn add_creates_case_toml_without_onboarding_or_home_state() {
    let home = TempDir::new("add");
    let (repo, sha) = fixture_repo(&home.0);

    add_touch_case(&home.0, &repo);

    let case_path = repo.join(".wizard/bench/cases/touch-case.toml");
    assert!(case_path.exists(), "case file must be written");
    let text = std::fs::read_to_string(&case_path).expect("case readable");
    assert!(
        text.contains("create marker"),
        "case stores the prompt:\n{text}"
    );
    assert!(
        text.contains(&sha),
        "case stores the full HEAD sha:\n{text}"
    );

    // Bench is config-free: no onboarding, nothing written under $HOME.
    assert!(
        !home.0.join(".wizard").exists(),
        "bench must not create ~/.wizard or trigger onboarding"
    );
}

#[test]
fn run_records_pass_and_fail_results_and_compare_reports_both() {
    let home = TempDir::new("run");
    let (repo, _sha) = fixture_repo(&home.0);
    add_touch_case(&home.0, &repo);

    // Passing run: the "harness" creates the file the check looks for.
    let output = run_wizard(
        &home.0,
        &repo,
        &[
            "bench",
            "run",
            "--runner",
            "touch marker.txt",
            "--label",
            "pass-run",
        ],
    );
    assert!(
        output.status.success(),
        "bench run must succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("1/1 passed"), "summary line:\n{stdout}");

    let pass_path = result_file(&repo, "pass-run");
    let pass_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&pass_path).expect("result readable"))
            .expect("result parses");
    assert_eq!(pass_json["pass_rate"], 1.0, "pass_rate in {pass_json}");

    // Failing run: harness does nothing, so the check fails.
    let output = run_wizard(
        &home.0,
        &repo,
        &["bench", "run", "--runner", "true", "--label", "fail-run"],
    );
    assert!(
        output.status.success(),
        "a failing case is still a clean run"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("0/1 passed"), "summary line:\n{stdout}");

    let fail_path = result_file(&repo, "fail-run");
    let fail_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&fail_path).expect("result readable"))
            .expect("result parses");
    assert_eq!(fail_json["passed"], 0, "passed count in {fail_json}");

    // Compare the two runs.
    let output = run_wizard(
        &home.0,
        &repo,
        &[
            "bench",
            "compare",
            pass_path.to_str().unwrap(),
            fail_path.to_str().unwrap(),
        ],
    );
    assert!(
        output.status.success(),
        "bench compare must succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("pass-run"),
        "compare names label A:\n{stdout}"
    );
    assert!(
        stdout.contains("fail-run"),
        "compare names label B:\n{stdout}"
    );
    assert!(
        stdout.contains("touch-case"),
        "compare lists the case:\n{stdout}"
    );
}

#[test]
fn list_shows_cases() {
    let home = TempDir::new("list");
    let (repo, _sha) = fixture_repo(&home.0);
    add_touch_case(&home.0, &repo);

    let output = run_wizard(&home.0, &repo, &["bench", "list"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("touch-case"),
        "list shows the case:\n{stdout}"
    );
}

#[test]
fn promote_accepts_clean_trajectories_and_rejects_dirty_ones() {
    let home = TempDir::new("promote");
    let (repo, sha) = fixture_repo(&home.0);

    let clean_id = "aaaaaaaa-0000-4000-8000-000000000000";
    let dirty_id = "bbbbbbbb-0000-4000-8000-000000000000";
    let record = |id: &str, dirty: bool| {
        format!(
            r#"{{"id":"{id}","timestamp":"2026-06-10T00:00:00Z","prompt":"create marker","git_ref":"{sha}","dirty":{dirty},"done_reason":"Completed","duration_secs":1.5,"model":"test-model","mode":"sovereign"}}"#
        )
    };
    std::fs::create_dir_all(repo.join(".wizard")).expect("create .wizard");
    std::fs::write(
        repo.join(".wizard/trajectories.jsonl"),
        format!("{}\n{}\n", record(clean_id, false), record(dirty_id, true)),
    )
    .expect("write trajectories");

    // Clean record promotes by id prefix.
    let output = run_wizard(
        &home.0,
        &repo,
        &["bench", "promote", "aaaa", "--check", "true"],
    );
    assert!(
        output.status.success(),
        "promote of a clean trajectory must succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        repo.join(format!(".wizard/bench/cases/{clean_id}.toml"))
            .exists(),
        "promoted case file must exist"
    );

    // Dirty record is rejected with an explanation.
    let output = run_wizard(
        &home.0,
        &repo,
        &["bench", "promote", "bbbb", "--check", "true"],
    );
    assert!(
        !output.status.success(),
        "promote of a dirty trajectory must fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("dirty"),
        "error explains the dirty repo:\n{stderr}"
    );
}
