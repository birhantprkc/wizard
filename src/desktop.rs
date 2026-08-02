//! Native desktop shell (`wizard app`): the browser GUI in its own window.
//!
//! One process, no sidecar, no IPC bridge. The GUI server ([`crate::gui`])
//! already runs in-process, so `wizard app` binds it to an OS-chosen
//! ephemeral loopback port and points a [`wry`] webview at
//! `http://127.0.0.1:<port>/`. The webview is the *system* one — WebKitGTK on
//! Linux, WKWebView on macOS — so the shell costs about ten megabytes rather
//! than the ~200MB of a bundled Chromium.
//!
//! # The `desktop` feature
//!
//! Everything that links a webview sits behind `--features desktop`, which is
//! **off by default and must stay off**: wry links WebKitGTK dynamically, so a
//! default build with the feature on would refuse to *start at all* — TUI, CLI
//! and every subcommand — on any Linux box without `libwebkit2gtk`, and it
//! could no longer be linked statically against musl.
//!
//! The launcher installer below (`wizard app --install`) is compiled either
//! way: it writes files and shells out, and depends on nothing new. That keeps
//! its behaviour under test in the default `cargo test` run.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// The app's mark: the wand the GUI already uses as its favicon
/// (`gui/assets/index.html`, `FAVICON_SVG` in `src/gui/server.rs`). Embedded,
/// not read from the source tree - `--install` runs from an installed binary
/// with no repository anywhere near it.
const ICON_SVG: &str = include_str!("../assets/wizard.svg");
const ICON_PNG: &[u8] = include_bytes!("../assets/wizard-512.png");

/// Display name: window title, launcher entry, `Wizard.app`.
const APP_NAME: &str = "Wizard";
/// Freedesktop application ID / icon name / `.desktop` basename, and the GTK
/// app id — which is what a Linux desktop matches `StartupWMClass` against.
const APP_ID: &str = "wizard";
/// macOS bundle identifier.
const BUNDLE_ID: &str = "com.teddytennant.wizard";

/// What `wizard app` was asked to do. Kept as one struct so the no-feature
/// build's stub and the real shell take the same arguments.
#[derive(Debug, Clone, Copy)]
pub struct AppArgs {
    /// Open the webview inspector on start.
    pub devtools: bool,
    /// Install the launcher entry (`.desktop` / `Wizard.app`) and exit.
    pub install: bool,
    /// Remove the launcher entry and exit.
    pub uninstall: bool,
}

// ---------------------------------------------------------------------------
// Display backend
// ---------------------------------------------------------------------------

/// Choose the GDK backend for the app window, before GTK is initialized.
///
/// WebKitGTK's Wayland backend computes a **negative** device pixel ratio here:
/// `devicePixelRatio` comes back as `-0.0208`, `innerWidth` as `-138240`, and
/// `height: 100vh` — which the GUI's shell uses — resolves to some hundreds of
/// millions of pixels, so the layout explodes. It is not our bug and not the
/// GUI's CSS: a fourteen-line stock `wry` example reproduces the identical
/// figure on the same machine (Hyprland, an output at fractional scale 1.33).
/// GTK 3 has no fractional-scale support with which to fix it.
///
/// The same binary under XWayland renders correctly (`dpr = 1`), XWayland is
/// present on every mainstream Wayland desktop, and the cost is a slightly
/// softer image on a HiDPI screen — a trade worth making against a window that
/// cannot be used at all. An explicit `GDK_BACKEND` always wins, so
/// `GDK_BACKEND=wayland wizard app` opts back in.
///
/// # Safety
///
/// Writes the environment, so it must be called before any thread is spawned —
/// i.e. from `main`, before the tokio runtime is built.
pub unsafe fn select_display_backend() {
    #[cfg(target_os = "linux")]
    {
        // The user's choice, or an X11 session: nothing to do either way.
        if std::env::var_os("GDK_BACKEND").is_some()
            || std::env::var_os("WAYLAND_DISPLAY").is_none()
        {
            return;
        }
        // SAFETY: the caller guarantees we are single-threaded.
        unsafe { std::env::set_var("GDK_BACKEND", "x11") };
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// `wizard app` without the `desktop` feature: the subcommand still exists —
/// an "unknown subcommand" error would be a lie about what Wizard can do — but
/// there is no webview in this binary, so say so and say how to get one.
#[cfg(not(feature = "desktop"))]
pub async fn run(args: AppArgs) -> Result<i32> {
    let _ = args;
    eprintln!(
        "this wizard binary has no desktop shell — it was built without the `desktop` feature.\n\
         \n\
         The shell is opt-in because it links the system webview (WebKitGTK on Linux), which\n\
         every plain `wizard` user would otherwise have to install just to start the binary.\n\
         \n\
         To get it:\n\
         \x20 curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh \\\n\
         \x20   | WIZARD_APP=1 bash        # installs `wizard-desktop` and adds it to your launcher\n\
         \x20 cargo build --release --features desktop   # from a checkout\n\
         \n\
         In the meantime, `wizard gui` serves the same interface in your browser.\n\
         See docs/desktop.md."
    );
    Ok(1)
}

/// `wizard app`: install/uninstall the launcher entry, or open the window.
#[cfg(feature = "desktop")]
pub async fn run(args: AppArgs) -> Result<i32> {
    if args.install && args.uninstall {
        anyhow::bail!("--install and --uninstall are mutually exclusive");
    }
    if args.install {
        return install_launcher().map(|()| 0);
    }
    if args.uninstall {
        return uninstall_launcher().map(|()| 0);
    }
    let config = crate::config::Config::load()?;
    shell::open(config, args.devtools).await
}

// ---------------------------------------------------------------------------
// Window geometry, persisted
// ---------------------------------------------------------------------------

/// The window's last size and position, kept in `~/.wizard/desktop.toml` so
/// the app reopens where it was left.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowState {
    pub width: u32,
    pub height: u32,
    /// Absent on first run: let the window manager place the window.
    pub x: Option<i32>,
    pub y: Option<i32>,
    pub maximized: bool,
}

impl Default for WindowState {
    fn default() -> Self {
        // Wide enough for the GUI's three panes (sidebar, conversation, git)
        // without the layout collapsing to its narrow breakpoint.
        Self {
            width: 1440,
            height: 900,
            x: None,
            y: None,
            maximized: false,
        }
    }
}

impl WindowState {
    /// `~/.wizard/desktop.toml`.
    pub fn path() -> Result<PathBuf> {
        Ok(crate::config::Config::wizard_dir()?.join("desktop.toml"))
    }

    /// Load the saved geometry. A missing or corrupt file is not an error —
    /// a bad geometry file must never stop the app opening; fall back to the
    /// default size.
    pub fn load() -> Self {
        let Ok(path) = Self::path() else {
            return Self::default();
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        match toml::from_str::<Self>(&text) {
            Ok(state) => state.sanitized(),
            Err(err) => {
                tracing::warn!("ignoring {}: {err}", path.display());
                Self::default()
            }
        }
    }

    /// Reject geometry that would open an unusable window — a zero or absurd
    /// size from a corrupt file, or from a monitor that no longer exists.
    fn sanitized(mut self) -> Self {
        let default = Self::default();
        if !(320..=16_384).contains(&self.width) {
            self.width = default.width;
        }
        if !(240..=16_384).contains(&self.height) {
            self.height = default.height;
        }
        // A window restored far off-canvas (unplugged monitor) is a window the
        // user cannot find. Drop the position and let the WM place it.
        if self.x.is_some_and(|x| !(-16_384..=16_384).contains(&x))
            || self.y.is_some_and(|y| !(-16_384..=16_384).contains(&y))
        {
            self.x = None;
            self.y = None;
        }
        self
    }

    /// Best-effort persist: failing to save geometry must not fail a quit.
    pub fn save(&self) {
        let Ok(path) = Self::path() else { return };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match toml::to_string_pretty(self) {
            Ok(text) => {
                if let Err(err) = std::fs::write(&path, text) {
                    tracing::warn!("could not save the window geometry: {err}");
                }
            }
            Err(err) => tracing::warn!("could not serialize the window geometry: {err}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Launcher install: the files
// ---------------------------------------------------------------------------

/// The `.desktop` entry for `exe`.
///
/// `Exec` is the absolute path of the *running* binary plus ` app`, not a bare
/// `wizard app`: the desktop file is launched by the DE with a PATH that
/// usually has nothing user-installed on it.
pub fn desktop_entry(exe: &Path) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Version=1.0\n\
         Name={APP_NAME}\n\
         GenericName=AI Agent\n\
         Comment=Your sovereign agent. Self-extending. Bring any model.\n\
         Exec={exec} app\n\
         Icon={APP_ID}\n\
         Terminal=false\n\
         Categories=Development;\n\
         Keywords=agent;ai;llm;coding;\n\
         StartupNotify=true\n\
         StartupWMClass={class}\n",
        exec = exec_quote(exe),
        class = wm_class(exe),
    )
}

/// The window class the desktop will see, which is what `StartupWMClass` has to
/// match for the running window to group under this launcher entry.
///
/// It derives from the binary's own file name, not from our GTK application id:
/// GTK 3 takes the class from the program name (`argv[0]`). But it does not take
/// it *verbatim* — GDK builds the class by upper-casing the first character of
/// the program name (`gdk_get_program_class`), so a binary run as `wizard`
/// presents the X11 `WM_CLASS` pair `("wizard", "Wizard")` and compositors
/// report the second. A live window on Hyprland reports `Wizard`, which is why
/// the raw file name here would never match and the running app would fall back
/// to a generic icon instead of this entry's.
///
/// A pure-Wayland GTK window would instead present the `app_id` set on the event
/// loop ([`BUNDLE_ID`]); desktops match that against the *file name* of the entry
/// rather than `StartupWMClass`, so nothing here needs to know about it.
fn wm_class(exe: &Path) -> String {
    let name = exe
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| APP_ID.to_string());
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => name,
    }
}

/// Quote a path for a `.desktop` `Exec=` line (freedesktop entry spec §Exec):
/// reserved characters must be inside double quotes, and `"`, `` ` ``, `$` and
/// `\` escaped with a backslash. Paths made only of safe characters are left
/// bare, which is the overwhelmingly common case and keeps the file readable.
fn exec_quote(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let safe = raw
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '+' | ','));
    if safe && !raw.is_empty() {
        return raw.into_owned();
    }
    let mut out = String::with_capacity(raw.len() + 2);
    out.push('"');
    for c in raw.chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// `Contents/Info.plist` for `Wizard.app`.
///
/// No `CFBundleSignature`, no code signature, no notarization: a bundle we
/// *generate on the user's own machine* never passes through the quarantine
/// (`com.apple.quarantine` is set by the downloader, not by the filesystem),
/// so Gatekeeper does not gate it. That is the whole reason this approach can
/// work without an Apple Developer account — a downloaded, unsigned `.app`
/// would be refused, one written locally by `wizard app --install` is not.
pub fn info_plist() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleName</key>
	<string>{APP_NAME}</string>
	<key>CFBundleDisplayName</key>
	<string>{APP_NAME}</string>
	<key>CFBundleIdentifier</key>
	<string>{BUNDLE_ID}</string>
	<key>CFBundleVersion</key>
	<string>{version}</string>
	<key>CFBundleShortVersionString</key>
	<string>{version}</string>
	<key>CFBundleExecutable</key>
	<string>{APP_NAME}</string>
	<key>CFBundleIconFile</key>
	<string>{APP_ID}</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>LSMinimumSystemVersion</key>
	<string>10.15</string>
	<key>NSHighResolutionCapable</key>
	<true/>
	<key>LSApplicationCategoryType</key>
	<string>public.app-category.developer-tools</string>
</dict>
</plist>
"#,
        version = env!("CARGO_PKG_VERSION"),
    )
}

/// `Contents/MacOS/Wizard`: a three-line trampoline that execs the real binary
/// with `app`. The bundle holds no copy of Wizard — `wizard update` then keeps
/// the app up to date for free, and there is one binary on disk, not two.
pub fn launcher_stub(exe: &Path) -> String {
    format!(
        "#!/bin/sh\n\
         # Generated by `wizard app --install`. Runs the Wizard binary this\n\
         # bundle was installed from; re-run --install if you move it.\n\
         exec {exec} app \"$@\"\n",
        exec = shell_quote(exe),
    )
}

/// Single-quote a path for `/bin/sh`.
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', r"'\''"))
}

// ---------------------------------------------------------------------------
// Launcher install: the filesystem
// ---------------------------------------------------------------------------

/// Install the launcher entry for the running binary. Idempotent: every write
/// truncates, and a second run reproduces the same tree.
pub fn install_launcher() -> Result<()> {
    let exe = current_exe()?;
    let home = home()?;
    let written = if cfg!(target_os = "macos") {
        install_macos(&home.join("Applications"), &exe)?
    } else {
        install_linux(&home.join(".local/share"), &exe)?
    };
    for path in &written {
        println!("wrote {}", path.display());
    }
    println!("\n{APP_NAME} is in your launcher. Remove it with `wizard app --uninstall`.");
    Ok(())
}

/// Remove the launcher entry. Idempotent: removing what is not there is fine.
pub fn uninstall_launcher() -> Result<()> {
    let home = home()?;
    let removed = if cfg!(target_os = "macos") {
        uninstall_macos(&home.join("Applications"))?
    } else {
        uninstall_linux(&home.join(".local/share"))?
    };
    if removed.is_empty() {
        println!("nothing to remove — {APP_NAME} is not installed in your launcher.");
        return Ok(());
    }
    for path in &removed {
        println!("removed {}", path.display());
    }
    Ok(())
}

/// Linux: a `.desktop` entry plus a scalable and a 512×512 icon, under
/// `~/.local/share` (the per-user half of the XDG data dirs — no root, no
/// package manager). `share` is a parameter so the tests can install into a
/// temporary directory instead of the user's real home.
pub fn install_linux(share: &Path, exe: &Path) -> Result<Vec<PathBuf>> {
    let mut written = Vec::new();

    let apps = share.join("applications");
    std::fs::create_dir_all(&apps).with_context(|| format!("creating {}", apps.display()))?;
    let entry = apps.join(format!("{APP_ID}.desktop"));
    std::fs::write(&entry, desktop_entry(exe))
        .with_context(|| format!("writing {}", entry.display()))?;
    written.push(entry);

    let scalable = share.join("icons/hicolor/scalable/apps");
    std::fs::create_dir_all(&scalable)
        .with_context(|| format!("creating {}", scalable.display()))?;
    let svg = scalable.join(format!("{APP_ID}.svg"));
    std::fs::write(&svg, ICON_SVG).with_context(|| format!("writing {}", svg.display()))?;
    written.push(svg);

    let raster = share.join("icons/hicolor/512x512/apps");
    std::fs::create_dir_all(&raster).with_context(|| format!("creating {}", raster.display()))?;
    let png = raster.join(format!("{APP_ID}.png"));
    std::fs::write(&png, ICON_PNG).with_context(|| format!("writing {}", png.display()))?;
    written.push(png);

    // Tell the desktop the entry exists. Absent on a minimal system, and some
    // desktops rescan on their own — never fail the install over it.
    run_quietly("update-desktop-database", &[apps_dir_arg(share)]);
    run_quietly("gtk-update-icon-cache", &[icon_dir_arg(share)]);

    Ok(written)
}

/// Undo [`install_linux`]. Returns what actually existed.
pub fn uninstall_linux(share: &Path) -> Result<Vec<PathBuf>> {
    let removed = remove_all(&[
        share.join(format!("applications/{APP_ID}.desktop")),
        share.join(format!("icons/hicolor/scalable/apps/{APP_ID}.svg")),
        share.join(format!("icons/hicolor/512x512/apps/{APP_ID}.png")),
    ])?;
    if !removed.is_empty() {
        run_quietly("update-desktop-database", &[apps_dir_arg(share)]);
        run_quietly("gtk-update-icon-cache", &[icon_dir_arg(share)]);
    }
    Ok(removed)
}

fn apps_dir_arg(share: &Path) -> PathBuf {
    share.join("applications")
}

fn icon_dir_arg(share: &Path) -> PathBuf {
    share.join("icons/hicolor")
}

/// macOS: `~/Applications/Wizard.app`, the standard three-directory bundle.
/// `apps` is a parameter for the same reason as `share` above.
pub fn install_macos(apps: &Path, exe: &Path) -> Result<Vec<PathBuf>> {
    let bundle = apps.join(format!("{APP_NAME}.app"));
    let contents = bundle.join("Contents");
    let macos = contents.join("MacOS");
    let resources = contents.join("Resources");
    std::fs::create_dir_all(&macos).with_context(|| format!("creating {}", macos.display()))?;
    std::fs::create_dir_all(&resources)
        .with_context(|| format!("creating {}", resources.display()))?;

    let mut written = Vec::new();

    let plist = contents.join("Info.plist");
    std::fs::write(&plist, info_plist()).with_context(|| format!("writing {}", plist.display()))?;
    written.push(plist);

    let stub = macos.join(APP_NAME);
    std::fs::write(&stub, launcher_stub(exe))
        .with_context(|| format!("writing {}", stub.display()))?;
    set_executable(&stub)?;
    written.push(stub);

    // The icon is a nice-to-have: `sips`/`iconutil` ship with macOS, but if a
    // stripped system lacks them the app should still install — it just gets
    // the generic bundle icon.
    match write_icns(&resources.join(format!("{APP_ID}.icns"))) {
        Ok(Some(icns)) => written.push(icns),
        Ok(None) => eprintln!(
            "note: no `sips`/`iconutil` on PATH — installed without an icon (the app still runs)"
        ),
        Err(err) => {
            eprintln!("note: could not generate the app icon ({err:#}) — installing without one")
        }
    }

    Ok(written)
}

/// Undo [`install_macos`]: remove the whole bundle (we created every file in
/// it, so there is nothing of the user's to preserve).
pub fn uninstall_macos(apps: &Path) -> Result<Vec<PathBuf>> {
    let bundle = apps.join(format!("{APP_NAME}.app"));
    if !bundle.exists() {
        return Ok(Vec::new());
    }
    std::fs::remove_dir_all(&bundle).with_context(|| format!("removing {}", bundle.display()))?;
    Ok(vec![bundle])
}

/// Build an `.icns` from the embedded PNG with the macOS tools, at install
/// time rather than in the repository — an `.icns` is a build artifact and it
/// would be one more thing to keep in step with the SVG.
///
/// `Ok(None)` when the tools are missing; the caller installs without an icon.
fn write_icns(dest: &Path) -> Result<Option<PathBuf>> {
    if !cfg!(target_os = "macos") || !have("sips") || !have("iconutil") {
        return Ok(None);
    }
    let scratch = tempdir_in_wizard()?;
    let iconset = scratch.join(format!("{APP_ID}.iconset"));
    std::fs::create_dir_all(&iconset)?;
    let source = scratch.join("icon-512.png");
    std::fs::write(&source, ICON_PNG)?;

    // The sizes `iconutil` expects. 1024 is upscaled from our 512 master; the
    // mark is a flat vector shape, so it survives it.
    for (size, name) in [
        (16, "icon_16x16.png"),
        (32, "icon_16x16@2x.png"),
        (32, "icon_32x32.png"),
        (64, "icon_32x32@2x.png"),
        (128, "icon_128x128.png"),
        (256, "icon_128x128@2x.png"),
        (256, "icon_256x256.png"),
        (512, "icon_256x256@2x.png"),
        (512, "icon_512x512.png"),
        (1024, "icon_512x512@2x.png"),
    ] {
        let out = iconset.join(name);
        let status = std::process::Command::new("sips")
            .args(["-z", &size.to_string(), &size.to_string()])
            .arg(&source)
            .arg("--out")
            .arg(&out)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .context("running sips")?;
        anyhow::ensure!(status.success(), "sips failed for the {size}px icon");
    }

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let status = std::process::Command::new("iconutil")
        .args(["-c", "icns"])
        .arg(&iconset)
        .arg("-o")
        .arg(dest)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context("running iconutil")?;
    let _ = std::fs::remove_dir_all(&scratch);
    anyhow::ensure!(status.success(), "iconutil failed");
    Ok(Some(dest.to_path_buf()))
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn home() -> Result<PathBuf> {
    dirs::home_dir().context("could not determine the home directory")
}

/// The absolute, symlink-resolved path of the running binary — what the
/// launcher entry has to point at.
fn current_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("locating the running wizard binary")?;
    Ok(std::fs::canonicalize(&exe).unwrap_or(exe))
}

fn remove_all(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut removed = Vec::new();
    for path in paths {
        match std::fs::remove_file(path) {
            Ok(()) => removed.push(path.clone()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err).with_context(|| format!("removing {}", path.display())),
        }
    }
    Ok(removed)
}

fn have(program: &str) -> bool {
    std::process::Command::new(program)
        .arg("--help")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

/// Run a cache-refresh command, ignoring every failure: these tools are
/// optional, and a desktop that has none of them still shows the entry.
fn run_quietly(program: &str, args: &[PathBuf]) {
    let _ = std::process::Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

fn set_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("making {} executable", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// A scratch directory under `~/.wizard` (not `/tmp`: we already own this one,
/// and it is on the same filesystem as everything else we write).
fn tempdir_in_wizard() -> Result<PathBuf> {
    let base = crate::config::Config::wizard_dir()?.join("tmp");
    let dir = base.join(format!("icns.{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

// ---------------------------------------------------------------------------
// The window (feature `desktop` only)
// ---------------------------------------------------------------------------

#[cfg(feature = "desktop")]
mod shell {
    use anyhow::{Context, Result};
    use tao::event::{Event, StartCause, WindowEvent};
    use tao::event_loop::{ControlFlow, EventLoopBuilder};
    use tao::window::WindowBuilder;
    use wry::WebViewBuilder;

    use super::{APP_NAME, BUNDLE_ID, ICON_PNG, WindowState};
    use crate::config::Config;
    use crate::gui::GuiServer;

    /// Open the app: bind the GUI server on an ephemeral loopback port, serve
    /// it on the tokio runtime, and run the window's event loop on this (the
    /// main) thread — which is where GTK and AppKit both insist on living.
    ///
    /// Never returns: `EventLoop::run` diverges (it exits the process), so the
    /// quit path is the `LoopDestroyed` arm below.
    pub async fn open(config: Config, devtools: bool) -> Result<i32> {
        preflight()?;

        // Port 0: the OS picks a free one. A fixed port would collide with a
        // `wizard gui` already serving on it.
        let server = GuiServer::bind(config, 0, None).await?;
        let origin = server.url();
        let cleanup = server.shutdown_handle();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();

        // The server runs on the runtime's worker threads; the event loop below
        // blocks this one forever.
        tokio::spawn(async move {
            if let Err(err) = server
                .serve(async {
                    let _ = stop_rx.await;
                })
                .await
            {
                eprintln!("error: the GUI server stopped: {err:#}");
            }
        });

        let state = WindowState::load();
        let event_loop = {
            let mut builder = EventLoopBuilder::new();
            // The GTK application id. It must be a valid GApplication id —
            // reverse-DNS, at least two segments — or `gtk_application_new`
            // rejects it and every GTK call downstream runs against a NULL
            // application (a screenful of GLib-CRITICAL, and no D-Bus
            // registration). It is *not* what the window's class ends up as:
            // GTK 3 takes that from the program name. See `wm_class`.
            #[cfg(target_os = "linux")]
            {
                use tao::platform::unix::EventLoopBuilderExtUnix;
                builder.with_app_id(BUNDLE_ID);
            }
            builder.build()
        };

        let mut window = WindowBuilder::new()
            .with_title(APP_NAME)
            .with_inner_size(tao::dpi::LogicalSize::new(state.width, state.height))
            .with_min_inner_size(tao::dpi::LogicalSize::new(640, 480))
            .with_maximized(state.maximized)
            .with_window_icon(window_icon());
        if let (Some(x), Some(y)) = (state.x, state.y) {
            window = window.with_position(tao::dpi::PhysicalPosition::new(x, y));
        }
        let window = window
            .build(&event_loop)
            .context("opening the app window")?;

        let builder = WebViewBuilder::new()
            .with_url(&origin)
            .with_devtools(devtools)
            // The GUI is dark; painting the webview dark too avoids a white
            // flash between the window mapping and the first paint.
            .with_background_color((12, 12, 14, 255))
            .with_navigation_handler(external_links(origin.clone()))
            .with_new_window_req_handler(new_window(origin.clone()));

        #[cfg(not(target_os = "linux"))]
        let webview = builder.build(&window).map_err(webview_error)?;
        #[cfg(target_os = "linux")]
        let webview = {
            // The GTK path rather than the raw window handle: it is the one
            // that works on Wayland as well as X11.
            use tao::platform::unix::WindowExtUnix;
            use wry::WebViewBuilderExtUnix;
            let vbox = window
                .default_vbox()
                .context("the app window has no GTK container")?;
            builder.build_gtk(vbox).map_err(webview_error)?
        };

        if devtools {
            webview.open_devtools();
        }

        let mut stop_tx = Some(stop_tx);
        event_loop.run(move |event, _, control_flow| {
            *control_flow = ControlFlow::Wait;
            match event {
                Event::NewEvents(StartCause::Init) => {}
                Event::WindowEvent {
                    event: WindowEvent::CloseRequested,
                    ..
                } => {
                    save_geometry(&window);
                    *control_flow = ControlFlow::Exit;
                }
                Event::LoopDestroyed => {
                    // The process is about to exit and the window is gone, so
                    // this is the last chance to leave the session registry
                    // clean: without it every task this window opened would sit
                    // in ~/.wizard/running/ looking like a live agent until it
                    // aged out as stale.
                    cleanup.shutdown();
                    if let Some(tx) = stop_tx.take() {
                        let _ = tx.send(());
                    }
                }
                _ => {}
            }
        })
    }

    /// Store the geometry the window has *right now*. Called on close, while
    /// the window still exists — `LoopDestroyed` is too late to ask it.
    fn save_geometry(window: &tao::window::Window) {
        let size = window.inner_size();
        let scale = window.scale_factor();
        let logical = size.to_logical::<u32>(scale);
        let position = window.outer_position().ok();
        WindowState {
            width: logical.width,
            height: logical.height,
            x: position.map(|p| p.x),
            y: position.map(|p| p.y),
            maximized: window.is_maximized(),
        }
        .save();
    }

    /// Keep the app window on the app. A link to anything that is not the
    /// loopback origin — docs, a GitHub issue, an OAuth consent page — belongs
    /// in the user's real browser, with their sessions and their extensions,
    /// not in a chromeless window with no address bar and no way back.
    fn external_links(origin: String) -> impl Fn(String) -> bool + 'static {
        move |url: String| {
            if is_internal(&url, &origin) {
                return true;
            }
            crate::gui::open_browser(&url);
            false
        }
    }

    /// `window.open` / `target="_blank"`: same rule, but there is no window to
    /// navigate — hand it to the browser and deny the popup.
    fn new_window(
        origin: String,
    ) -> impl Fn(String, wry::NewWindowFeatures) -> wry::NewWindowResponse + 'static {
        move |url: String, _features| {
            if !is_internal(&url, &origin) {
                crate::gui::open_browser(&url);
            }
            wry::NewWindowResponse::Deny
        }
    }

    /// Is `url` part of the app itself?
    ///
    /// `origin` is `http://127.0.0.1:<port>`, so the prefix test is exact on
    /// scheme, host and port; the following character must be a path boundary,
    /// or `http://127.0.0.1:8000.evil.test` would pass against port 8000. The
    /// webview's own internal URLs (`about:blank` on start-up) are internal
    /// too, and never navigations the user asked for.
    fn is_internal(url: &str, origin: &str) -> bool {
        if url == "about:blank" || url.starts_with("about:") {
            return true;
        }
        let Some(rest) = url.strip_prefix(origin) else {
            return false;
        };
        rest.is_empty() || rest.starts_with('/') || rest.starts_with('?') || rest.starts_with('#')
    }

    /// The window icon (Linux/X11 — macOS takes its icon from the bundle).
    /// Decoded from the same embedded PNG the installer writes.
    fn window_icon() -> Option<tao::window::Icon> {
        let image = image::load_from_memory(ICON_PNG).ok()?.into_rgba8();
        let (width, height) = image.dimensions();
        tao::window::Icon::from_rgba(image.into_raw(), width, height).ok()
    }

    /// Turn a webview construction failure into something a user can act on.
    /// The *common* Linux failure — no WebKitGTK at all — cannot reach this
    /// code: the library is linked at load time, so the process dies in the
    /// dynamic loader before `main`. That is why the requirement is documented
    /// in `docs/desktop.md` and checked by `install.sh`, and why this build is
    /// a separate binary from the plain one.
    fn webview_error(err: wry::Error) -> anyhow::Error {
        let hint = if cfg!(target_os = "linux") {
            "\nthe system webview (WebKitGTK) could not start. Install it:\n\
             \x20 Debian/Ubuntu: sudo apt install libwebkit2gtk-4.1-0\n\
             \x20 Fedora:        sudo dnf install webkit2gtk4.1\n\
             \x20 Arch:          sudo pacman -S webkit2gtk-4.1\n\
             \x20 NixOS:         it comes with the flake (nix run github:teddytennant/wizard)"
        } else {
            "\nthe system webview (WKWebView) could not start."
        };
        anyhow::anyhow!("{err}{hint}")
    }

    /// A window needs a display. Checked up front because GTK does not fail
    /// gracefully here — `EventLoop::build` aborts the process with
    /// "Failed to initialize gtk backend!", which tells a user over SSH
    /// nothing at all.
    fn preflight() -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            let display = std::env::var_os("WAYLAND_DISPLAY").is_some()
                || std::env::var_os("DISPLAY").is_some();
            anyhow::ensure!(
                display,
                "no display: $DISPLAY and $WAYLAND_DISPLAY are both unset, so there is no \
                 desktop to open a window on (are you on a headless box or over SSH?).\n\
                 Use `wizard gui` and forward the port, or `wizard` for the TUI."
            );
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
//
// The webview itself is not exercised here: it needs a display server and a
// system WebKit/WKWebView, neither of which exists on a CI runner, and a fake
// of it would test nothing. What *is* tested is everything the installer
// writes — the launcher files, their contents, and the idempotence of putting
// them in and taking them out — because that is where a broken install shows
// up, and it is all pure filesystem work that runs on any platform.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_entry_launches_the_running_binary_windowless() {
        let entry = desktop_entry(Path::new("/usr/local/bin/wizard-desktop"));
        assert!(entry.starts_with("[Desktop Entry]\n"));
        assert!(entry.contains("Exec=/usr/local/bin/wizard-desktop app\n"));
        // No terminal: this is the whole point of the launcher entry.
        assert!(entry.contains("Terminal=false\n"));
        assert!(entry.contains("Type=Application\n"));
        assert!(entry.contains("Icon=wizard\n"));
        // Exactly one *main* category: `desktop-file-validate` warns that two
        // can list the app twice in the menu.
        assert!(entry.contains("Categories=Development;\n"));
        // The dock groups the window with the entry by this class, and GDK
        // builds it from the binary's name with the first letter upper-cased —
        // so it tracks the Exec, capitalized. A live window on Hyprland run from
        // `target/release/wizard` reports `Wizard`, not `wizard`.
        assert!(entry.contains("StartupWMClass=Wizard-desktop\n"));
        assert!(
            desktop_entry(Path::new("/src/target/release/wizard"))
                .contains("StartupWMClass=Wizard\n")
        );
    }

    #[test]
    fn desktop_entry_quotes_a_path_with_spaces() {
        let entry = desktop_entry(Path::new("/home/a b/bin/wizard"));
        assert!(
            entry.contains(r#"Exec="/home/a b/bin/wizard" app"#),
            "{entry}"
        );
    }

    #[test]
    fn exec_quote_escapes_the_reserved_characters() {
        assert_eq!(exec_quote(Path::new("/usr/bin/wizard")), "/usr/bin/wizard");
        assert_eq!(exec_quote(Path::new(r#"/a"b/wizard"#)), r#""/a\"b/wizard""#);
        assert_eq!(exec_quote(Path::new("/a$b/wizard")), r#""/a\$b/wizard""#);
    }

    #[test]
    fn shell_quote_survives_a_quote_in_the_path() {
        assert_eq!(shell_quote(Path::new("/a'b/wizard")), r"'/a'\''b/wizard'");
    }

    #[test]
    fn info_plist_carries_what_launchservices_needs() {
        let plist = info_plist();
        assert!(plist.starts_with("<?xml version=\"1.0\""));
        assert!(
            plist.contains(
                "<key>CFBundleIdentifier</key>\n\t<string>com.teddytennant.wizard</string>"
            )
        );
        assert!(plist.contains("<key>CFBundleName</key>\n\t<string>Wizard</string>"));
        // Must match Contents/MacOS/<name>, or LaunchServices cannot start it.
        assert!(plist.contains("<key>CFBundleExecutable</key>\n\t<string>Wizard</string>"));
        assert!(plist.contains("<key>CFBundleIconFile</key>\n\t<string>wizard</string>"));
        assert!(plist.contains("<key>LSMinimumSystemVersion</key>"));
        assert!(plist.contains("<key>NSHighResolutionCapable</key>\n\t<true/>"));
        assert!(plist.contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn launcher_stub_execs_the_real_binary() {
        let stub = launcher_stub(Path::new("/usr/local/bin/wizard-desktop"));
        assert!(stub.starts_with("#!/bin/sh\n"));
        assert!(stub.contains("exec '/usr/local/bin/wizard-desktop' app \"$@\"\n"));
    }

    #[test]
    fn linux_install_writes_the_entry_and_both_icons() {
        let tmp = tempfile::tempdir().unwrap();
        let share = tmp.path().join(".local/share");
        let exe = tmp.path().join("bin/wizard");

        let written = install_linux(&share, &exe).unwrap();
        assert_eq!(written.len(), 3);
        let entry = share.join("applications/wizard.desktop");
        let svg = share.join("icons/hicolor/scalable/apps/wizard.svg");
        let png = share.join("icons/hicolor/512x512/apps/wizard.png");
        assert!(entry.is_file() && svg.is_file() && png.is_file());
        assert!(
            std::fs::read_to_string(&entry)
                .unwrap()
                .contains(&format!("Exec={} app", exe.display()))
        );
        // A real PNG, not a placeholder.
        assert_eq!(&std::fs::read(&png).unwrap()[..4], b"\x89PNG");
    }

    #[test]
    fn linux_install_and_uninstall_are_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let share = tmp.path().join(".local/share");
        let exe = tmp.path().join("bin/wizard");

        let first = install_linux(&share, &exe).unwrap();
        let second = install_linux(&share, &exe).unwrap();
        assert_eq!(first, second, "a second install must reproduce the tree");

        let removed = uninstall_linux(&share).unwrap();
        assert_eq!(removed.len(), 3);
        assert!(!share.join("applications/wizard.desktop").exists());
        // Uninstalling what is not there is not an error.
        assert!(uninstall_linux(&share).unwrap().is_empty());
    }

    #[test]
    fn macos_install_builds_the_bundle_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let apps = tmp.path().join("Applications");
        let exe = tmp.path().join("bin/wizard");

        install_macos(&apps, &exe).unwrap();
        let contents = apps.join("Wizard.app/Contents");
        assert!(contents.join("Info.plist").is_file());
        assert!(contents.join("MacOS/Wizard").is_file());
        assert!(contents.join("Resources").is_dir());
        let stub = std::fs::read_to_string(contents.join("MacOS/Wizard")).unwrap();
        assert!(stub.contains(&format!("exec '{}' app", exe.display())));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(contents.join("MacOS/Wizard"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "the trampoline must be executable");
        }
    }

    #[test]
    fn macos_install_and_uninstall_are_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let apps = tmp.path().join("Applications");
        let exe = tmp.path().join("bin/wizard");

        let first = install_macos(&apps, &exe).unwrap();
        let second = install_macos(&apps, &exe).unwrap();
        assert_eq!(first, second);

        let removed = uninstall_macos(&apps).unwrap();
        assert_eq!(removed, vec![apps.join("Wizard.app")]);
        assert!(!apps.join("Wizard.app").exists());
        assert!(uninstall_macos(&apps).unwrap().is_empty());
    }

    #[test]
    fn window_state_round_trips_and_rejects_nonsense() {
        let state = WindowState {
            width: 1200,
            height: 800,
            x: Some(40),
            y: Some(60),
            maximized: true,
        };
        let text = toml::to_string_pretty(&state).unwrap();
        assert_eq!(toml::from_str::<WindowState>(&text).unwrap(), state);

        // Partial files are fine: every field has a default.
        let partial: WindowState = toml::from_str("maximized = true").unwrap();
        assert_eq!(partial.width, WindowState::default().width);

        let absurd = WindowState {
            width: 0,
            height: 0,
            x: Some(-999_999),
            y: Some(0),
            maximized: false,
        }
        .sanitized();
        assert_eq!(absurd.width, WindowState::default().width);
        assert_eq!(absurd.height, WindowState::default().height);
        assert_eq!(absurd.x, None, "an off-canvas window is an unfindable one");
    }

    #[test]
    fn the_embedded_icons_are_the_wand_mark() {
        assert!(
            ICON_SVG.contains("wand") || ICON_SVG.contains("four-point spark"),
            "same mark as the favicon"
        );
        assert!(
            ICON_SVG.contains("#0c0c0e") && ICON_SVG.contains("#ececee"),
            "brand colours present"
        );
        assert_eq!(&ICON_PNG[..4], b"\x89PNG");
        // 512×512, big-endian in the IHDR.
        assert_eq!(&ICON_PNG[16..24], &[0, 0, 2, 0, 0, 0, 2, 0]);
    }
}
