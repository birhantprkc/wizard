//! `wizard desktop-setup`: install and enable the OS dependencies the
//! [`computer`](super) tool needs, with clear post-setup instructions.
//!
//! Linux installs `ydotool` (+ daemon), screenshot tools, and the AT-SPI /
//! portal stack via the detected package manager, drops a uinput udev rule,
//! adds the user to the `input` group, and enables the `ydotoold` user
//! service. NixOS and macOS can't be set up imperatively, so they get exact
//! instructions instead.

use std::process::Command;

use anyhow::Result;

/// Package families Wizard knows how to drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Distro {
    Debian,
    Fedora,
    Arch,
    Suse,
    NixOs,
    Unknown,
}

/// Entry point for the `desktop-setup` subcommand.
pub fn run() -> Result<()> {
    if cfg!(target_os = "macos") {
        print_macos_instructions();
        return Ok(());
    }
    if !cfg!(target_os = "linux") {
        println!(
            "Desktop control (computer use) is supported on Linux and macOS only.\n\
             This OS is not supported."
        );
        return Ok(());
    }

    let distro = detect_distro();
    println!("wizard desktop-setup — preparing this machine for computer use\n");
    println!("Detected distribution family: {distro:?}\n");

    if distro == Distro::NixOs {
        print_nixos_instructions();
        return Ok(());
    }

    let mut failures = Vec::new();

    // 1. Install packages.
    match install_command(distro) {
        Some((program, args)) => {
            if !run_step(
                "Installing desktop-control packages (ydotool, grim, slurp, maim, AT-SPI, portals)",
                program,
                &args,
            ) {
                failures.push("package installation");
            }
        }
        None => {
            println!(
                "Could not determine the package manager. Install these manually: \
                 ydotool at-spi2-core grim slurp maim xdg-desktop-portal \
                 xdg-desktop-portal-gtk\n"
            );
            failures.push("package installation (unknown package manager)");
        }
    }

    // 2. uinput udev rule so the input group can open /dev/uinput.
    if !install_udev_rule() {
        failures.push("uinput udev rule");
    }

    // 3. Add the user to the input group.
    if let Ok(user) = std::env::var("USER") {
        if !run_step(
            &format!("Adding {user} to the 'input' group"),
            "sudo",
            &["usermod".into(), "-aG".into(), "input".into(), user.clone()],
        ) {
            failures.push("input group membership");
        }
    } else {
        println!("Skipped input-group step: $USER is not set.\n");
    }

    // 4. Enable the ydotoold user service.
    if !run_step(
        "Enabling the ydotoold user service",
        "systemctl",
        &[
            "--user".into(),
            "enable".into(),
            "--now".into(),
            "ydotoold".into(),
        ],
    ) {
        println!(
            "  ydotoold could not be enabled as a user service. If your ydotool package ships \
             no unit, start the daemon manually (it must keep running):\n\
             \x20   nohup ydotoold >/dev/null 2>&1 &\n"
        );
    }

    println!("\n────────────────────────────────────────────────────────");
    if failures.is_empty() {
        println!("Setup complete.");
    } else {
        println!("Setup finished with issues in: {}.", failures.join(", "));
        println!("Re-run after resolving them, or perform those steps by hand.");
    }
    println!(
        "\nIMPORTANT: group membership only takes effect after you log out and back in \
         (or reboot). Until then, /dev/uinput access will be denied and input actions will \
         fail.\n\nThen verify with:  wizard -p \"take a screenshot of my desktop\""
    );
    Ok(())
}

/// Run one setup step, echoing the command first. Returns whether it
/// succeeded.
fn run_step(description: &str, program: &str, args: &[String]) -> bool {
    println!("→ {description}");
    println!("  $ {program} {}", args.join(" "));
    match Command::new(program).args(args).status() {
        Ok(status) if status.success() => {
            println!("  ok\n");
            true
        }
        Ok(status) => {
            println!("  failed (exit {})\n", status.code().unwrap_or(-1));
            false
        }
        Err(err) => {
            println!("  could not run: {err}\n");
            false
        }
    }
}

/// Read `/etc/os-release` and classify the distribution family.
fn detect_distro() -> Distro {
    let release = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    classify_os_release(&release)
}

/// Classify an `os-release` file body by its `ID` / `ID_LIKE` fields.
fn classify_os_release(release: &str) -> Distro {
    let field = |key: &str| -> String {
        release
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .map(|rest| {
                rest.trim_start_matches('=')
                    .trim_matches('"')
                    .to_ascii_lowercase()
            })
            .unwrap_or_default()
    };
    let id = field("ID=");
    let id_like = field("ID_LIKE=");
    let haystack = format!("{id} {id_like}");

    if id == "nixos" {
        return Distro::NixOs;
    }
    if haystack.contains("debian") || haystack.contains("ubuntu") {
        return Distro::Debian;
    }
    if haystack.contains("fedora") || haystack.contains("rhel") || haystack.contains("centos") {
        return Distro::Fedora;
    }
    if haystack.contains("arch") {
        return Distro::Arch;
    }
    if haystack.contains("suse") {
        return Distro::Suse;
    }
    Distro::Unknown
}

/// The package-install command for a known distro family.
fn install_command(distro: Distro) -> Option<(&'static str, Vec<String>)> {
    let s = |v: &str| v.to_string();
    let pkgs_portal = ["xdg-desktop-portal", "xdg-desktop-portal-gtk"];
    // `grim` covers Wayland and `maim` covers X11; installing only the former
    // left every X11 session with a `computer` tool that could not screenshot,
    // because the X11 chain in `linux.rs` tries `maim` and then ImageMagick's
    // `import` and finds neither. `maim` is in the default repos of all four
    // families here (Debian/Ubuntu, Fedora/EPEL, Arch, openSUSE Leap and
    // Tumbleweed), so one name works everywhere and no ImageMagick pull-in is
    // needed.
    let base = ["ydotool", "at-spi2-core", "grim", "slurp", "maim"];
    let all = || base.iter().chain(pkgs_portal.iter()).map(|p| s(p));

    match distro {
        Distro::Debian => Some((
            "sudo",
            std::iter::once(s("apt"))
                .chain([s("install"), s("-y")])
                .chain(all())
                .collect(),
        )),
        Distro::Fedora => Some((
            "sudo",
            std::iter::once(s("dnf"))
                .chain([s("install"), s("-y")])
                .chain(all())
                .collect(),
        )),
        Distro::Arch => Some((
            "sudo",
            std::iter::once(s("pacman"))
                .chain([s("-S"), s("--needed"), s("--noconfirm")])
                .chain(all())
                .collect(),
        )),
        Distro::Suse => Some((
            "sudo",
            std::iter::once(s("zypper"))
                .chain([s("install"), s("-y")])
                // openSUSE has no separate portal-gtk metapackage name here.
                .chain(
                    base.iter()
                        .chain(["xdg-desktop-portal"].iter())
                        .map(|p| s(p)),
                )
                .collect(),
        )),
        Distro::NixOs | Distro::Unknown => None,
    }
}

/// Install the uinput udev rule if it is not already present, then reload
/// rules. Returns whether the rule is in place afterward.
fn install_udev_rule() -> bool {
    const PATH: &str = "/etc/udev/rules.d/80-uinput.rules";
    const RULE: &str = "KERNEL==\"uinput\", MODE=\"0660\", GROUP=\"input\"";
    if std::path::Path::new(PATH).exists() {
        println!("→ uinput udev rule already present at {PATH}\n");
        return true;
    }
    println!("→ Installing uinput udev rule at {PATH}");
    // `sudo tee` so the redirect runs with privilege.
    let tee = Command::new("sudo")
        .args(["tee", PATH])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn();
    let mut child = match tee {
        Ok(child) => child,
        Err(err) => {
            println!("  could not run sudo tee: {err}\n");
            return false;
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        let _ = writeln!(stdin, "{RULE}");
    }
    let wrote = matches!(child.wait(), Ok(status) if status.success());
    if !wrote {
        println!("  failed to write the rule\n");
        return false;
    }
    let _ = Command::new("sudo")
        .args(["udevadm", "control", "--reload-rules"])
        .status();
    let _ = Command::new("sudo").args(["udevadm", "trigger"]).status();
    println!("  ok\n");
    true
}

/// NixOS can't be configured imperatively; print the declarative snippet plus
/// an optional imperative stopgap.
fn print_nixos_instructions() {
    println!(
        "NixOS is configured declaratively. Add the following to your system flake/config and \
         rebuild:\n\n\
         \x20 # configuration.nix / a module\n\
         \x20 programs.ydotool.enable = true;            # ydotool + ydotoold + uinput rule\n\
         \x20 environment.systemPackages = with pkgs; [\n\
         \x20   grim slurp maim ydotool                  # capture (Wayland + X11) + input\n\
         \x20   at-spi2-core xdg-desktop-portal xdg-desktop-portal-gtk\n\
         \x20 ];\n\
         \x20 users.users.<you>.extraGroups = [ \"input\" \"uinput\" ];\n\n\
         Then:  sudo nixos-rebuild switch   (and log out/in for the group change)\n\n\
         Stopgap without a rebuild (this session only):\n\
         \x20 nix profile install nixpkgs#ydotool nixpkgs#grim nixpkgs#slurp nixpkgs#maim\n\
         \x20 systemctl --user start ydotoold   # or: nohup ydotoold &\n"
    );
    println!("\n{}", nixos_missing_summary(&which_missing()));
}

/// Every binary this backend shells out to, in the order the summary names
/// them. `grim` serves Wayland and `maim` serves X11; a machine that runs both
/// session types over its life wants both.
const REQUIRED_BINARIES: [&str; 4] = ["ydotool", "grim", "slurp", "maim"];

/// Which of [`REQUIRED_BINARIES`] are not on `PATH`.
///
/// Looked up rather than assumed. This sentence used to be the constant "your
/// machine already has grim and slurp; you mainly need ydotool", which was true
/// of exactly one machine and told everyone else they were nearly done when
/// they had none of it installed.
fn which_missing() -> Vec<&'static str> {
    REQUIRED_BINARIES
        .into_iter()
        .filter(|bin| which(bin).is_none())
        .collect()
}

/// The closing line of the NixOS instructions: what is actually still missing.
///
/// Split out from the printing so it can be tested without a PATH fixture.
fn nixos_missing_summary(missing: &[&str]) -> String {
    match missing {
        [] => format!(
            "All four binaries ({}) are already on PATH; what is left is ydotoold running and \
             membership in the 'input'/'uinput' groups.",
            REQUIRED_BINARIES.join(", ")
        ),
        _ => format!(
            "Not on PATH yet: {}. You also need ydotoold running and membership in the \
             'input'/'uinput' groups.",
            missing.join(", ")
        ),
    }
}

/// Look `bin` up on `PATH`.
///
/// A local copy of the same helper the Linux backend uses: `setup` runs as its
/// own subcommand on every OS, and `linux.rs` is compiled only on Linux.
fn which(bin: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(bin))
        .find(|candidate| candidate.is_file())
}

/// macOS setup is permission-granting, not package installation.
fn print_macos_instructions() {
    println!(
        "macOS computer use uses the built-in CoreGraphics (Accessibility) API and \
         `screencapture` — nothing to install. Grant two permissions to the terminal app (or \
         the app bundle) you run Wizard from:\n\n\
         \x20 System Settings → Privacy & Security → Accessibility\n\
         \x20   → enable your terminal (Terminal, iTerm, Ghostty, VS Code, …)\n\n\
         \x20 System Settings → Privacy & Security → Screen Recording\n\
         \x20   → enable the same terminal\n\n\
         You must fully quit and reopen the terminal after granting each permission. Without \
         Accessibility, mouse/keyboard events are silently dropped; without Screen Recording, \
         screenshots come back blank.\n\n\
         Then verify with:  wizard -p \"take a screenshot of my desktop\"\n"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The NixOS closing line reports what is missing rather than asserting it.
    ///
    /// The regression it guards: the line was a constant claiming grim and
    /// slurp were already installed, so a machine with none of the three was
    /// told it "mainly needs ydotool" and would then fail every capture with a
    /// tool-not-found error the instructions had promised was not the problem.
    #[test]
    fn the_nixos_summary_names_what_is_actually_missing() {
        let all = nixos_missing_summary(&REQUIRED_BINARIES);
        for bin in REQUIRED_BINARIES {
            assert!(all.contains(bin), "{bin} must be named: {all}");
        }
        assert!(
            !all.contains("already"),
            "must not claim anything is installed: {all}"
        );

        let some = nixos_missing_summary(&["ydotool"]);
        assert!(some.contains("ydotool"), "{some}");
        assert!(
            !some.contains("grim"),
            "what is present is not listed: {some}"
        );

        let none = nixos_missing_summary(&[]);
        assert!(none.contains("already on PATH"), "{none}");
        assert!(
            none.contains("ydotoold"),
            "the daemon is still required: {none}"
        );
    }

    #[test]
    fn classifies_common_distros() {
        assert_eq!(
            classify_os_release("ID=ubuntu\nID_LIKE=debian\n"),
            Distro::Debian
        );
        assert_eq!(classify_os_release("ID=debian\n"), Distro::Debian);
        assert_eq!(
            classify_os_release("ID=pop\nID_LIKE=\"ubuntu debian\"\n"),
            Distro::Debian
        );
        assert_eq!(
            classify_os_release("ID=fedora\nID_LIKE=\"\"\n"),
            Distro::Fedora
        );
        assert_eq!(
            classify_os_release("ID=rocky\nID_LIKE=\"rhel centos fedora\"\n"),
            Distro::Fedora
        );
        assert_eq!(
            classify_os_release("ID=cachyos\nID_LIKE=arch\n"),
            Distro::Arch
        );
        assert_eq!(
            classify_os_release("ID=opensuse-tumbleweed\nID_LIKE=\"suse opensuse\"\n"),
            Distro::Suse
        );
        assert_eq!(classify_os_release("ID=nixos\n"), Distro::NixOs);
        assert_eq!(classify_os_release("ID=plan9\n"), Distro::Unknown);
    }

    #[test]
    fn install_command_covers_known_families_and_skips_others() {
        for distro in [Distro::Debian, Distro::Fedora, Distro::Arch, Distro::Suse] {
            let (program, args) = install_command(distro).expect("known distro has a command");
            assert_eq!(program, "sudo");
            assert!(
                args.iter().any(|a| a == "ydotool"),
                "{distro:?} installs ydotool"
            );
            assert!(args.iter().any(|a| a == "grim"), "{distro:?} installs grim");
            // The X11 half of the capture chain. Without it `wizard
            // desktop-setup` reported success and left an X11 session with a
            // `computer` tool that could not take a screenshot at all.
            assert!(args.iter().any(|a| a == "maim"), "{distro:?} installs maim");
        }
        assert!(install_command(Distro::NixOs).is_none());
        assert!(install_command(Distro::Unknown).is_none());
    }
}
