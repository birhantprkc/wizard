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
        .env_remove("WIZARD_LLAMACPP_HOST")
        .env_remove("WIZARD_GGUF_PATH")
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
    // `--auto` is deprecated (approval gating was removed): still parsed for
    // compatibility but hidden from help.
    for flag in [
        "--mode",
        "--prompt",
        "--evolve",
        "--deep",
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
fn unreachable_ollama_provider_fails_with_actionable_error() {
    let home = TempDir::new();
    // Port 1 on localhost: connection refused immediately, no server needed.
    let bogus = "http://127.0.0.1:1";
    // Ollama is opt-in: only an explicit provider entry selects it.
    write_config(
        &home.0,
        "[[providers]]\n\
         name = \"local\"\n\
         kind = \"ollama\"\n\
         base_url = \"http://127.0.0.1:1\"\n\
         model = \"qwen3.5:9b\"\n",
    );
    let output = run_wizard(&home.0, &["--mode", "sovereign", "-p", "do nothing"], &[]);

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
fn unreachable_llamacpp_host_fails_with_actionable_error() {
    let home = TempDir::new();
    // Port 1 on localhost: connection refused immediately, no server needed.
    let bogus = "http://127.0.0.1:1";
    // An empty PATH guarantees auto-spawn is impossible even on machines
    // that have llama-server installed, so the failure is deterministic.
    let output = run_wizard(
        &home.0,
        &["--mode", "sovereign", "-p", "do nothing"],
        &[("WIZARD_LLAMACPP_HOST", bogus), ("PATH", "/nonexistent")],
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
        stderr.contains("llama-server"),
        "error must tell the user how to fix it:\n{stderr}"
    );
    assert!(
        !stderr.contains("panicked"),
        "must fail gracefully, not panic:\n{stderr}"
    );
}

/// Write `~/.wizard/config.toml` under the fake home.
fn write_config(home: &Path, contents: &str) {
    let dir = home.join(".wizard");
    std::fs::create_dir_all(&dir).expect("create .wizard dir");
    std::fs::write(dir.join("config.toml"), contents).expect("write config.toml");
}

#[test]
fn fresh_config_resolves_to_the_llamacpp_provider() {
    let home = TempDir::new();
    // A config written by current versions always carries `llamacpp_host`;
    // point it at port 1 so the probe fails instantly instead of touching
    // whatever might really be listening on the default port.
    write_config(&home.0, "llamacpp_host = \"http://127.0.0.1:1\"\n");
    let output = run_wizard(
        &home.0,
        &["--mode", "sovereign", "-p", "do nothing"],
        &[("PATH", "/nonexistent")],
    );

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("llama-server") && stderr.contains("http://127.0.0.1:1"),
        "the synthesized provider must be llama.cpp at the configured host:\n{stderr}"
    );
    assert!(
        !stderr.contains("ollama serve"),
        "a fresh config must not resolve to Ollama:\n{stderr}"
    );
}

#[test]
fn legacy_ollama_config_resolves_to_llamacpp() {
    let home = TempDir::new();
    // A pre-llama.cpp config: legacy top-level keys, none of the new ones.
    // The synthesized local provider is llama.cpp regardless — Ollama is
    // opt-in via an explicit [[providers]] entry.
    write_config(
        &home.0,
        "model = \"qwen3.5:9b\"\nollama_host = \"http://127.0.0.1:1\"\n",
    );
    // Point llama.cpp at port 1 (instant refusal) and empty the PATH so
    // auto-spawn is impossible: the failure is deterministic even on
    // machines with a real llama-server on the default port.
    let output = run_wizard(
        &home.0,
        &["--mode", "sovereign", "-p", "do nothing"],
        &[
            ("WIZARD_LLAMACPP_HOST", "http://127.0.0.1:1"),
            ("PATH", "/nonexistent"),
        ],
    );

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("llama-server") && stderr.contains("http://127.0.0.1:1"),
        "a legacy config must resolve to llama.cpp:\n{stderr}"
    );
    assert!(
        !stderr.contains("ollama serve"),
        "a legacy config must not resolve to Ollama:\n{stderr}"
    );
}

#[test]
fn missing_config_without_a_tty_points_at_onboarding() {
    let home = TempDir::new();
    // `Command::output` pipes stdout/stderr, so this is non-interactive: no
    // config must not fall through to a local provider probe (Ollama or
    // llama.cpp).
    let output = run_wizard(&home.0, &[], &[]);

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("onboard") || stderr.contains("config"),
        "error must point at setup:\n{stderr}"
    );
    assert!(
        !stderr.contains("ollama serve"),
        "a fresh install must not require Ollama:\n{stderr}"
    );
    assert!(
        !stderr.contains("llama-server"),
        "a fresh install must not probe llama-server before setup:\n{stderr}"
    );
}

#[test]
fn headless_mode_without_a_prompt_is_an_actionable_error() {
    let home = TempDir::new();
    write_config(&home.0, "llamacpp_host = \"http://127.0.0.1:1\"\n");
    let output = run_wizard(
        &home.0,
        &["--mode", "sovereign"],
        &[("PATH", "/nonexistent")],
    );

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("-p"),
        "error must point at the missing -p flag:\n{stderr}"
    );
}

#[test]
fn schedule_add_list_remove_round_trip() {
    let home = TempDir::new();
    let cwd = home.0.to_string_lossy().to_string();

    let output = run_wizard(
        &home.0,
        &[
            "schedule",
            "add",
            "nightly",
            "--cron",
            "0 3 * * *",
            "--prompt",
            "tidy the repo",
            "--cwd",
            &cwd,
        ],
        &[],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "add must succeed.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("next fire"),
        "add must print the next fire time:\n{stdout}"
    );
    assert!(
        home.0.join(".wizard").join("schedule.toml").exists(),
        "add must persist schedule.toml"
    );

    let output = run_wizard(&home.0, &["schedule", "list"], &[]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("nightly") && stdout.contains("0 3 * * *"),
        "list must show the entry:\n{stdout}"
    );

    let output = run_wizard(&home.0, &["schedule", "remove", "nightly"], &[]);
    assert!(output.status.success(), "remove must succeed");

    let output = run_wizard(&home.0, &["schedule", "list"], &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("no entries"),
        "list after remove must be empty:\n{stdout}"
    );

    let output = run_wizard(&home.0, &["schedule", "remove", "nightly"], &[]);
    assert!(
        !output.status.success(),
        "removing a missing entry must fail"
    );
}

#[test]
fn schedule_add_rejects_a_bad_cron_expression() {
    let home = TempDir::new();
    let cwd = home.0.to_string_lossy().to_string();
    let output = run_wizard(
        &home.0,
        &[
            "schedule", "add", "broken", "--cron", "whenever", "--prompt", "x", "--cwd", &cwd,
        ],
        &[],
    );
    assert!(!output.status.success(), "a bad cron must be rejected");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cron"),
        "error must mention the cron expression:\n{stderr}"
    );
    assert!(
        !home.0.join(".wizard").join("schedule.toml").exists(),
        "nothing may be persisted on a failed add"
    );
}

/// Real inference end to end: auto-spawn llama-server, load a GGUF, run one
/// sovereign turn. Opt-in only — set `WIZARD_E2E_GGUF` to a local GGUF path
/// (small models recommended); skipped otherwise so `cargo test` never loads
/// a model.
#[test]
fn e2e_inference_with_auto_spawned_llama_server() {
    let Some(gguf) = std::env::var("WIZARD_E2E_GGUF")
        .ok()
        .filter(|path| !path.trim().is_empty())
    else {
        eprintln!("skipping: set WIZARD_E2E_GGUF=/path/to/model.gguf to run");
        return;
    };
    assert!(
        Path::new(&gguf).exists(),
        "WIZARD_E2E_GGUF points at a missing file: {gguf}"
    );
    assert!(
        Command::new("llama-server")
            .arg("--version")
            .output()
            .is_ok(),
        "WIZARD_E2E_GGUF is set but llama-server is not on PATH"
    );

    let home = TempDir::new();
    // An uncommon port so the test never collides with a llama-server the
    // developer is actually running on the default 8080.
    let host = "http://127.0.0.1:18434";
    let output = run_wizard(
        &home.0,
        &[
            "--mode",
            "sovereign",
            "--loop",
            "1",
            "--max-hours",
            "0.2",
            "-p",
            "Reply with the single word DONE. Do not use any tools.",
        ],
        &[("WIZARD_LLAMACPP_HOST", host), ("WIZARD_GGUF_PATH", &gguf)],
    );

    // The spawned server deliberately outlives wizard; stop it before any
    // assertion can bail out of the test.
    let pid_file = home.0.join(".wizard").join("llama-server.pid");
    if let Ok(pid) = std::fs::read_to_string(&pid_file) {
        let _ = Command::new("kill").arg(pid.trim()).status();
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "sovereign run must succeed.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("started llama-server"),
        "wizard must report the server it spawned:\n{stdout}"
    );
    assert!(!stderr.contains("panicked"), "must not panic:\n{stderr}");
}
