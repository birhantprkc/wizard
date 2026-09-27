//! Files under `~/.wizard/computer/` that other processes read.
//!
//! - `vm.json`: where the running VM's VNC server listens ([`VmState`]).
//!   Written by `wizard computer vm up`, removed by `down`. Wizard GUI reads
//!   it to show the VM.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::Config;

/// `~/.wizard/computer/`.
pub fn dir() -> Result<PathBuf> {
    Ok(Config::wizard_dir()?.join("computer"))
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
