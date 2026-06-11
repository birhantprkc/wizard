//! Wizard — a single-binary, fully local agent.
//!
//! A Ratatui front end on top of an Ollama-backed agent loop with an
//! extensible tool set (native + scripted + MCP) and tiered self-extension.
//! See `docs/architecture.md` for the full design.

pub mod agent;
pub mod app;
pub mod bench;
pub mod checkpoint;
pub mod cli;
pub mod commands;
pub mod config;
pub mod dispatch;
pub mod doctor;
pub mod event;
pub mod evolve;
pub mod fleet;
pub mod gateway;
pub mod hardware;
pub mod hooks;
pub mod instructions;
pub mod llm;
pub mod local_setup;
pub mod mcp;
pub mod memory;
pub mod onboarding;
pub mod output;
pub mod progress;
pub mod schedule;
pub mod server;
pub mod skills;
pub mod tools;
pub mod ui;
pub mod usage;

use std::io::IsTerminal;

use anyhow::Result;

use crate::config::Mode;

/// Top-level entry point: load config, apply CLI overrides, and dispatch to
/// the selected run mode (genie TUI, sovereign headless loop, or `--evolve`).
///
/// Returns the process exit code: headless runs map their outcome through
/// [`output::exit_code`] (0 completed, 2 max-steps, 3 circuit breaker, 4 time
/// limit); every other mode exits 0 on success. Hard errors surface as `Err`
/// and exit 1 from `main`.
pub async fn run(cli: cli::Cli) -> Result<i32> {
    // Bench is self-contained tooling: it must work with no config and no
    // LLM, so dispatch before onboarding and before the config load.
    if let Some(cli::Command::Bench { cmd }) = &cli.command {
        if let Some(dir) = &cli.cwd {
            std::env::set_current_dir(dir)?;
        }
        return bench::run(cmd.clone()).await.map(|()| 0);
    }

    // Doctor diagnoses the environment — starting with "does the config
    // parse?" — so it too dispatches before the config load and can never
    // trigger onboarding. Exits 0 when no check failed, 1 otherwise.
    if let Some(cli::Command::Doctor) = &cli.command {
        if let Some(dir) = &cli.cwd {
            std::env::set_current_dir(dir)?;
        }
        return doctor::run().await;
    }

    // Schedule CRUD and the scheduler daemon are config-independent too:
    // they only touch ~/.wizard/schedule.toml, and the jobs they spawn are
    // wizard child processes that load config themselves. `schedule run`
    // propagates the child's exit code.
    if let Some(cli::Command::Schedule { cmd }) = &cli.command {
        if let Some(dir) = &cli.cwd {
            std::env::set_current_dir(dir)?;
        }
        return schedule::run(cmd.clone()).await;
    }
    if let Some(cli::Command::Scheduler) = &cli.command {
        if let Some(dir) = &cli.cwd {
            std::env::set_current_dir(dir)?;
        }
        return schedule::run_daemon().await;
    }

    // Fleet dispatches before the normal flow too, but `fleet run` loads
    // config itself (its planning and synthesis turns drive a real
    // in-process agent); `fleet status` / `fleet stop` only touch the
    // project's `.wizard/fleet/` directory.
    if let Some(cli::Command::Fleet { cmd }) = &cli.command {
        if let Some(dir) = &cli.cwd {
            std::env::set_current_dir(dir)?;
        }
        return fleet::run(cmd.clone()).await;
    }

    // `--login` is a one-shot credential flow: no config, no onboarding,
    // no TUI. Tokens land in a dedicated file under ~/.wizard/.
    if let Some(provider) = &cli.login {
        return match provider.as_str() {
            "xai" => llm::xai_oauth::login(|line: &str| println!("{line}"))
                .await
                .map(|()| 0),
            other => anyhow::bail!("unknown login provider '{other}' (supported: xai)"),
        };
    }

    // First-run onboarding: build a fresh config interactively when requested,
    // or automatically on a fresh install in an interactive terminal. A
    // cancelled wizard exits gracefully without touching anything.
    let mut config = if should_onboard(&cli)? {
        match onboarding::run().await? {
            Some(config) => config,
            None => {
                println!("onboarding cancelled — run `wizard --onboard` any time.");
                return Ok(0);
            }
        }
    } else {
        let config_path = config::Config::path()?;
        if !config_path.exists() {
            // Non-interactive first runs (piped stdout, CI, cron) must not
            // silently fall back to a baked-in local provider — there is no
            // config yet and onboarding needs a TTY.
            let headless_with_prompt = cli.prompt.is_some()
                && (cli.mode == Some(Mode::Sovereign) || cli.continuous);
            if !headless_with_prompt {
                anyhow::bail!(
                    "no config at {} — run `wizard` in an interactive terminal \
                     (or `wizard --onboard`) to pick a provider",
                    config_path.display()
                );
            }
        }
        config::Config::load()?
    };
    if !config.auto_approve {
        eprintln!(
            "warning: `auto_approve = false` in config.toml is no longer honored — \
             approval gating was removed; every tool call executes directly"
        );
    }
    config.apply_cli(&cli);

    if let Some(dir) = &cli.cwd {
        std::env::set_current_dir(dir)?;
    }

    if cli.publish {
        return evolve::run_publish_cli(config, cli).await.map(|()| 0);
    }

    if cli.evolve {
        return evolve::run_cli(config, cli).await.map(|()| 0);
    }

    if cli.gateway {
        return gateway::run(config, cli).await.map(|()| 0);
    }

    match config.mode {
        Mode::Genie => app::run_tui(config, cli).await,
        Mode::Sovereign => agent::run_headless(config, cli).await,
    }
}

/// Decide whether to run onboarding before the normal flow.
///
/// `--onboard` forces it (when a terminal is available); otherwise it runs
/// only on a genuine first run: the config file is absent, stdin/stdout are a
/// terminal, and this is not a publish / evolve / gateway invocation or a
/// headless-with-prompt sovereign run. A non-interactive run never onboards,
/// so piping into Wizard never blocks.
fn should_onboard(cli: &cli::Cli) -> Result<bool> {
    // Subcommands (bench) are dispatched before this is ever consulted; the
    // check here is a defensive guarantee that they can never onboard.
    if cli.command.is_some() {
        return Ok(false);
    }
    if cli.publish || cli.evolve || cli.gateway || cli.login.is_some() {
        return Ok(false);
    }
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if !interactive {
        return Ok(false);
    }
    if cli.onboard {
        return Ok(true);
    }
    // Headless-with-prompt sovereign runs are batch jobs — don't interrupt them.
    let headless_with_prompt =
        cli.prompt.is_some() && (cli.mode == Some(Mode::Sovereign) || cli.continuous);
    let config_missing = !config::Config::path()?.exists();
    Ok(config_missing && !headless_with_prompt)
}
