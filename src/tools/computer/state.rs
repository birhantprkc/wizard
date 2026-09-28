//! Files under `~/.wizard/computer/` that other processes read: Wizard GUI's
//! live screen panel, mostly.
//!
//! - `vm.json`: where the running VM's VNC server listens ([`VmState`]).
//!   Written by `wizard computer vm up`, removed by `down`.
//! - `last-action.json`: the agent's most recent `computer` action
//!   ([`ActionRecord`]), so the panel can draw a click ripple or a caption for
//!   typed text.
//! - `latest.png`: the host backend's most recent screenshot. The VM backend
//!   does not write it; the panel watches the VM over VNC instead.
//! - `control`: present while the user has taken control of the screen
//!   ([`ControlLease`]). The panel writes it and removes it; the `computer`
//!   tool reads it and holds its input actions until it is gone.
//!
//! Files rather than protocol messages because the panel and the agent are
//! often different processes with nothing else in common: the GUI's engine
//! may be a daemon, and a TUI session can drive the same VM the GUI shows.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::Config;

/// `~/.wizard/computer/`.
pub fn dir() -> Result<PathBuf> {
    Ok(Config::wizard_dir()?.join("computer"))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Write `bytes` to `name` in [`dir`] through a rename, so a reader polling
/// the file never sees half of it.
fn write_atomic(name: &str, bytes: &[u8]) -> Result<()> {
    let dir = dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(format!(".{name}.{}", std::process::id()));
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, dir.join(name)).with_context(|| format!("replacing {name}"))
}

/// The running VM, as `vm up` left it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmState {
    /// `host:port` of its VNC server.
    pub address: String,
    pub width: u32,
    pub height: u32,
    /// Container engine and name, or empty for a VNC server Wizard does not
    /// manage.
    pub engine: String,
    pub container: String,
}

pub fn write_vm_state(state: &VmState) -> Result<()> {
    write_atomic("vm.json", &serde_json::to_vec_pretty(state)?)
}

pub fn read_vm_state() -> Option<VmState> {
    let raw = std::fs::read(dir().ok()?.join("vm.json")).ok()?;
    serde_json::from_slice(&raw).ok()
}

pub fn clear_vm_state() {
    if let Ok(dir) = dir() {
        let _ = std::fs::remove_file(dir.join("vm.json"));
    }
}

/// One `computer` action, for the panel to draw.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionRecord {
    /// Increases by one per action, so a reader can tell a repeat of the same
    /// click from a stale file.
    pub seq: u64,
    /// Unix milliseconds.
    pub at_ms: u64,
    /// `host` or `vm`.
    pub backend: String,
    /// The action name as the model sent it (`left_click`, `type`, ...).
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub y: Option<i32>,
    /// Typed text or the pressed chord, cut to 200 characters.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// Record an action. Best effort: the panel is a convenience, and a full disk
/// must not fail the click it would have drawn.
pub fn record_action(backend: &str, action: &str, xy: Option<(i32, i32)>, text: Option<&str>) {
    let previous = dir()
        .ok()
        .and_then(|dir| std::fs::read(dir.join("last-action.json")).ok())
        .and_then(|raw| serde_json::from_slice::<ActionRecord>(&raw).ok())
        .map_or(0, |record| record.seq);
    let record = ActionRecord {
        seq: previous + 1,
        at_ms: now_ms(),
        backend: backend.to_string(),
        action: action.to_string(),
        x: xy.map(|(x, _)| x),
        y: xy.map(|(_, y)| y),
        text: text.map(|t| t.chars().take(200).collect()),
    };
    if let Ok(bytes) = serde_json::to_vec(&record)
        && let Err(err) = write_atomic("last-action.json", &bytes)
    {
        tracing::debug!("computer: could not record the last action: {err:#}");
    }
}

/// Keep the host backend's latest screenshot for the panel. Best effort.
pub fn record_frame(png: &[u8]) {
    if let Err(err) = write_atomic("latest.png", png) {
        tracing::debug!("computer: could not keep the latest frame: {err:#}");
    }
}

/// Who holds the screen while the user has taken control.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlLease {
    /// The process holding it. A lease whose process is gone is stale and
    /// ignored, so a GUI that crashed mid-takeover does not stall the agent
    /// forever.
    pub pid: u32,
    /// Who, for the message the agent gets (`Wizard GUI`).
    #[serde(default)]
    pub holder: String,
    /// Unix milliseconds.
    #[serde(default)]
    pub since_ms: u64,
}

/// The live lease, if the user holds the screen.
pub fn control_held() -> Option<ControlLease> {
    let raw = std::fs::read(dir().ok()?.join("control")).ok()?;
    let lease: ControlLease = serde_json::from_slice(&raw).ok()?;
    crate::platform::process::alive(lease.pid).then_some(lease)
}

/// Take the screen for `holder`. The GUI does this; so can a script.
pub fn take_control(holder: &str) -> Result<()> {
    let lease = ControlLease {
        pid: std::process::id(),
        holder: holder.to_string(),
        since_ms: now_ms(),
    };
    write_atomic("control", &serde_json::to_vec(&lease)?)
}

/// Hand the screen back.
pub fn give_back() {
    if let Ok(dir) = dir() {
        let _ = std::fs::remove_file(dir.join("control"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_lease_from_a_dead_process_is_ignored() {
        use crate::platform::process::alive;
        assert!(alive(std::process::id()));
        // A u32 past pid_t's range cannot name a process at all.
        assert!(!alive(u32::MAX));
    }

    #[test]
    fn records_parse_back_and_skip_absent_fields() {
        let record = ActionRecord {
            seq: 3,
            at_ms: 1,
            backend: "vm".into(),
            action: "screenshot".into(),
            x: None,
            y: None,
            text: None,
        };
        let raw = serde_json::to_string(&record).unwrap();
        assert!(!raw.contains("\"x\""), "{raw}");
        assert_eq!(serde_json::from_str::<ActionRecord>(&raw).unwrap(), record);
    }
}
