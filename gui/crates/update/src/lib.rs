//! Wizard GUI's self-updater.
//!
//! [`feed`] finds the newest release on GitHub and verifies it against the
//! Wizard release key ([`signature`]); [`install`] knows where this copy lives
//! and how to swap a verified release in. The engine runs an [`Updater`] that
//! checks on launch and every six hours and publishes [`UpdateStatus`] over
//! the `UpdateStatus` RPC stream, including whether a restart right now would
//! interrupt an agent run or a terminal. The UI drives the download and the
//! restart.

use std::sync::Arc;

use tokio::sync::watch;

pub mod feed;
pub mod install;
pub mod signature;
#[cfg(windows)]
pub mod windows;

pub use feed::{Release, is_newer};
pub use install::{InstallKind, Progress, Staged, detect_install};

/// The version compiled into this binary.
pub const fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Check cadence.
const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);
/// Retry sooner after a failed check (offline at launch, a GitHub hiccup).
const CHECK_RETRY: std::time::Duration = std::time::Duration::from_secs(30 * 60);
/// The first check waits out engine boot.
const CHECK_INITIAL_DELAY: std::time::Duration = std::time::Duration::from_secs(10);
/// How often [`UpdateStatus::idle`] is refreshed.
const IDLE_POLL: std::time::Duration = std::time::Duration::from_secs(2);

/// What the engine reports over the `UpdateStatus` stream.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStatus {
    pub current_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_version: Option<String>,
    #[serde(default)]
    pub update_available: bool,
    /// The newest release's page, for "What's new".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes_url: Option<String>,
    /// Epoch ms of the last successful check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<i64>,
    /// Epoch ms of the last attempt, successful or not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempted_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// No agent run and no terminal is live in this engine, so restarting it
    /// interrupts nothing. Engines that predate the field read as idle.
    #[serde(default = "default_idle")]
    pub idle: bool,
    /// A check is in flight.
    #[serde(default)]
    pub checking: bool,
}

fn default_idle() -> bool {
    true
}

impl UpdateStatus {
    fn initial() -> Self {
        Self {
            current_version: current_version().to_string(),
            latest_version: None,
            update_available: false,
            notes_url: None,
            checked_at: None,
            attempted_at: None,
            error: None,
            idle: true,
            checking: false,
        }
    }
}

/// "Nothing would be interrupted by a restart right now", wired by the engine
/// to its live-run and open-terminal registries.
pub type QuiescentCheck = Arc<dyn Fn() -> bool + Send + Sync>;

/// The release checker: fetches the signed feed on launch and every six
/// hours (or on [`Updater::check_now`]) and keeps [`UpdateStatus`] current,
/// `idle` included. It never installs anything; the UI does.
#[derive(Clone)]
pub struct Updater {
    status_tx: Arc<watch::Sender<UpdateStatus>>,
    check_tx: Arc<watch::Sender<u64>>,
    quiescent: Option<QuiescentCheck>,
    shutdown_tx: Arc<watch::Sender<bool>>,
    tasks: Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

impl Updater {
    /// Spawn the check loop (must run on a tokio runtime).
    pub fn spawn(quiescent: Option<QuiescentCheck>) -> Self {
        let (status_tx, _) = watch::channel(UpdateStatus::initial());
        let (check_tx, _) = watch::channel(0);
        let (shutdown_tx, _) = watch::channel(false);
        let updater = Self {
            status_tx: Arc::new(status_tx),
            check_tx: Arc::new(check_tx),
            quiescent,
            shutdown_tx: Arc::new(shutdown_tx),
            tasks: Arc::default(),
        };
        let checks = updater.clone();
        let idle = updater.clone();
        *updater.tasks.lock().unwrap() = vec![
            tokio::spawn(async move { checks.check_loop().await }),
            tokio::spawn(async move { idle.idle_loop().await }),
        ];
        updater
    }

    /// Stop both loops and wait for them. Idempotent.
    pub async fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
        let tasks = std::mem::take(&mut *self.tasks.lock().unwrap_or_else(|e| e.into_inner()));
        for task in tasks {
            let _ = task.await;
        }
    }

    pub fn watch(&self) -> watch::Receiver<UpdateStatus> {
        self.status_tx.subscribe()
    }

    /// Check now instead of waiting for the next scheduled check.
    pub fn check_now(&self) {
        self.check_tx
            .send_modify(|epoch| *epoch = epoch.wrapping_add(1));
    }

    fn quiescent_now(&self) -> bool {
        self.quiescent.as_ref().is_none_or(|check| check())
    }

    async fn idle_loop(&self) {
        let mut shutdown = self.shutdown_tx.subscribe();
        loop {
            let idle = self.quiescent_now();
            self.status_tx
                .send_if_modified(|status| std::mem::replace(&mut status.idle, idle) != idle);
            tokio::select! {
                _ = shutdown.wait_for(|stop| *stop) => return,
                _ = tokio::time::sleep(IDLE_POLL) => {}
            }
        }
    }

    async fn check_loop(&self) {
        let mut shutdown = self.shutdown_tx.subscribe();
        tokio::select! {
            _ = shutdown.wait_for(|stop| *stop) => {}
            _ = async {
                let mut checks = self.check_tx.subscribe();
                tokio::select! {
                    _ = tokio::time::sleep(CHECK_INITIAL_DELAY) => {}
                    _ = checks.changed() => {}
                }
                loop {
                    let ok = self.check_once().await;
                    tokio::select! {
                        _ = tokio::time::sleep(if ok { CHECK_INTERVAL } else { CHECK_RETRY }) => {}
                        _ = checks.changed() => {}
                    }
                }
            } => {}
        }
    }

    /// One check; false on failure (retry sooner).
    async fn check_once(&self) -> bool {
        self.status_tx.send_modify(|status| status.checking = true);
        let result = feed::fetch_latest().await;
        let now = now_ms();
        match result {
            Ok(release) => {
                let latest = release.version.to_string();
                let available = is_newer(&latest, current_version());
                if available {
                    tracing::info!(%latest, current = current_version(), "Wizard GUI update available");
                }
                self.status_tx.send_modify(|status| {
                    status.update_available = available;
                    status.notes_url = Some(release.notes_url());
                    status.latest_version = Some(latest);
                    status.checked_at = Some(now);
                    status.attempted_at = Some(now);
                    status.error = None;
                    status.checking = false;
                });
                true
            }
            Err(err) => {
                tracing::info!(error = %format!("{err:#}"), "update check failed");
                self.status_tx.send_modify(|status| {
                    status.error = Some(format!("{err:#}"));
                    status.attempted_at = Some(now);
                    status.checking = false;
                });
                false
            }
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_from_an_older_engine_reads_as_idle() {
        let status: UpdateStatus =
            serde_json::from_str(r#"{"currentVersion":"3.6.1","updateAvailable":false}"#).unwrap();
        assert!(status.idle);
        assert!(!status.checking);
    }

    #[tokio::test]
    async fn idle_follows_the_quiescent_gate() {
        let busy = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let gate = busy.clone();
        let updater = Updater::spawn(Some(Arc::new(move || {
            !gate.load(std::sync::atomic::Ordering::SeqCst)
        })));
        let mut status = updater.watch();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            status.wait_for(|s| !s.idle),
        )
        .await
        .unwrap()
        .unwrap();
        busy.store(false, std::sync::atomic::Ordering::SeqCst);
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            status.wait_for(|s| s.idle),
        )
        .await
        .unwrap()
        .unwrap();
        updater.shutdown().await;
    }
}
