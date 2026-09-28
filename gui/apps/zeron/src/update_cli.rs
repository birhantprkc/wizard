//! `wizard-gui update`: the app's Update button from a terminal. Same feed,
//! same signature and checksum checks, same install swap; it doesn't relaunch
//! anything, so a running window keeps its version until it is reopened.

use anyhow::bail;
use zeron_update::{current_version, detect_install, is_newer};

/// `--check` prints the verdict and exits 1 when an update is available, so
/// scripts can gate on it.
pub async fn update(check_only: bool) -> anyhow::Result<()> {
    let release = zeron_update::feed::fetch_latest().await?;
    let current = current_version();
    let latest = release.version.to_string();
    if !is_newer(&latest, current) {
        println!("Wizard GUI {current} is up to date (latest: {latest}).");
        return Ok(());
    }
    println!(
        "Wizard GUI {latest} is available (this is {current}). What's new: {}",
        release.notes_url()
    );
    if check_only {
        std::process::exit(1);
    }
    let kind = detect_install();
    if let Some(reason) = kind.manual_reason() {
        bail!("{reason}\nDownload it from {}", release.notes_url());
    }
    let last = std::sync::Mutex::new(String::new());
    let staged = kind
        .stage(&release, &super::paths::data_dir(), &|progress| {
            let mut last = last.lock().unwrap();
            if *last != progress.status {
                println!("{}", progress.status);
                *last = progress.status;
            }
        })
        .await?;
    kind.apply(&staged, false)?;
    println!("Installed Wizard GUI {latest}. Reopen the app to use it.");
    Ok(())
}
