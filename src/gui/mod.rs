//! Browser GUI (`wizard gui`): a local web app over the same agent core as
//! the TUI.
//!
//! An axum HTTP server on `127.0.0.1:<port>` serves the embedded static
//! assets from `gui/assets/` plus a JSON API (see `docs/gui-protocol.md`):
//! task listing/creation, transcript replay, git status/commit, and a
//! WebSocket per task that streams [`AgentEvent`](crate::agent::AgentEvent)s
//! as JSON frames. Agents are built lazily — one per task, on its first
//! turn — via [`crate::agent::build_headless_agent_for_session`], so the
//! server starts fine without a reachable provider.
//!
//! No auth: the listener binds 127.0.0.1 strictly and never another
//! interface.

mod git;
mod server;
mod tasks;
mod transcript;
mod ws;

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::config::Config;

/// Shared server state: the loaded config, the in-process task manager, the
/// directory the server was launched from (where a new chat opens), and the
/// optional on-disk assets override (`--assets`, dev mode).
pub(crate) struct GuiState {
    pub config: Config,
    pub manager: tasks::TaskManager,
    pub cwd: PathBuf,
    pub assets_dir: Option<PathBuf>,
}

/// Entry point for `wizard gui`: bind 127.0.0.1:`port` (an occupied port is
/// a hard error — pass `--port` to pick another), print the URL, open the
/// browser (unless `no_open`), and serve until Ctrl-C.
pub async fn run(config: Config, port: u16, no_open: bool, assets: Option<PathBuf>) -> Result<()> {
    if let Some(dir) = &assets {
        anyhow::ensure!(
            dir.is_dir(),
            "--assets {} is not a directory",
            dir.display()
        );
    }

    let cwd = std::env::current_dir().context("reading the working directory")?;
    let state = Arc::new(GuiState {
        manager: tasks::TaskManager::new(config.clone()),
        config,
        cwd,
        assets_dir: assets,
    });
    let router = server::router(state);

    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr} — is the port taken? pass --port"))?;
    let url = format!("http://{addr}");
    println!("wizard gui — serving {url} (Ctrl-C to stop)");
    if !no_open {
        open_browser(&url);
    }

    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .context("serving the GUI")?;
    println!("\n[gui stopped]");
    Ok(())
}

/// Open `url` in the user's browser, best-effort: a missing opener must
/// never fail the server, so errors are logged and dropped.
fn open_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    if let Err(err) = std::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        tracing::warn!("could not open the browser via {opener}: {err}");
    }
}
