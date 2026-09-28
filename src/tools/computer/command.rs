//! `wizard computer`: status, setup, check, the VM, and turning it off.
//!
//! Setup is the only path that writes `[computer] enabled = true`, and it
//! writes it only after [`verify`] has taken a screenshot and moved the
//! pointer through the chosen backend. Nothing here drives the screen beyond
//! that one-pixel nudge.

use std::io::{BufRead, IsTerminal, Write};

use anyhow::{Context, Result, bail};

use super::detect::{self, Decision, HostPath, VmPath};
use super::{Backend, driver, vm};
use crate::cli::{ComputerBackendArg, ComputerCmd, ComputerDriverArg, ComputerVmCmd};
use crate::config::{ComputerBackend, ComputerConfig, Config, HostDriver};

/// Entry point for `wizard computer`. Returns the process exit code.
pub fn run(cmd: ComputerCmd) -> Result<i32> {
    match cmd {
        ComputerCmd::Status => {
            let config = Config::load()?;
            println!("{}", status_report(&config.computer));
            Ok(0)
        }
        ComputerCmd::Setup { backend, yes } => setup(backend, yes),
        ComputerCmd::Check {
            backend,
            driver,
            save,
        } => {
            let mut computer = Config::load()?.computer;
            if let Some(backend) = backend {
                computer.backend = backend_of(backend);
            }
            if let Some(driver) = driver {
                computer.backend = ComputerBackend::Host;
                computer.driver = match driver {
                    ComputerDriverArg::Native => HostDriver::Native,
                    ComputerDriverArg::Scripted => HostDriver::Scripted,
                };
            }
            let backend = super::backend_for(&computer);
            println!("Checking {}", backend.label());
            match verify(&*backend, save.as_deref()) {
                Ok(report) => {
                    println!("ok: {report}");
                    Ok(0)
                }
                Err(err) => {
                    println!("failed: {err:#}");
                    Ok(1)
                }
            }
        }
        ComputerCmd::Vm { cmd } => {
            let config = Config::load()?;
            let vm_config = &config.computer.vm;
            match cmd {
                ComputerVmCmd::Up { rebuild } => {
                    let status = vm::up(vm_config, rebuild)?;
                    println!("VM {}", status.describe());
                    if !(config.computer.enabled && config.computer.backend == ComputerBackend::Vm)
                    {
                        println!(
                            "The computer tool is not using it yet: run `wizard computer setup \
                             --backend vm` to check it and turn it on."
                        );
                    }
                }
                ComputerVmCmd::Down => {
                    vm::down(vm_config)?;
                    println!("VM stopped.");
                }
                ComputerVmCmd::Status => println!("VM {}", vm::status(vm_config)?.describe()),
            }
            Ok(0)
        }
        ComputerCmd::Disable => {
            let mut config = Config::load()?;
            config.computer.enabled = false;
            config.save()?;
            println!("Computer use is off. The `computer` tool is gone after /reload.");
            Ok(0)
        }
    }
}

fn backend_of(arg: ComputerBackendArg) -> ComputerBackend {
    match arg {
        ComputerBackendArg::Host => ComputerBackend::Host,
        ComputerBackendArg::Vm => ComputerBackend::Vm,
    }
}

/// Take a screenshot and move the pointer one pixel and back. `save` also
/// writes the screenshot to a file.
pub(crate) fn verify(backend: &dyn Backend, save: Option<&std::path::Path>) -> Result<String> {
    let shot = backend.screenshot().context("screenshot")?;
    let (w, h) = (shot.width as i32, shot.height as i32);
    if w == 0 || h == 0 {
        bail!("the screenshot is {w}x{h}");
    }
    if let Some(path) = save {
        std::fs::write(path, &shot.png).with_context(|| format!("writing {}", path.display()))?;
    }
    let start = backend.cursor_position().ok();
    let (x, y) = start.unwrap_or((w / 2, h / 2));
    let nudge = if x + 1 < w { x + 1 } else { x - 1 };
    backend.mouse_move(nudge, y).context("moving the pointer")?;
    backend
        .mouse_move(x, y)
        .context("moving the pointer back")?;
    Ok(format!(
        "screenshot {w}x{h}; pointer moved to ({nudge}, {y}) and back to ({x}, {y})"
    ))
}

/// Everything `/computer` and `wizard computer status` show.
pub fn status_report(computer: &ComputerConfig) -> String {
    let env = detect::probe();
    let decision = detect::decide(&env);
    let mut out = String::new();
    let configured = if computer.enabled {
        match (computer.backend, computer.driver) {
            (ComputerBackend::Vm, _) => format!("on, VM at {}", computer.vm.vnc_address()),
            (ComputerBackend::Host, HostDriver::Native) => {
                "on, this desktop (built-in driver)".into()
            }
            (ComputerBackend::Host, HostDriver::Scripted) => {
                "on, this desktop (generated driver)".into()
            }
        }
    } else {
        "off (the default)".into()
    };
    out.push_str(&format!("computer use: {configured}\n"));
    out.push_str(&format!("system: {}\n", env.summary()));
    out.push_str(&format!("host: {}\n", describe_host(&decision.host)));
    match &decision.vm {
        VmPath::Available { engine } => {
            let vm_status = if engine == &computer.vm.engine || computer.vm.address.is_some() {
                vm::status(&computer.vm)
                    .map(|s| s.describe())
                    .unwrap_or_else(|err| format!("unknown ({err:#})"))
            } else {
                format!("{engine} found; set [computer.vm] engine = \"{engine}\"")
            };
            out.push_str(&format!("vm: available through {engine}; {vm_status}\n"));
        }
        VmPath::Unavailable { why } => out.push_str(&format!("vm: unavailable, {why}\n")),
    }
    if !decision.verified {
        out.push_str(
            "note: this platform's row of the table has not been tested on a real machine\n",
        );
    }
    if !computer.enabled {
        out.push_str("next: run `wizard computer setup` in a terminal, or ask me to set it up");
    }
    out.trim_end().to_string()
}

fn describe_host(host: &HostPath) -> String {
    match host {
        HostPath::Ready { how } => format!("ready, {how}"),
        HostPath::Install { how, missing } => {
            format!("built in ({how}), missing {}", missing.join(", "))
        }
        HostPath::Generate { why, .. } => {
            format!("no built-in driver ({why}); one can be generated")
        }
        HostPath::Unavailable { why } => format!("unavailable, {why}"),
    }
}

/// Ask a yes/no question on the terminal. `default` answers an empty line;
/// without a terminal the answer is `default` without asking.
fn confirm(question: &str, default: bool) -> bool {
    if !std::io::stdin().is_terminal() {
        return default;
    }
    let hint = if default { "[Y/n]" } else { "[y/N]" };
    print!("{question} {hint} ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line).is_err() {
        return default;
    }
    match line.trim().to_ascii_lowercase().as_str() {
        "" => default,
        answer => answer.starts_with('y'),
    }
}

fn choose_backend(decision: &Decision) -> Option<ComputerBackend> {
    let vm = decision.recommends_vm();
    let host = !matches!(decision.host, HostPath::Unavailable { .. });
    match (vm, host) {
        (true, true) => {
            if confirm(
                "Use a VM (a desktop in a local container, kept apart from yours)? No uses this \
                 desktop instead.",
                true,
            ) {
                Some(ComputerBackend::Vm)
            } else {
                Some(ComputerBackend::Host)
            }
        }
        (true, false) => Some(ComputerBackend::Vm),
        (false, true) => Some(ComputerBackend::Host),
        (false, false) => None,
    }
}

fn setup(backend: Option<ComputerBackendArg>, yes: bool) -> Result<i32> {
    let env = detect::probe();
    let decision = detect::decide(&env);
    println!("wizard computer setup\n");
    println!("System: {}", env.summary());
    println!("This desktop: {}", describe_host(&decision.host));
    match &decision.vm {
        VmPath::Available { engine } => println!("VM: available through {engine}"),
        VmPath::Unavailable { why } => println!("VM: unavailable, {why}"),
    }
    if !decision.verified {
        println!("Note: this platform's path is untested; expect to fix things by hand.");
    }
    println!();

    let chosen = match backend {
        Some(arg) => backend_of(arg),
        None if !std::io::stdin().is_terminal() && !yes => {
            println!(
                "Nothing changed. Run this in a terminal to answer the questions, or pass \
                 --backend vm|host --yes."
            );
            return Ok(1);
        }
        None => match choose_backend(&decision) {
            Some(choice) => choice,
            None => {
                println!(
                    "There is nothing here Wizard can drive: no desktop session and no Docker \
                     or Podman for a VM. Install one of those and run this again."
                );
                return Ok(1);
            }
        },
    };

    let mut config = Config::load()?;
    match chosen {
        ComputerBackend::Vm => {
            if let VmPath::Available { engine } = &decision.vm
                && config.computer.vm.address.is_none()
            {
                config.computer.vm.engine = engine.clone();
            }
            let status = vm::up(&config.computer.vm, false)?;
            println!("VM {}", status.describe());
            let backend = vm::VmBackend::new(config.computer.vm.vnc_address());
            let report = verify(&backend, None)?;
            println!("Check: {report}");
            config.computer.backend = ComputerBackend::Vm;
        }
        ComputerBackend::Host => {
            let driver = match host_setup(&env, &decision, yes)? {
                Some(driver) => driver,
                None => return Ok(1),
            };
            config.computer.backend = ComputerBackend::Host;
            config.computer.driver = driver;
        }
    }
    config.computer.enabled = true;
    config.save()?;
    println!(
        "\nComputer use is on. New sessions have the `computer` tool; a running one gets it \
         after /reload."
    );
    if chosen == ComputerBackend::Vm {
        println!(
            "Wizard GUI shows the VM live in its screen panel. `wizard computer vm down` stops it."
        );
    }
    Ok(0)
}

/// Host setup. Returns the driver to use once one checks out, or `None`
/// after explaining why not.
fn host_setup(
    env: &detect::Environment,
    decision: &Decision,
    yes: bool,
) -> Result<Option<HostDriver>> {
    match &decision.host {
        HostPath::Unavailable { why } => {
            println!("This desktop cannot be driven: {why}. Use --backend vm.");
            Ok(None)
        }
        HostPath::Ready { how } => {
            println!("Using the built-in driver: {how}.");
            check_host(HostDriver::Native)
        }
        HostPath::Install { how, missing } => {
            println!(
                "The built-in driver covers this desktop ({how}) but needs: {}.",
                missing.join(", ")
            );
            if yes || confirm("Run `wizard desktop-setup` to install them now?", true) {
                super::setup::run()?;
            }
            println!(
                "Once those are installed (and you have logged out and back in if setup said \
                 so), run `wizard computer setup --backend host` again."
            );
            Ok(None)
        }
        HostPath::Generate { why, plan } => {
            println!("Wizard has no built-in driver for this desktop: {why}.");
            println!("The agent can write one as a scripted tool in ~/.wizard/tools/. Plan:");
            for step in plan {
                println!("  - {step}");
            }
            if !(yes
                || confirm(
                    "Should the agent generate computer-use tools for this system?",
                    false,
                ))
            {
                println!("Nothing changed.");
                return Ok(None);
            }
            generate_driver(env, decision)?;
            check_host(HostDriver::Scripted)
        }
    }
}

fn check_host(driver: HostDriver) -> Result<Option<HostDriver>> {
    let config = ComputerConfig {
        enabled: true,
        backend: ComputerBackend::Host,
        driver,
        ..ComputerConfig::default()
    };
    let backend = super::backend_for(&config);
    match verify(&*backend, None) {
        Ok(report) => {
            println!("Check: {report}");
            Ok(Some(driver))
        }
        Err(err) => {
            println!("Check failed: {err:#}");
            println!("Computer use stays off. Fix the above and run `wizard computer check`.");
            Ok(None)
        }
    }
}

/// Hand the driver to the agent: a child `wizard -p` with the generation
/// prompt, using whatever model the user configured.
fn generate_driver(env: &detect::Environment, decision: &Decision) -> Result<()> {
    let prompt = driver::generation_prompt(env, decision);
    let exe = std::env::current_exe().context("locating the wizard binary")?;
    println!("\nAsking the agent to write the driver...\n");
    let status = std::process::Command::new(exe)
        .arg("--prompt")
        .arg(&prompt)
        .status()
        .context("starting the agent")?;
    if !status.success() {
        bail!("the agent exited with {status}");
    }
    let manifest = driver::manifest_path()?;
    if !manifest.is_file() {
        bail!("the agent finished without writing {}", manifest.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::computer::{MouseButton, Screenshot, ScrollDirection};
    use std::sync::Mutex;

    /// A backend that records moves and serves a fixed screen.
    struct Fake {
        moves: Mutex<Vec<(i32, i32)>>,
        cursor: Option<(i32, i32)>,
    }

    impl Backend for Fake {
        fn label(&self) -> String {
            "fake".into()
        }
        fn screenshot(&self) -> anyhow::Result<Screenshot> {
            Ok(Screenshot {
                png: vec![],
                width: 100,
                height: 50,
            })
        }
        fn mouse_move(&self, x: i32, y: i32) -> anyhow::Result<()> {
            self.moves.lock().unwrap().push((x, y));
            Ok(())
        }
        fn click(&self, _: MouseButton, _: u32) -> anyhow::Result<()> {
            unreachable!("verify never clicks")
        }
        fn drag(&self, _: i32, _: i32) -> anyhow::Result<()> {
            unreachable!("verify never drags")
        }
        fn type_text(&self, _: &str) -> anyhow::Result<()> {
            unreachable!("verify never types")
        }
        fn key(&self, _: &str) -> anyhow::Result<()> {
            unreachable!("verify never presses keys")
        }
        fn scroll(&self, _: ScrollDirection, _: u32) -> anyhow::Result<()> {
            unreachable!("verify never scrolls")
        }
        fn cursor_position(&self) -> anyhow::Result<(i32, i32)> {
            self.cursor.ok_or_else(|| anyhow::anyhow!("unknown"))
        }
    }

    #[test]
    fn verify_only_nudges_the_pointer_and_puts_it_back() {
        let fake = Fake {
            moves: Mutex::new(vec![]),
            cursor: Some((99, 10)),
        };
        let report = verify(&fake, None).unwrap();
        assert!(report.contains("100x50"), "{report}");
        // At the right edge, the nudge goes left.
        assert_eq!(*fake.moves.lock().unwrap(), [(98, 10), (99, 10)]);

        let fake = Fake {
            moves: Mutex::new(vec![]),
            cursor: None,
        };
        verify(&fake, None).unwrap();
        assert_eq!(*fake.moves.lock().unwrap(), [(51, 25), (50, 25)]);
    }

    #[test]
    fn the_report_says_how_to_turn_it_on() {
        let report = status_report(&ComputerConfig::default());
        assert!(report.starts_with("computer use: off"), "{report}");
        assert!(report.contains("wizard computer setup"), "{report}");
    }
}
