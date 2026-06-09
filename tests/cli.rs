//! Integration tests for the compiled `wizard` binary.
//!
//! Every invocation runs with `HOME` pointed at a throwaway directory so the
//! binary's `~/.wizard` tree is created there and the real one is never
//! touched (`dirs::home_dir()` honors `$HOME` on Linux).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Temp dir removed on drop. Serves as both fake `$HOME` and project root.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "wizard-itest-{}-{:?}",
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

/// Run the compiled binary with `args`, an isolated `$HOME`, and the wizard
/// env overrides cleared (unless re-set via `envs`).
fn run_wizard(home: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_wizard"));
    command
        .args(args)
        .env("HOME", home)
        .env_remove("WIZARD_MODEL")
        .env_remove("WIZARD_OLLAMA_HOST")
        .current_dir(home);
    for (key, value) in envs {
        command.env(key, value);
    }
    command.output().expect("binary runs")
}

#[test]
fn help_prints_usage_and_documented_flags() {
    let home = TempDir::new();
    let output = run_wizard(&home.0, &["--help"], &[]);

    assert!(output.status.success(), "--help must exit 0");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Usage:"), "help shows usage:\n{stdout}");
    for flag in [
        "--mode",
        "--prompt",
        "--evolve",
        "--deep",
        "--auto",
        "--max-hours",
        "--loop",
        "--cwd",
        "--resume",
    ] {
        assert!(
            stdout.contains(flag),
            "help must document {flag}:\n{stdout}"
        );
    }
    assert!(
        !home.0.join(".wizard").exists(),
        "--help must not create state"
    );
}

#[test]
fn version_prints_name_and_version() {
    let home = TempDir::new();
    let output = run_wizard(&home.0, &["--version"], &[]);

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with("wizard "), "got: {stdout}");
}

#[test]
fn unreachable_ollama_host_fails_with_actionable_error() {
    let home = TempDir::new();
    // Port 1 on localhost: connection refused immediately, no server needed.
    let bogus = "http://127.0.0.1:1";
    let output = run_wizard(
        &home.0,
        &["--mode", "sovereign", "-p", "do nothing"],
        &[("WIZARD_OLLAMA_HOST", bogus)],
    );

    assert!(
        !output.status.success(),
        "an unreachable host must be a failure"
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "clean exit code, not a crash"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("error:"), "stderr: {stderr}");
    assert!(
        stderr.contains(bogus),
        "error must name the configured host:\n{stderr}"
    );
    assert!(
        stderr.contains("ollama serve"),
        "error must tell the user how to fix it:\n{stderr}"
    );
    assert!(
        !stderr.contains("panicked"),
        "must fail gracefully, not panic:\n{stderr}"
    );
}

#[test]
fn headless_mode_without_a_prompt_is_an_actionable_error() {
    let home = TempDir::new();
    let output = run_wizard(
        &home.0,
        &["--mode", "sovereign"],
        &[("WIZARD_OLLAMA_HOST", "http://127.0.0.1:1")],
    );

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("-p"),
        "error must point at the missing -p flag:\n{stderr}"
    );
}
