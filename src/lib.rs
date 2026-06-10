//! Wizard — a single-binary, fully local coding agent.
//!
//! A Ratatui front end on top of an Ollama-backed agent loop with an
//! extensible tool set (native + scripted + MCP) and tiered self-extension.
//! See `docs/architecture.md` for the full design.

pub mod agent;
pub mod app;
pub mod cli;
pub mod config;
pub mod event;
pub mod evolve;
pub mod llm;
pub mod mcp;
pub mod skills;
pub mod tools;
pub mod ui;

use anyhow::Result;

use crate::config::Mode;

/// Top-level entry point: load config, apply CLI overrides, and dispatch to
/// the selected run mode (genie TUI, sovereign headless loop, or `--evolve`).
pub async fn run(cli: cli::Cli) -> Result<()> {
    let mut config = config::Config::load()?;
    config.apply_cli(&cli);

    if let Some(dir) = &cli.cwd {
        std::env::set_current_dir(dir)?;
    }

    if cli.publish {
        return evolve::run_publish_cli(config, cli).await;
    }

    if cli.evolve {
        return evolve::run_cli(config, cli).await;
    }

    match config.mode {
        Mode::Genie => app::run_tui(config, cli).await,
        Mode::Sovereign => agent::run_headless(config, cli).await,
    }
}
