//! In-app updates. The engine's release checker says whether a newer Wizard
//! GUI exists (the `UpdateStatus` stream); [`Updates`] owns what this process
//! does about it: download and stage it with a progress bar, then install it
//! when the app quits, relaunching when the user asked for a restart. One per
//! process, shared by the sidebar banner and Settings → About.
//!
//! A restart is only offered while the engine reports `idle` (no agent run,
//! no open terminal). Quitting always installs a staged update, so a machine
//! that is never idle still picks it up the next time the app is closed.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui::{App, AppContext as _, Context, Entity, Global, Subscription, Task};
use gpui_tokio::Tokio;
use zeron_rpc::methods;
use zeron_update::{InstallKind, Progress, Staged, UpdateStatus};

use crate::state::AppState;

#[derive(Debug, Clone, PartialEq)]
pub enum Flow {
    Idle,
    Downloading,
    /// Verified and unpacked; installs on quit or on "Restart to update".
    Ready(Staged),
    Failed(String),
}

pub struct Updates {
    state: Entity<AppState>,
    data_dir: PathBuf,
    install: InstallKind,
    flow: Flow,
    progress: Arc<Mutex<Option<Progress>>>,
    /// The version whose banner was closed; a newer one shows again.
    dismissed: Option<String>,
    /// Set by "Restart to update": the quit that follows relaunches.
    relaunch_on_quit: bool,
    task: Option<Task<()>>,
    poll: Option<Task<()>>,
    _state: Subscription,
}

struct GlobalUpdates(Entity<Updates>);
impl Global for GlobalUpdates {}

/// Create the process's [`Updates`]. Call once at boot.
pub fn init(state: Entity<AppState>, data_dir: PathBuf, cx: &mut App) -> Entity<Updates> {
    let install = zeron_update::detect_install();
    tracing::info!(
        version = zeron_update::current_version(),
        install = ?install,
        test_key = zeron_update::signature::test_key_build(),
        "Wizard GUI updater ready"
    );
    let updates = cx.new(|cx| Updates::new(state, data_dir, install, cx));
    cx.set_global(GlobalUpdates(updates.clone()));
    updates
}

pub fn global(cx: &App) -> Option<Entity<Updates>> {
    cx.try_global::<GlobalUpdates>().map(|g| g.0.clone())
}

/// The app is quitting: put a staged update in place, relaunching if the
/// user asked for a restart. Runs inside `on_app_quit`.
pub fn install_on_quit(cx: &mut App) {
    if let Some(updates) = global(cx) {
        updates.update(cx, |updates, _| updates.install_now());
    }
}

impl Updates {
    fn new(
        state: Entity<AppState>,
        data_dir: PathBuf,
        install: InstallKind,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.observe(&state, |this, _, cx| this.on_status(cx));
        Self {
            state,
            data_dir,
            install,
            flow: Flow::Idle,
            progress: Arc::default(),
            dismissed: None,
            relaunch_on_quit: false,
            task: None,
            poll: None,
            _state: subscription,
        }
    }

    pub fn status(&self, cx: &App) -> Option<UpdateStatus> {
        self.state.read(cx).update.clone()
    }

    /// The newer version on offer, if there is one. Compared against this
    /// binary rather than the engine's, which may be a separate daemon.
    pub fn available(&self, cx: &App) -> Option<String> {
        let latest = self.status(cx)?.latest_version?;
        zeron_update::is_newer(&latest, zeron_update::current_version()).then_some(latest)
    }

    pub fn notes_url(&self, cx: &App) -> Option<String> {
        self.status(cx)?.notes_url
    }

    /// Restarting now would not cut off a run or a terminal.
    pub fn idle(&self, cx: &App) -> bool {
        self.status(cx).is_none_or(|status| status.idle)
    }

    pub fn install_kind(&self) -> &InstallKind {
        &self.install
    }

    pub fn flow(&self) -> &Flow {
        &self.flow
    }

    pub fn progress(&self) -> Option<Progress> {
        self.progress.lock().ok()?.clone()
    }

    pub fn auto_install(cx: &App) -> bool {
        crate::settings::current(cx).auto_install_updates
    }

    pub fn set_auto_install(&mut self, on: bool, cx: &mut Context<Self>) {
        crate::settings::update(crate::settings::SavePolicy::Immediate, cx, |settings| {
            settings.auto_install_updates = on;
        });
        self.on_status(cx);
        cx.notify();
    }

    /// Whether the sidebar banner shows.
    pub fn banner_visible(&self, cx: &App) -> bool {
        match self.available(cx) {
            Some(latest) => {
                !matches!(self.flow, Flow::Idle) || self.dismissed.as_deref() != Some(&latest)
            }
            None => false,
        }
    }

    pub fn dismiss(&mut self, cx: &mut Context<Self>) {
        self.dismissed = self.available(cx);
        cx.notify();
    }

    /// Ask the engine to check now.
    pub fn check_now(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        cx.spawn(async move |_, _| {
            if let Err(err) = engine
                .client()
                .call(methods::CHECK_FOR_UPDATES, serde_json::Value::Null)
                .await
            {
                tracing::warn!(error = %err, "update check request failed");
            }
        })
        .detach();
    }

    fn on_status(&mut self, cx: &mut Context<Self>) {
        let latest = self.available(cx);
        // A newer release than the one staged replaces it.
        if let Flow::Ready(staged) = &self.flow
            && latest.as_deref() != Some(staged.version.as_str())
            && latest.is_some()
        {
            self.flow = Flow::Idle;
        }
        if latest.is_some()
            && matches!(self.flow, Flow::Idle)
            && self.install.can_self_update()
            && Self::auto_install(cx)
        {
            self.start(cx);
        }
    }

    /// Download, verify and stage the newest release.
    pub fn start(&mut self, cx: &mut Context<Self>) {
        if matches!(self.flow, Flow::Downloading | Flow::Ready(_)) {
            return;
        }
        if let Some(reason) = self.install.manual_reason() {
            self.flow = Flow::Failed(reason.to_string());
            cx.notify();
            return;
        }
        self.flow = Flow::Downloading;
        *self.progress.lock().unwrap() = None;
        let install = self.install.clone();
        let data_dir = self.data_dir.clone();
        let progress = self.progress.clone();
        let download = Tokio::spawn(cx, async move {
            let release = zeron_update::feed::fetch_latest().await?;
            let report = move |p: Progress| {
                if let Ok(mut slot) = progress.lock() {
                    *slot = Some(p);
                }
            };
            install.stage(&release, &data_dir, &report).await
        });
        self.task = Some(cx.spawn(async move |this, cx| {
            let outcome = match download.await {
                Ok(Ok(staged)) => Ok(staged),
                Ok(Err(err)) => Err(format!("{err:#}")),
                Err(err) => Err(err.to_string()),
            };
            this.update(cx, |this, cx| {
                this.poll = None;
                this.flow = match outcome {
                    Ok(staged) => {
                        tracing::info!(version = %staged.version, "Wizard GUI update staged");
                        Flow::Ready(staged)
                    }
                    Err(message) => {
                        tracing::warn!(%message, "Wizard GUI update failed");
                        Flow::Failed(message)
                    }
                };
                cx.notify();
            })
            .ok();
        }));
        self.poll = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        }));
        cx.notify();
    }

    /// "Restart to update": quit through the normal path (unsaved files
    /// still get their prompt), install on the way out, and relaunch.
    pub fn restart(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.flow, Flow::Ready(_)) || !self.idle(cx) {
            return;
        }
        self.relaunch_on_quit = true;
        crate::app_menus::request_quit(cx);
    }

    fn install_now(&mut self) {
        let Flow::Ready(staged) = std::mem::replace(&mut self.flow, Flow::Idle) else {
            return;
        };
        match self.install.apply(&staged, self.relaunch_on_quit) {
            Ok(()) => tracing::info!(version = %staged.version, "installed Wizard GUI update"),
            Err(err) => {
                tracing::error!(error = %format!("{err:#}"), "installing the update failed")
            }
        }
    }
}

/// "3m ago" for Settings.
pub fn format_checked(at_ms: Option<i64>, now_ms: i64) -> String {
    let Some(at) = at_ms else {
        return "never".to_string();
    };
    let secs = (now_ms - at).max(0) / 1000;
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

/// The install bar's view of an update's progress.
pub fn bar_progress(
    progress: Option<Progress>,
) -> Option<zeron_harness::install_progress::InstallProgress> {
    progress.map(|p| zeron_harness::install_progress::InstallProgress {
        status: p.status,
        fraction: p.fraction,
        detail: p.detail,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checked_times_read_like_the_devices_page() {
        assert_eq!(format_checked(None, 0), "never");
        assert_eq!(format_checked(Some(0), 30_000), "just now");
        assert_eq!(format_checked(Some(0), 5 * 60_000), "5m ago");
        assert_eq!(format_checked(Some(0), 3 * 3_600_000), "3h ago");
        assert_eq!(format_checked(Some(0), 2 * 86_400_000), "2d ago");
        assert_eq!(format_checked(Some(10_000), 0), "just now");
    }
}
