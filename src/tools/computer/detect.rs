//! What kind of desktop this is, and whether Wizard can drive it without
//! writing anything new.
//!
//! [`probe`] reads the real environment into an [`Environment`], and
//! [`decide`] turns one into a [`Decision`]. `decide` is a pure function so
//! the table can be tested over made-up machines: most of the rows below
//! (GNOME and KDE on Wayland, Windows, WSL, macOS) are not the machine this
//! code was written on.
//!
//! The table, in short:
//!
//! | system                                | host path                                  |
//! | ------------------------------------- | ------------------------------------------ |
//! | Linux, wlroots Wayland (Hyprland, sway, river, wayfire), or X11 | built in: ydotool + grim or maim |
//! | Linux, GNOME or KDE on Wayland        | generate: grim cannot capture there        |
//! | Linux, other Wayland compositor       | generate: capture support unknown          |
//! | Linux, no display server              | none: use the VM                           |
//! | macOS                                 | built in: CoreGraphics + screencapture (not verified here) |
//! | Windows                               | generate: PowerShell driver (not verified) |
//! | WSL                                   | generate: drive Windows through powershell.exe (not verified) |
//!
//! The VM backend is offered wherever Docker or Podman is on `PATH`.

use std::collections::BTreeSet;

/// The operating system family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Linux,
    MacOs,
    Windows,
    Other,
}

/// Which display server a Linux session runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Session {
    Wayland,
    X11,
    None,
}

/// The Wayland compositor or desktop, as far as the environment says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Desktop {
    Hyprland,
    Sway,
    /// Another wlroots compositor (river, wayfire, labwc, niri is not one).
    Wlroots(String),
    Gnome,
    Kde,
    Other(String),
    Unknown,
}

impl Desktop {
    /// Whether grim's wlr-screencopy protocol is available.
    fn has_wlr_screencopy(&self) -> bool {
        matches!(
            self,
            Desktop::Hyprland | Desktop::Sway | Desktop::Wlroots(_)
        )
    }

    fn label(&self) -> String {
        match self {
            Desktop::Hyprland => "Hyprland".into(),
            Desktop::Sway => "sway".into(),
            Desktop::Wlroots(name) | Desktop::Other(name) => name.clone(),
            Desktop::Gnome => "GNOME".into(),
            Desktop::Kde => "KDE Plasma".into(),
            Desktop::Unknown => "unknown desktop".into(),
        }
    }
}

/// Everything the decision depends on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Environment {
    pub os: Os,
    /// Linux running under Windows (WSL1 or WSL2).
    pub wsl: bool,
    pub session: Session,
    pub desktop: Desktop,
    pub nixos: bool,
    /// Which of the binaries Wizard cares about are on `PATH`.
    pub binaries: BTreeSet<String>,
}

impl Environment {
    fn has(&self, bin: &str) -> bool {
        self.binaries.contains(bin)
    }

    /// One line naming the system, e.g. `Linux, Wayland (Hyprland), NixOS`.
    pub fn summary(&self) -> String {
        match self.os {
            Os::MacOs => "macOS".into(),
            Os::Windows => "Windows".into(),
            Os::Other => std::env::consts::OS.into(),
            Os::Linux => {
                let mut out = String::from(if self.wsl { "Linux under WSL" } else { "Linux" });
                match self.session {
                    Session::Wayland => {
                        out.push_str(&format!(", Wayland ({})", self.desktop.label()));
                    }
                    Session::X11 => {
                        out.push_str(", X11");
                        if self.desktop != Desktop::Unknown {
                            out.push_str(&format!(" ({})", self.desktop.label()));
                        }
                    }
                    Session::None => out.push_str(", no display server"),
                }
                if self.nixos {
                    out.push_str(", NixOS");
                }
                out
            }
        }
    }
}

/// How the host backend can drive this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostPath {
    /// The built-in driver covers this system and its tools are installed.
    Ready { how: String },
    /// The built-in driver covers this system; these tools are missing.
    /// `wizard desktop-setup` installs them (or prints the NixOS config).
    Install { how: String, missing: Vec<String> },
    /// The built-in driver does not cover this system. The agent can write a
    /// driver; `plan` says what it would use.
    Generate { why: String, plan: Vec<String> },
    /// Nothing on the host to drive.
    Unavailable { why: String },
}

/// Whether the VM backend can run here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VmPath {
    Available { engine: String },
    Unavailable { why: String },
}

/// What `wizard computer setup` offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub host: HostPath,
    pub vm: VmPath,
    /// This row of the table was checked on real hardware. False for the
    /// macOS, Windows and WSL rows, which are reasoned, not run.
    pub verified: bool,
}

impl Decision {
    /// The backend to suggest first: the VM when one can run, since it keeps
    /// the agent off the user's own desktop.
    pub fn recommends_vm(&self) -> bool {
        matches!(self.vm, VmPath::Available { .. })
    }
}

/// Binaries [`probe`] looks for.
pub const WATCHED_BINARIES: &[&str] = &[
    "ydotool",
    "ydotoold",
    "grim",
    "maim",
    "import",
    "xdotool",
    "spectacle",
    "gnome-screenshot",
    "gdbus",
    "powershell.exe",
    "docker",
    "podman",
];

/// Read the running system.
pub fn probe() -> Environment {
    let os = if cfg!(target_os = "linux") {
        Os::Linux
    } else if cfg!(target_os = "macos") {
        Os::MacOs
    } else if cfg!(target_os = "windows") {
        Os::Windows
    } else {
        Os::Other
    };
    let var = |key: &str| std::env::var(key).unwrap_or_default();
    let osrelease = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    let wsl = os == Os::Linux
        && (std::env::var_os("WSL_DISTRO_NAME").is_some()
            || osrelease.to_ascii_lowercase().contains("microsoft"));
    let session = classify_session(
        &var("XDG_SESSION_TYPE"),
        std::env::var_os("WAYLAND_DISPLAY").is_some(),
        std::env::var_os("DISPLAY").is_some(),
    );
    let desktop = classify_desktop(
        &var("XDG_CURRENT_DESKTOP"),
        std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some(),
        std::env::var_os("SWAYSOCK").is_some(),
    );
    let nixos = std::fs::read_to_string("/etc/os-release")
        .map(|release| release.lines().any(|line| line.trim() == "ID=nixos"))
        .unwrap_or(false);
    let binaries = WATCHED_BINARIES
        .iter()
        .filter(|bin| super::which(bin).is_some())
        .map(|bin| bin.to_string())
        .collect();
    Environment {
        os,
        wsl,
        session,
        desktop,
        nixos,
        binaries,
    }
}

/// Classify the session from `XDG_SESSION_TYPE` and which display sockets
/// are set. `WAYLAND_DISPLAY` wins over `DISPLAY` because XWayland sets both.
pub fn classify_session(session_type: &str, wayland: bool, x11: bool) -> Session {
    match session_type.to_ascii_lowercase().as_str() {
        "wayland" if wayland => return Session::Wayland,
        "x11" if x11 => return Session::X11,
        _ => {}
    }
    if wayland {
        Session::Wayland
    } else if x11 {
        Session::X11
    } else {
        Session::None
    }
}

/// Classify the desktop from `XDG_CURRENT_DESKTOP` (a colon list such as
/// `ubuntu:GNOME`) plus the compositor sockets that are more reliable than it.
pub fn classify_desktop(current: &str, hyprland: bool, sway: bool) -> Desktop {
    if hyprland {
        return Desktop::Hyprland;
    }
    if sway {
        return Desktop::Sway;
    }
    let parts: Vec<String> = current
        .split(':')
        .map(|part| part.trim().to_ascii_lowercase())
        .filter(|part| !part.is_empty())
        .collect();
    for part in &parts {
        match part.as_str() {
            "hyprland" => return Desktop::Hyprland,
            "sway" => return Desktop::Sway,
            "river" | "wayfire" | "labwc" | "hikari" | "dwl" | "qtile" => {
                return Desktop::Wlroots(part.clone());
            }
            "gnome" | "gnome-classic" | "unity" | "pop" => return Desktop::Gnome,
            "kde" | "plasma" => return Desktop::Kde,
            _ => {}
        }
    }
    match parts.first() {
        Some(first) => Desktop::Other(first.clone()),
        None => Desktop::Unknown,
    }
}

/// The decision table. Pure: everything it reads is in `env`.
pub fn decide(env: &Environment) -> Decision {
    let vm = if env.has("docker") {
        VmPath::Available {
            engine: "docker".into(),
        }
    } else if env.has("podman") {
        VmPath::Available {
            engine: "podman".into(),
        }
    } else {
        VmPath::Unavailable {
            why: "neither docker nor podman is on PATH".into(),
        }
    };

    let (host, verified) = match env.os {
        Os::MacOs => (
            HostPath::Ready {
                how: "CoreGraphics input and screencapture, after granting Accessibility and \
                      Screen Recording to your terminal"
                    .into(),
            },
            false,
        ),
        Os::Windows => (
            HostPath::Generate {
                why: "Wizard has no built-in Windows driver".into(),
                plan: vec![
                    "capture: PowerShell with System.Drawing (Graphics.CopyFromScreen) saving a PNG"
                        .into(),
                    "input: PowerShell calling user32 SetCursorPos, mouse_event and SendInput \
                     through Add-Type"
                        .into(),
                    "wrap both in ~/.wizard/tools/computer_driver.lua, which shells out to \
                     powershell -NoProfile -Command"
                        .into(),
                ],
            },
            false,
        ),
        Os::Other => (
            HostPath::Unavailable {
                why: format!("no desktop driver for {}", std::env::consts::OS),
            },
            false,
        ),
        Os::Linux if env.wsl => (
            HostPath::Generate {
                why: "under WSL the desktop is Windows, which Linux tools cannot reach".into(),
                plan: vec![
                    "capture and input: the Windows PowerShell driver, run from Linux through \
                     powershell.exe interop"
                        .into(),
                    "screenshots are written to a Windows temp path and read back through \
                     /mnt/c"
                        .into(),
                ],
            },
            false,
        ),
        Os::Linux => (linux_host(env), true),
    };

    Decision { host, vm, verified }
}

fn linux_host(env: &Environment) -> HostPath {
    let input_missing = !env.has("ydotool");
    match env.session {
        Session::None => HostPath::Unavailable {
            why: "no Wayland or X11 session in this environment (WAYLAND_DISPLAY and DISPLAY \
                  are unset)"
                .into(),
        },
        Session::X11 => {
            let mut missing = Vec::new();
            if input_missing {
                missing.push("ydotool".to_string());
            }
            if !env.has("maim") && !env.has("import") {
                missing.push("maim".to_string());
            }
            built_in("ydotool input (uinput) and maim capture".into(), missing)
        }
        Session::Wayland if env.desktop.has_wlr_screencopy() => {
            let mut missing = Vec::new();
            if input_missing {
                missing.push("ydotool".to_string());
            }
            if !env.has("grim") {
                missing.push("grim".to_string());
            }
            built_in(
                format!(
                    "ydotool input (uinput) and grim capture over wlr-screencopy on {}",
                    env.desktop.label()
                ),
                missing,
            )
        }
        Session::Wayland => {
            let input = if env.has("ydotool") {
                "input: ydotool, already installed (uinput works under every compositor)"
                    .to_string()
            } else {
                "input: ydotool through uinput, which works under every compositor (`wizard \
                 desktop-setup` installs it)"
                    .to_string()
            };
            let (why, capture) = match &env.desktop {
                Desktop::Gnome => (
                    "GNOME's compositor does not offer wlr-screencopy, so grim cannot capture"
                        .to_string(),
                    if env.has("gnome-screenshot") {
                        "capture: gnome-screenshot -f <file>, falling back to the \
                         org.gnome.Shell.Screenshot D-Bus call through gdbus"
                            .to_string()
                    } else {
                        "capture: the org.gnome.Shell.Screenshot D-Bus call through gdbus, or \
                         the xdg-desktop-portal Screenshot portal (asks once for permission)"
                            .to_string()
                    },
                ),
                Desktop::Kde => (
                    "KWin does not offer wlr-screencopy, so grim cannot capture".to_string(),
                    "capture: spectacle -b -n -f -o <file> (background, no notification, full \
                     screen)"
                        .to_string(),
                ),
                other => (
                    format!(
                        "{} is not a compositor Wizard knows supports wlr-screencopy",
                        other.label()
                    ),
                    "capture: try grim first; if it fails, the xdg-desktop-portal Screenshot \
                     portal through gdbus"
                        .to_string(),
                ),
            };
            HostPath::Generate {
                why,
                plan: vec![
                    capture,
                    input,
                    "wrap both in ~/.wizard/tools/computer_driver.lua (LuaJIT, os.execute) \
                     behind the driver contract"
                        .to_string(),
                ],
            }
        }
    }
}

fn built_in(how: String, missing: Vec<String>) -> HostPath {
    if missing.is_empty() {
        HostPath::Ready { how }
    } else {
        HostPath::Install { how, missing }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(os: Os, session: Session, desktop: Desktop, bins: &[&str]) -> Environment {
        Environment {
            os,
            wsl: false,
            session,
            desktop,
            nixos: false,
            binaries: bins.iter().map(|b| b.to_string()).collect(),
        }
    }

    #[test]
    fn wlroots_wayland_with_tools_is_ready() {
        for desktop in [
            Desktop::Hyprland,
            Desktop::Sway,
            Desktop::Wlroots("river".into()),
        ] {
            let e = env(Os::Linux, Session::Wayland, desktop, &["ydotool", "grim"]);
            let d = decide(&e);
            assert!(matches!(d.host, HostPath::Ready { .. }), "{:?}", d.host);
            assert!(d.verified);
        }
    }

    #[test]
    fn wlroots_wayland_without_tools_needs_install() {
        let e = env(Os::Linux, Session::Wayland, Desktop::Hyprland, &[]);
        match decide(&e).host {
            HostPath::Install { missing, .. } => assert_eq!(missing, ["ydotool", "grim"]),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn x11_accepts_maim_or_import() {
        let e = env(
            Os::Linux,
            Session::X11,
            Desktop::Unknown,
            &["ydotool", "import"],
        );
        assert!(matches!(decide(&e).host, HostPath::Ready { .. }));
        let e = env(Os::Linux, Session::X11, Desktop::Gnome, &["ydotool"]);
        match decide(&e).host {
            HostPath::Install { missing, .. } => assert_eq!(missing, ["maim"]),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn gnome_and_kde_wayland_need_a_generated_driver() {
        let e = env(
            Os::Linux,
            Session::Wayland,
            Desktop::Gnome,
            &["ydotool", "grim", "gnome-screenshot"],
        );
        match decide(&e).host {
            HostPath::Generate { why, plan } => {
                assert!(why.contains("GNOME"), "{why}");
                assert!(plan[0].contains("gnome-screenshot"), "{plan:?}");
                assert!(plan[1].contains("already installed"), "{plan:?}");
            }
            other => panic!("{other:?}"),
        }
        let e = env(Os::Linux, Session::Wayland, Desktop::Kde, &[]);
        match decide(&e).host {
            HostPath::Generate { plan, .. } => assert!(plan[0].contains("spectacle")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unknown_wayland_compositor_generates_with_a_probe_plan() {
        let e = env(
            Os::Linux,
            Session::Wayland,
            Desktop::Other("niri".into()),
            &["ydotool", "grim"],
        );
        match decide(&e).host {
            HostPath::Generate { why, plan } => {
                assert!(why.contains("niri"));
                assert!(plan[0].contains("try grim first"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn headless_linux_has_no_host_path_but_can_use_a_vm() {
        let e = env(Os::Linux, Session::None, Desktop::Unknown, &["docker"]);
        let d = decide(&e);
        assert!(matches!(d.host, HostPath::Unavailable { .. }));
        assert_eq!(
            d.vm,
            VmPath::Available {
                engine: "docker".into()
            }
        );
        assert!(d.recommends_vm());
    }

    #[test]
    fn podman_is_used_when_docker_is_absent() {
        let e = env(Os::Linux, Session::None, Desktop::Unknown, &["podman"]);
        assert_eq!(
            decide(&e).vm,
            VmPath::Available {
                engine: "podman".into()
            }
        );
        let e = env(Os::Linux, Session::None, Desktop::Unknown, &[]);
        assert!(matches!(decide(&e).vm, VmPath::Unavailable { .. }));
        assert!(!decide(&e).recommends_vm());
    }

    #[test]
    fn unverified_platforms_say_so() {
        let mac = decide(&env(Os::MacOs, Session::None, Desktop::Unknown, &[]));
        assert!(matches!(mac.host, HostPath::Ready { .. }));
        assert!(!mac.verified);

        let win = decide(&env(Os::Windows, Session::None, Desktop::Unknown, &[]));
        assert!(matches!(win.host, HostPath::Generate { .. }));
        assert!(!win.verified);

        let mut wsl = env(
            Os::Linux,
            Session::X11,
            Desktop::Unknown,
            &["ydotool", "maim"],
        );
        wsl.wsl = true;
        let d = decide(&wsl);
        match d.host {
            HostPath::Generate { plan, .. } => assert!(plan[0].contains("powershell.exe")),
            other => panic!("WSL must not claim the Linux X11 path: {other:?}"),
        }
        assert!(!d.verified);
    }

    #[test]
    fn session_prefers_wayland_when_both_sockets_exist() {
        assert_eq!(classify_session("", true, true), Session::Wayland);
        assert_eq!(classify_session("x11", true, true), Session::X11);
        assert_eq!(classify_session("wayland", false, true), Session::X11);
        assert_eq!(classify_session("tty", false, false), Session::None);
    }

    #[test]
    fn desktop_reads_sockets_then_xdg_current_desktop() {
        assert_eq!(classify_desktop("GNOME", true, false), Desktop::Hyprland);
        assert_eq!(classify_desktop("", false, true), Desktop::Sway);
        assert_eq!(
            classify_desktop("ubuntu:GNOME", false, false),
            Desktop::Gnome
        );
        assert_eq!(classify_desktop("KDE", false, false), Desktop::Kde);
        assert_eq!(
            classify_desktop("river", false, false),
            Desktop::Wlroots("river".into())
        );
        assert_eq!(
            classify_desktop("niri", false, false),
            Desktop::Other("niri".into())
        );
        assert_eq!(classify_desktop("", false, false), Desktop::Unknown);
    }

    #[test]
    fn summary_names_the_session() {
        let mut e = env(Os::Linux, Session::Wayland, Desktop::Hyprland, &[]);
        e.nixos = true;
        assert_eq!(e.summary(), "Linux, Wayland (Hyprland), NixOS");
        e.wsl = true;
        e.session = Session::None;
        e.nixos = false;
        assert_eq!(e.summary(), "Linux under WSL, no display server");
    }
}
