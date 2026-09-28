//! The `vm` backend: a desktop in a local container, driven over VNC.
//!
//! `wizard computer vm up` builds a small Alpine image (Xvfb, fluxbox, xterm,
//! x11vnc; the recipe is `contrib/computer-vm/`, compiled into the binary so
//! an installed Wizard needs no checkout) and runs it with the VNC port
//! published on 127.0.0.1. The [`VmBackend`] then speaks RFB to it through
//! [`super::rfb`] for both screenshots and input.
//!
//! Why VNC rather than `docker exec xdotool`: one connection carries frames
//! and input with no per-action process spawn, the same client serves Wizard
//! GUI's live panel and its take-control mode, and any VNC-speaking VM works
//! (a QEMU `-vnc` display via `[computer.vm] address`) without changing the
//! backend.

use std::collections::HashMap;
use std::process::Command;
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use super::rfb::{self, Client, Input};
use super::state::{self, VmState};
use super::{Backend, MouseButton, Screenshot, ScrollDirection};
use crate::config::ComputerVmConfig;

const DOCKERFILE: &str = include_str!("../../../contrib/computer-vm/Dockerfile");
const START_SH: &str = include_str!("../../../contrib/computer-vm/start.sh");
const MENU: &str = include_str!("../../../contrib/computer-vm/menu");

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Last pointer position per VNC address. RFB has no "where is the pointer"
/// query, and `click` and `drag` start from wherever it was left.
fn pointers() -> &'static Mutex<HashMap<String, (u16, u16)>> {
    static POINTERS: OnceLock<Mutex<HashMap<String, (u16, u16)>>> = OnceLock::new();
    POINTERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Drives the desktop behind a VNC address. One short connection per action.
pub(crate) struct VmBackend {
    address: String,
}

impl VmBackend {
    pub(crate) fn new(address: String) -> Self {
        Self { address }
    }

    fn connect(&self) -> Result<Client> {
        Client::connect(&self.address, CONNECT_TIMEOUT).with_context(|| {
            format!(
                "could not reach the VM's VNC server at {}. Is it running? Start it with \
                 `wizard computer vm up`.",
                self.address
            )
        })
    }

    fn pointer(&self) -> Option<(u16, u16)> {
        pointers()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&self.address)
            .copied()
    }

    fn set_pointer(&self, xy: (u16, u16)) {
        pointers()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(self.address.clone(), xy);
    }

    /// Run `f` with a fresh connection's input handle, then wait until the
    /// server has handled everything sent. The wait is a one-pixel,
    /// non-incremental update request: the server answers messages in order,
    /// so its reply proves the input before it was processed, and closing the
    /// socket straight after sending could otherwise drop the tail.
    fn with_input(&self, f: impl FnOnce(&Client, &Input) -> Result<()>) -> Result<()> {
        let mut client = self.connect()?;
        let input = client.input();
        f(&client, &input)?;
        input.request_update(
            false,
            rfb::Rect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
        )?;
        loop {
            if let rfb::Event::Updated(_) | rfb::Event::Resized { .. } = client.read_event()? {
                return Ok(());
            }
        }
    }

    fn clamp(client: &Client, x: i32, y: i32) -> Result<(u16, u16)> {
        let (w, h) = (i32::from(client.width()), i32::from(client.height()));
        if x < 0 || y < 0 || x >= w || y >= h {
            bail!("({x}, {y}) is outside the VM's {w}x{h} screen");
        }
        Ok((x as u16, y as u16))
    }
}

fn button_mask(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => rfb::BUTTON_LEFT,
        MouseButton::Middle => rfb::BUTTON_MIDDLE,
        MouseButton::Right => rfb::BUTTON_RIGHT,
    }
}

impl Backend for VmBackend {
    fn label(&self) -> String {
        format!("vm: VNC at {}", self.address)
    }

    fn screenshot(&self) -> Result<Screenshot> {
        let mut client = self.connect()?;
        client.refresh().context("reading the VM's screen")?;
        let (width, height) = (u32::from(client.width()), u32::from(client.height()));
        let png = encode_png(client.framebuffer(), width, height)?;
        Ok(Screenshot { png, width, height })
    }

    fn mouse_move(&self, x: i32, y: i32) -> Result<()> {
        let mut at = None;
        self.with_input(|client, input| {
            let xy = Self::clamp(client, x, y)?;
            input.pointer(xy.0, xy.1, 0)?;
            at = Some(xy);
            Ok(())
        })?;
        if let Some(xy) = at {
            self.set_pointer(xy);
        }
        Ok(())
    }

    fn click(&self, button: MouseButton, count: u32) -> Result<()> {
        let (x, y) = self.pointer().unwrap_or((0, 0));
        self.with_input(|_, input| Ok(input.click(x, y, button_mask(button), count)?))
    }

    fn drag(&self, x: i32, y: i32) -> Result<()> {
        let (x0, y0) = self.pointer().unwrap_or((0, 0));
        let mut end = None;
        self.with_input(|client, input| {
            let (x1, y1) = Self::clamp(client, x, y)?;
            input.pointer(x0, y0, rfb::BUTTON_LEFT)?;
            // A few points in between, so the target sees motion with the
            // button held rather than a jump.
            for eighth in 1..=8i32 {
                let lerp = |a: u16, b: u16| {
                    (i32::from(a) + (i32::from(b) - i32::from(a)) * eighth / 8) as u16
                };
                input.pointer(lerp(x0, x1), lerp(y0, y1), rfb::BUTTON_LEFT)?;
            }
            input.pointer(x1, y1, 0)?;
            end = Some((x1, y1));
            Ok(())
        })?;
        if let Some(xy) = end {
            self.set_pointer(xy);
        }
        Ok(())
    }

    fn type_text(&self, text: &str) -> Result<()> {
        self.with_input(|_, input| Ok(input.type_text(text)?))
    }

    fn key(&self, chord: &str) -> Result<()> {
        rfb::parse_chord(chord).map_err(|e| anyhow!(e))?;
        self.with_input(|_, input| Ok(input.chord(chord)?))
    }

    fn scroll(&self, direction: ScrollDirection, amount: u32) -> Result<()> {
        let (x, y) = self.pointer().unwrap_or((0, 0));
        let wheel = match direction {
            ScrollDirection::Up => rfb::WHEEL_UP,
            ScrollDirection::Down => rfb::WHEEL_DOWN,
            ScrollDirection::Left => rfb::WHEEL_LEFT,
            ScrollDirection::Right => rfb::WHEEL_RIGHT,
        };
        self.with_input(|_, input| Ok(input.click(x, y, wheel, amount)?))
    }

    fn cursor_position(&self) -> Result<(i32, i32)> {
        self.pointer()
            .map(|(x, y)| (i32::from(x), i32::from(y)))
            .ok_or_else(|| anyhow!("the pointer has not been moved in this session yet"))
    }
}

/// RGBA framebuffer to PNG, dropping the alpha channel (it is always opaque).
fn encode_png(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    use image::ImageEncoder;
    let rgb: Vec<u8> = rgba
        .chunks_exact(4)
        .flat_map(|px| [px[0], px[1], px[2]])
        .collect();
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new_with_quality(
        &mut out,
        image::codecs::png::CompressionType::Fast,
        image::codecs::png::FilterType::Adaptive,
    )
    .write_image(&rgb, width, height, image::ExtendedColorType::Rgb8)
    .context("encoding the VM screenshot")?;
    Ok(out)
}

/* ---------------------------------------------------------------------- */
/* Lifecycle: `wizard computer vm up|down|status`                          */
/* ---------------------------------------------------------------------- */

/// What `vm status` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmStatus {
    /// `running`, `exited`, `absent`, or `external` for an address Wizard
    /// does not manage.
    pub container: String,
    /// The VNC server answered, with this desktop size.
    pub desktop: Option<(u16, u16)>,
    pub address: String,
}

impl VmStatus {
    pub fn is_up(&self) -> bool {
        self.desktop.is_some()
    }

    pub fn describe(&self) -> String {
        match (self.container.as_str(), self.desktop) {
            (_, Some((w, h))) => format!("up: VNC at {} answers, desktop {w}x{h}", self.address),
            ("external", None) => format!("down: nothing answers VNC at {}", self.address),
            ("absent", None) => "down: no container (start it with `wizard computer vm up`)".into(),
            (state, None) => format!(
                "down: container is {state} and VNC at {} does not answer",
                self.address
            ),
        }
    }
}

fn engine_output(engine: &str, args: &[&str]) -> Result<std::process::Output> {
    Command::new(engine)
        .args(args)
        .output()
        .with_context(|| format!("could not run `{engine}`; is it installed?"))
}

fn container_state(config: &ComputerVmConfig) -> Result<String> {
    let out = engine_output(
        &config.engine,
        &["inspect", "-f", "{{.State.Status}}", &config.container],
    )?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Ok("absent".into())
    }
}

fn probe_vnc(address: &str) -> Option<(u16, u16)> {
    let client = Client::connect(address, Duration::from_secs(2)).ok()?;
    Some((client.width(), client.height()))
}

/// Container and VNC state, without changing anything.
pub fn status(config: &ComputerVmConfig) -> Result<VmStatus> {
    let address = config.vnc_address();
    let container = if config.address.is_some() {
        "external".to_string()
    } else {
        container_state(config)?
    };
    Ok(VmStatus {
        desktop: probe_vnc(&address),
        container,
        address,
    })
}

/// Build the image if it is missing, start the container if it is not
/// running, wait for VNC, and record where it listens. Idempotent.
pub fn up(config: &ComputerVmConfig, rebuild: bool) -> Result<VmStatus> {
    let address = config.vnc_address();
    if config.address.is_some() {
        let status = status(config)?;
        if !status.is_up() {
            bail!(
                "[computer.vm] address points at {address}, which Wizard does not start; \
                 nothing answers there"
            );
        }
        record(config, &status);
        return Ok(status);
    }

    let image_present = engine_output(&config.engine, &["image", "inspect", &config.image])?
        .status
        .success();
    if rebuild || !image_present {
        build_image(config)?;
    }

    match container_state(config)?.as_str() {
        "running" => {}
        "absent" => run_container(config)?,
        _ => {
            // Stopped or wedged: replace it rather than guess what state its
            // display server was left in.
            let _ = engine_output(&config.engine, &["rm", "-f", &config.container]);
            run_container(config)?;
        }
    }

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(desktop) = probe_vnc(&address) {
            let status = VmStatus {
                container: "running".into(),
                desktop: Some(desktop),
                address,
            };
            record(config, &status);
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let logs = engine_output(&config.engine, &["logs", "--tail", "20", &config.container])
                .map(|o| String::from_utf8_lossy(&o.stderr).into_owned())
                .unwrap_or_default();
            bail!("the VM started but VNC at {address} never answered. Container log:\n{logs}");
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn record(config: &ComputerVmConfig, status: &VmStatus) {
    let (width, height) = status
        .desktop
        .map_or((config.width, config.height), |(w, h)| {
            (u32::from(w), u32::from(h))
        });
    let managed = config.address.is_none();
    if let Err(err) = state::write_vm_state(&VmState {
        address: status.address.clone(),
        width,
        height,
        engine: if managed {
            config.engine.clone()
        } else {
            String::new()
        },
        container: if managed {
            config.container.clone()
        } else {
            String::new()
        },
    }) {
        tracing::warn!("computer: could not record the VM's address: {err:#}");
    }
}

fn build_image(config: &ComputerVmConfig) -> Result<()> {
    let dir = crate::platform::paths::staging_dir("computer-vm-build")?;
    std::fs::write(dir.join("Dockerfile"), DOCKERFILE)?;
    std::fs::write(dir.join("start.sh"), START_SH)?;
    std::fs::write(dir.join("menu"), MENU)?;
    eprintln!("Building the VM image {} (first run only)...", config.image);
    let status = Command::new(&config.engine)
        .args(["build", "-t", &config.image])
        .arg(&dir)
        .status()
        .with_context(|| format!("could not run `{}`", config.engine))?;
    if !status.success() {
        bail!("`{} build` failed; see its output above", config.engine);
    }
    Ok(())
}

fn run_container(config: &ComputerVmConfig) -> Result<()> {
    let publish = format!("127.0.0.1:{}:5900", config.port);
    let out = engine_output(
        &config.engine,
        &[
            "run",
            "-d",
            "--rm",
            "--name",
            &config.container,
            "-p",
            &publish,
            "-e",
            &format!("WIDTH={}", config.width),
            "-e",
            &format!("HEIGHT={}", config.height),
            "--shm-size",
            "256m",
            &config.image,
        ],
    )?;
    if !out.status.success() {
        bail!(
            "`{} run` failed: {}",
            config.engine,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Stop and remove the container. A VNC address Wizard does not manage is
/// left alone.
pub fn down(config: &ComputerVmConfig) -> Result<()> {
    state::clear_vm_state();
    if config.address.is_some() {
        return Ok(());
    }
    let out = engine_output(&config.engine, &["rm", "-f", &config.container])?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !stderr.contains("No such container") && !stderr.contains("no such container") {
            bail!("`{} rm` failed: {}", config.engine, stderr.trim());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_recipe_matches_the_contrib_files() {
        assert!(DOCKERFILE.contains("x11vnc"));
        assert!(DOCKERFILE.contains("COPY start.sh"));
        assert!(DOCKERFILE.contains("COPY menu"));
        assert!(START_SH.contains("-rfbport 5900"));
    }

    #[test]
    fn png_encoding_keeps_the_size_and_drops_alpha() {
        let rgba = [
            255u8, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 9, 9, 9, 255,
        ];
        let png = encode_png(&rgba, 2, 2).unwrap();
        assert_eq!(super::super::png_dimensions(&png), Some((2, 2)));
        let decoded = image::load_from_memory(&png).unwrap().to_rgb8();
        assert_eq!(decoded.get_pixel(1, 0).0, [0, 255, 0]);
    }

    #[test]
    fn status_text_says_how_to_start_it() {
        let status = VmStatus {
            container: "absent".into(),
            desktop: None,
            address: "127.0.0.1:5905".into(),
        };
        assert!(status.describe().contains("wizard computer vm up"));
        let status = VmStatus {
            container: "running".into(),
            desktop: Some((1280, 800)),
            address: "127.0.0.1:5905".into(),
        };
        assert!(status.is_up());
        assert!(status.describe().contains("1280x800"));
    }
}
