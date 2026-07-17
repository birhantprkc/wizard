//! Wizard — a single-binary, fully local agent.
//!
//! A Ratatui front end on top of an Ollama-backed agent loop with an
//! extensible tool set (native + scripted + MCP) and tiered self-extension.
//! See `docs/architecture.md` for the full design.

pub mod acp;
pub mod agent;
pub mod app;
pub mod bench;
pub mod checkpoint;
pub mod cli;
pub mod commands;
pub mod config;
pub mod credentials;
pub mod desktop;
pub mod dispatch;
pub mod doctor;
pub mod event;
pub mod evolve;
pub mod fleet;
pub mod gateway;
pub mod gui;
pub mod hardware;
pub mod harness;
pub mod hooks;
pub mod image_view;
pub mod images;
pub mod import_claude;
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
pub mod session_registry;
pub mod skills;
pub mod sync;
pub mod tools;
pub mod ui;
pub mod update;
pub mod usage;
pub mod vim;

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
    // Top-level flags are not global: the self-contained subcommands below
    // read none of them (only --cwd). Reject the combination loudly instead
    // of silently dropping the flags (`wizard --plan fleet run` must not run
    // an un-planned fleet). `wizard agents` is exempt: it goes through the
    // normal config path, where the flags do apply.
    if let Some(command) = &cli.command
        && !matches!(command, cli::Command::Agents)
    {
        let ignored = cli.ignored_top_level_flags();
        if !ignored.is_empty() {
            anyhow::bail!(
                "{} cannot be combined with a `wizard` subcommand — \
                 these top-level flags would be ignored; drop them (only --cwd applies)",
                ignored.join(", ")
            );
        }
    }

    // Harness bundle tooling is self-contained: no config, no LLM.
    // (`--harness-dir` itself is published as `$WIZARD_HARNESS_DIR` in
    // `main`, pre-runtime.)
    if let Some(cli::Command::Harness { cmd }) = &cli.command {
        if let Some(dir) = &cli.cwd {
            std::env::set_current_dir(dir)?;
        }
        return harness::run(cmd.clone()).map(|()| 0);
    }

    // Bench is self-contained tooling: it must work with no config and no
    // LLM, so dispatch before onboarding and before the config load. Its
    // exit code is nonzero when any replayed case failed, so it can gate CI.
    if let Some(cli::Command::Bench { cmd }) = &cli.command {
        if let Some(dir) = &cli.cwd {
            std::env::set_current_dir(dir)?;
        }
        return bench::run(cmd.clone()).await;
    }

    // MCP server: expose Wizard's native tools over stdio to another MCP
    // client. Self-contained — no config, no onboarding, no LLM — so it
    // dispatches before the config load like the other tooling subcommands.
    if let Some(cli::Command::McpServe { scripted }) = &cli.command {
        if let Some(dir) = &cli.cwd {
            std::env::set_current_dir(dir)?;
        }
        return mcp::serve::run(*scripted).await.map(|()| 0);
    }

    // Usage rollup: reads ~/.wizard/usage.jsonl only.
    if let Some(cli::Command::Usage { since }) = &cli.command {
        return usage::run_cli(since.as_deref());
    }

    // The browser GUI serves existing sessions and builds agents lazily per
    // task, so it loads config directly (defaults on a fresh install) and
    // never onboards — startup must not depend on a reachable provider.
    if let Some(cli::Command::Gui {
        port,
        no_open,
        assets,
    }) = &cli.command
    {
        if let Some(dir) = &cli.cwd {
            std::env::set_current_dir(dir)?;
        }
        let config = config::Config::load()?;
        return gui::run(config, *port, *no_open, assets.clone())
            .await
            .map(|()| 0);
    }

    // ACP server: an editor drives Wizard over stdin/stdout, so it must not
    // onboard or open a TUI. Loads config directly (defaults on a fresh
    // install) like the GUI, then serves until the client closes the pipe.
    if let Some(cli::Command::Acp) = &cli.command {
        if let Some(dir) = &cli.cwd {
            std::env::set_current_dir(dir)?;
        }
        let config = config::Config::load()?;
        return acp::run(config).await.map(|()| 0);
    }

    // The desktop shell is the same GUI server in a webview window, so it
    // dispatches the same way. `--install` / `--uninstall` only write launcher
    // files, and a build without the `desktop` feature only prints how to get
    // one — neither needs a config, so the load happens inside.
    if let Some(cli::Command::App {
        devtools,
        install,
        uninstall,
    }) = &cli.command
    {
        if let Some(dir) = &cli.cwd {
            std::env::set_current_dir(dir)?;
        }
        return desktop::run(desktop::AppArgs {
            devtools: *devtools,
            install: *install,
            uninstall: *uninstall,
        })
        .await;
    }

    // Evolution history: reads ~/.wizard/evolution.jsonl and touches the
    // recorded artifacts directly (list / undo) — no config, no LLM.
    if let Some(cli::Command::Evolve { cmd }) = &cli.command {
        return evolve::run_history_cli(cmd.clone());
    }

    // Self-update: `wizard update` downloads a release binary from GitHub,
    // verifies its checksum, and swaps it in. Self-contained — no config, no
    // onboarding, no LLM — so it dispatches before the config load too.
    if let Some(cli::Command::Update {
        check,
        to,
        force,
        rollback,
    }) = &cli.command
    {
        if let Some(dir) = &cli.cwd {
            std::env::set_current_dir(dir)?;
        }
        return update::run(*check, to.clone(), *force, *rollback).await;
    }

    // Config/skills sync: `wizard sync` packs and pulls signed bundles of
    // portable ~/.wizard state. Self-contained — no config load (pull reads
    // `[sync].source` from config.toml directly), no onboarding, no LLM — so
    // it dispatches before the config load like `update`.
    if let Some(cli::Command::Sync { cmd }) = &cli.command {
        if let Some(dir) = &cli.cwd {
            std::env::set_current_dir(dir)?;
        }
        return sync::run(cmd.clone()).await;
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
            "chatgpt" => llm::chatgpt_oauth::login(|line: &str| println!("{line}"))
                .await
                .map(|()| 0),
            other => {
                anyhow::bail!("unknown login provider '{other}' (supported: xai, chatgpt)")
            }
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
            let headless_with_prompt =
                cli.prompt.is_some() && (cli.mode == Some(Mode::Sovereign) || cli.continuous);
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

    // `wizard agents` always opens the TUI dashboard, regardless of the
    // configured default mode.
    if matches!(cli.command, Some(cli::Command::Agents)) {
        // Passive self-update: print any cached "update available" notice now,
        // before the TUI takes the alternate screen, then refresh the cache in
        // the background (fire-and-forget, so it never delays the TUI).
        // Sovereign is headless and skips both (handled in the match below).
        update::print_startup_notice(&config.update);
        update::maybe_check_on_startup(&config.update).await;
        return app::run_tui(config, cli).await;
    }

    match config.mode {
        Mode::Genie => {
            update::print_startup_notice(&config.update);
            update::maybe_check_on_startup(&config.update).await;
            app::run_tui(config, cli).await
        }
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
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    should_onboard_given(cli, interactive)
}

/// Testable core of [`should_onboard`]; `interactive` is whether stdin and
/// stdout are both terminals.
fn should_onboard_given(cli: &cli::Cli, interactive: bool) -> Result<bool> {
    // Subcommands (bench) are dispatched before this is ever consulted; the
    // check here is a defensive guarantee that they can never onboard.
    if cli.command.is_some() {
        return Ok(false);
    }
    if cli.publish || cli.evolve || cli.gateway || cli.login.is_some() {
        return Ok(false);
    }
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

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    fn parse(args: &[&str]) -> cli::Cli {
        cli::Cli::try_parse_from(args).expect("cli parses")
    }

    #[test]
    fn onboard_flag_forces_onboarding_in_an_interactive_terminal() {
        assert!(should_onboard_given(&parse(&["wizard", "--onboard"]), true).unwrap());
    }

    #[test]
    fn non_interactive_runs_never_onboard() {
        assert!(!should_onboard_given(&parse(&["wizard", "--onboard"]), false).unwrap());
        assert!(!should_onboard_given(&parse(&["wizard"]), false).unwrap());
    }

    #[test]
    fn subcommands_never_onboard() {
        assert!(!should_onboard_given(&parse(&["wizard", "doctor"]), true).unwrap());
        assert!(!should_onboard_given(&parse(&["wizard", "agents"]), true).unwrap());
    }

    #[test]
    fn dedicated_run_modes_suppress_onboarding() {
        for args in [
            &["wizard", "--gateway"][..],
            &["wizard", "--publish"],
            &["wizard", "--evolve"],
            &["wizard", "--login", "xai"],
        ] {
            assert!(
                !should_onboard_given(&parse(args), true).unwrap(),
                "{args:?} must not onboard"
            );
        }
    }

    #[test]
    fn headless_sovereign_prompts_skip_onboarding_even_when_interactive() {
        let sovereign = parse(&["wizard", "--mode", "sovereign", "-p", "task"]);
        assert!(!should_onboard_given(&sovereign, true).unwrap());
        let continuous = parse(&["wizard", "--continuous", "-p", "task"]);
        assert!(!should_onboard_given(&continuous, true).unwrap());
    }
}
