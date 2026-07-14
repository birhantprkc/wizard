//! Browser GUI (`wizard gui`): a local web app over the same agent core as
//! the TUI.
//!
//! An axum HTTP server on `127.0.0.1:<port>` serves the embedded static
//! assets from `gui/assets/` plus a JSON API (see `docs/gui-protocol.md`):
//! task listing/creation, transcript replay, git status/diffs, and a
//! WebSocket per task that streams [`AgentEvent`](crate::agent::AgentEvent)s
//! as JSON frames. Agents are built lazily — one per task, on its first
//! turn — via [`crate::agent::build_headless_agent_for_session`], so the
//! server starts fine without a reachable provider.
//!
//! No auth: the listener binds 127.0.0.1 strictly and never another
//! interface.

mod git;
mod oauth;
mod server;
mod settings;
mod tasks;
mod transcript;
mod ws;

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::config::Config;

/// Shared server state: the config store (re-read per request, so Settings
/// edits and TUI edits both take effect without a restart), the in-process
/// task manager, the directory the server was launched from (where a new chat
/// opens), and the optional on-disk assets override (`--assets`, dev mode).
pub(crate) struct GuiState {
    pub config: Arc<settings::ConfigStore>,
    pub manager: tasks::TaskManager,
    pub cwd: PathBuf,
    pub assets_dir: Option<PathBuf>,
    /// The subscription sign-in in flight, if any. `Arc` because a sign-in
    /// finishes in a spawned task that outlives the request that started it.
    pub sign_in: Arc<oauth::SignIn>,
}

/// A GUI server that is bound but not yet serving.
///
/// Binding and serving are separate steps because the desktop shell
/// (`wizard app`, `crate::desktop`) needs the port *before* it can point a
/// webview at it, and it asks the OS to pick one (`port` 0) rather than
/// racing a `wizard gui` for a fixed one. `run` below is the same two steps
/// with a browser in between.
pub struct GuiServer {
    listener: tokio::net::TcpListener,
    router: axum::Router,
    state: Arc<GuiState>,
    addr: SocketAddr,
}

/// A cleanup handle for a running [`GuiServer`], detached from the server
/// itself: the desktop shell's window event loop never returns, so it has to
/// drop the registry records from inside the loop rather than after `serve`.
/// Idempotent — dropping a record twice is a no-op.
#[derive(Clone)]
pub struct GuiShutdown(Arc<GuiState>);

impl GuiShutdown {
    /// Drop every managed task's `~/.wizard/running/` record, so a stopped
    /// server leaves nothing behind that looks like a live session.
    pub fn shutdown(&self) {
        self.0.manager.shutdown();
    }
}

impl GuiServer {
    /// Bind 127.0.0.1:`port` and build the router. `port` 0 asks the OS for a
    /// free ephemeral port — read it back with [`GuiServer::addr`]. A taken
    /// port is a hard error.
    pub async fn bind(config: Config, port: u16, assets: Option<PathBuf>) -> Result<Self> {
        if let Some(dir) = &assets {
            anyhow::ensure!(
                dir.is_dir(),
                "--assets {} is not a directory",
                dir.display()
            );
        }

        let cwd = std::env::current_dir().context("reading the working directory")?;
        let store = Arc::new(settings::ConfigStore::new(config));
        // Once for the process, not once per task: an agent build that connects its
        // own servers would give a GUI with four warm chats four copies of every
        // configured MCP server. The TUI holds one manager for the same reason —
        // and, for the same reason, `/reload` re-registers against this one rather
        // than starting a second set beside it. Agent builds take the read side, so
        // two tasks still build in parallel; only a reload has to wait for them.
        let mcp = Arc::new(tokio::sync::RwLock::new(crate::agent::connect_mcp().await));
        let state = Arc::new(GuiState {
            manager: tasks::TaskManager::new(Arc::clone(&store), mcp),
            config: store,
            cwd,
            assets_dir: assets,
            sign_in: Arc::new(oauth::SignIn::default()),
        });
        let router = server::router(Arc::clone(&state));

        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("binding {addr} — is the port taken? pass --port"))?;
        // The bound address, not the requested one: with port 0 they differ.
        let addr = listener.local_addr().context("reading the bound address")?;
        Ok(Self {
            listener,
            router,
            state,
            addr,
        })
    }

    /// The address actually bound.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// `http://127.0.0.1:<port>` — the origin the loopback guard accepts.
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// A cleanup handle usable after `self` has been moved into [`Self::serve`].
    pub fn shutdown_handle(&self) -> GuiShutdown {
        GuiShutdown(Arc::clone(&self.state))
    }

    /// Serve until `shutdown` resolves.
    pub async fn serve<F>(self, shutdown: F) -> Result<()>
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        axum::serve(self.listener, self.router)
            .with_graceful_shutdown(shutdown)
            .await
            .context("serving the GUI")?;
        // The tasks die with the process; their heartbeats would otherwise sit in
        // the registry claiming to be running until they aged out.
        self.state.manager.shutdown();
        Ok(())
    }
}

/// Entry point for `wizard gui`: bind 127.0.0.1:`port` (an occupied port is
/// a hard error — pass `--port` to pick another), print the URL, open the
/// browser (unless `no_open`), and serve until Ctrl-C.
pub async fn run(config: Config, port: u16, no_open: bool, assets: Option<PathBuf>) -> Result<()> {
    let server = GuiServer::bind(config, port, assets).await?;
    let url = server.url();
    println!("wizard gui — serving {url} (Ctrl-C to stop)");
    if !no_open {
        open_browser(&url);
    }

    server
        .serve(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    println!("\n[gui stopped]");
    Ok(())
}

/// Open `url` in the user's browser, best-effort: a missing opener must
/// never fail the server, so errors are logged and dropped. The desktop shell
/// reuses it to send outbound links to the real browser instead of loading
/// them in the app window.
pub(crate) fn open_browser(url: &str) {
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
