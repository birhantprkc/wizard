//! Live screen: what Wizard's `computer` tool sees, in a floating card that
//! opens into a lightbox, with a "Take control" mode that pauses the agent
//! and forwards this window's mouse and keyboard to that screen.
//!
//! Frames come straight from the source rather than through the agent:
//!
//! - **VM backend**: Wizard records the VM's VNC address in
//!   `~/.wizard/computer/vm.json`, and this panel opens its own RFB
//!   connection to it. That is live at video rate whether or not a turn is
//!   running, and it is the same connection take-control types into. The
//!   client is Wizard's own (`src/tools/computer/rfb.rs`, std only), compiled
//!   here by path so the two cannot drift.
//! - **Host backend**: the agent's screenshots, which Wizard keeps as
//!   `latest.png`. There is no live stream of the host; the user is already
//!   looking at it.
//!
//! The agent's last action (`last-action.json`) draws a ripple where it
//! clicked and a caption for what it typed. Taking control writes the
//! `control` lease that makes the `computer` tool hold its input actions;
//! giving it back, closing the app, or this process dying releases it.
//!
//! Going through ACP instead would mean base64 frames through the harness,
//! the engine and the Loro doc, none of which carries images today (tool
//! call images render as nothing), and a relay for input in the other
//! direction. Files plus a direct VNC socket need neither.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gpui::{
    AnyElement, Bounds, Context, FocusHandle, Pixels, Point, RenderImage, SharedString, Task,
    Window, div, point, prelude::*, px, size,
};

use crate::theme::Theme;

#[allow(dead_code)]
#[path = "../../../../src/tools/computer/rfb.rs"]
mod rfb;

/// Width of the floating card.
const CARD_WIDTH: f32 = 300.0;
/// How often the files under `~/.wizard/computer/` are looked at.
const POLL: Duration = Duration::from_millis(300);
/// Frames are published at most this often.
const FRAME_INTERVAL: Duration = Duration::from_millis(66);
/// A click ripple lasts this long.
const RIPPLE: Duration = Duration::from_millis(1200);
/// The host card stays up this long after the agent's last host action.
const HOST_RECENT: Duration = Duration::from_secs(600);

/// The agent's last `computer` action, as `last-action.json` has it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct Action {
    pub seq: u64,
    pub at_ms: u64,
    pub backend: String,
    pub action: String,
    #[serde(default)]
    pub x: Option<i32>,
    #[serde(default)]
    pub y: Option<i32>,
    #[serde(default)]
    pub text: Option<String>,
}

impl Action {
    /// What the caption under the screen says, if anything.
    pub fn caption(&self) -> Option<String> {
        match self.action.as_str() {
            "type" => self
                .text
                .as_ref()
                .map(|t| format!("typed \u{201c}{t}\u{201d}")),
            "key" => self.text.as_ref().map(|t| format!("pressed {t}")),
            "scroll" => Some("scrolled".into()),
            "screenshot" => None,
            other => Some(other.replace('_', " ")),
        }
    }

    /// Whether the action has a point worth a ripple.
    pub fn point(&self) -> Option<(i32, i32)> {
        match self.action.as_str() {
            "left_click" | "right_click" | "middle_click" | "double_click" | "mouse_move"
            | "left_click_drag" => Some((self.x?, self.y?)),
            _ => None,
        }
    }
}

/// `~/.wizard/computer`, honouring `WIZARD_HOME` the way Wizard does.
///
/// `None` under test: a Shell built in a test must not read, or connect to
/// the VM of, whoever is running the suite.
pub fn computer_dir() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    let home = std::env::var_os("WIZARD_HOME")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".wizard")))?;
    Some(home.join("computer"))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The lease the `computer` tool waits on. Same shape as Wizard's
/// `ControlLease`: `{pid, holder, since_ms}`.
pub fn lease_json(pid: u32, since_ms: u64) -> String {
    serde_json::json!({ "pid": pid, "holder": "Wizard GUI", "since_ms": since_ms }).to_string()
}

fn write_lease(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".control.{}", std::process::id()));
    std::fs::write(&tmp, lease_json(std::process::id(), now_ms()))?;
    std::fs::rename(&tmp, dir.join("control"))
}

fn release_lease(dir: &Path) {
    let path = dir.join("control");
    // Only ours: a lease another process holds is not this panel's to drop.
    let ours = std::fs::read(&path)
        .ok()
        .and_then(|raw| serde_json::from_slice::<serde_json::Value>(&raw).ok())
        .and_then(|lease| lease["pid"].as_u64())
        == Some(u64::from(std::process::id()));
    if ours {
        let _ = std::fs::remove_file(path);
    }
}

/// Where a `frame`-sized picture sits when fitted into `avail`, centered,
/// scaled by at most `max_scale`.
pub fn fit(frame: (u32, u32), avail: gpui::Size<Pixels>, max_scale: f32) -> gpui::Size<Pixels> {
    let (fw, fh) = (frame.0.max(1) as f32, frame.1.max(1) as f32);
    let scale = (f32::from(avail.width) / fw)
        .min(f32::from(avail.height) / fh)
        .min(max_scale);
    size(px(fw * scale), px(fh * scale))
}

/// The screen pixel under `position`, for a frame painted into `bounds`.
pub fn to_screen(
    position: Point<Pixels>,
    bounds: Bounds<Pixels>,
    frame: (u32, u32),
) -> Option<(u16, u16)> {
    let rel = position - bounds.origin;
    let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let (rx, ry) = (f32::from(rel.x) / w, f32::from(rel.y) / h);
    if !(0.0..1.0).contains(&rx) || !(0.0..1.0).contains(&ry) {
        return None;
    }
    let x = (rx * frame.0 as f32) as u32;
    let y = (ry * frame.1 as f32) as u32;
    Some((
        x.min(u32::from(u16::MAX)) as u16,
        y.min(u32::from(u16::MAX)) as u16,
    ))
}

/// The keysyms a keystroke sends: modifiers to hold, and the key. A plain
/// printable character is sent as itself, with shift already applied.
pub fn keysyms(stroke: &gpui::Keystroke) -> Option<(Vec<u32>, u32)> {
    let m = stroke.modifiers;
    let chorded = m.control || m.alt || m.platform;
    if !chorded
        && let Some(text) = stroke.key_char.as_deref()
        && let [c] = text.chars().collect::<Vec<_>>()[..]
        && !c.is_control()
    {
        return Some((Vec::new(), rfb::keysym_for_char(c)));
    }
    let key = rfb::key_keysym(&stroke.key)?;
    let mut mods = Vec::new();
    for (held, name) in [
        (m.control, "ctrl"),
        (m.alt, "alt"),
        (m.shift, "shift"),
        (m.platform, "super"),
    ] {
        if held {
            mods.extend(rfb::modifier_keysym(name));
        }
    }
    Some((mods, key))
}

fn button_bit(button: gpui::MouseButton) -> u8 {
    match button {
        gpui::MouseButton::Left => rfb::BUTTON_LEFT,
        gpui::MouseButton::Middle => rfb::BUTTON_MIDDLE,
        gpui::MouseButton::Right => rfb::BUTTON_RIGHT,
        gpui::MouseButton::Navigate(_) => 0,
    }
}

/// RGBA from the RFB client to the BGRA a `RenderImage` holds.
fn render_image(rgba: &[u8], width: u32, height: u32) -> Option<Arc<RenderImage>> {
    let mut bgra = rgba.to_vec();
    for px in bgra.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    let pixels = image::RgbaImage::from_raw(width, height, bgra)?;
    Some(Arc::new(RenderImage::new([image::Frame::new(pixels)])))
}

/// State the VNC thread shares with the panel.
#[derive(Default)]
struct Shared {
    frame: Mutex<Option<(Arc<RenderImage>, u32, u32)>>,
    input: Mutex<Option<rfb::Input>>,
    status: Mutex<Option<String>>,
}

/// A running VNC connection, stopped when dropped.
struct VmLink {
    address: String,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    _feed: Task<()>,
}

impl Drop for VmLink {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn run_vnc(
    address: String,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    notify: tokio::sync::mpsc::Sender<()>,
) {
    let set_status = |status: Option<String>| {
        *shared.status.lock().unwrap() = status;
        let _ = notify.try_send(());
    };
    while !stop.load(Ordering::Relaxed) {
        let mut client = match rfb::Client::connect(&address, Duration::from_secs(3)) {
            Ok(client) => client,
            Err(err) => {
                set_status(Some(format!("VM not reachable at {address}: {err}")));
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        let input = client.input();
        *shared.input.lock().unwrap() = Some(input.clone());
        set_status(None);
        let _ = client.set_read_timeout(Some(Duration::from_millis(500)));
        let mut last = Instant::now() - FRAME_INTERVAL;
        let mut result = input.request_update(false, client.full());
        while result.is_ok() && !stop.load(Ordering::Relaxed) {
            match client.read_event() {
                Ok(rfb::Event::Updated(_)) => {
                    let (w, h) = (u32::from(client.width()), u32::from(client.height()));
                    if let Some(image) = render_image(client.framebuffer(), w, h) {
                        *shared.frame.lock().unwrap() = Some((image, w, h));
                        let _ = notify.try_send(());
                    }
                    // Pace the next request rather than the painting: an
                    // incremental request only comes back when something
                    // changed, so this caps a busy screen and costs an idle
                    // one nothing.
                    let wait = FRAME_INTERVAL.saturating_sub(last.elapsed());
                    std::thread::sleep(wait);
                    last = Instant::now();
                    result = input.request_update(true, client.full());
                }
                Ok(rfb::Event::Resized { .. }) => {
                    result = input.request_update(false, client.full());
                }
                Ok(_) => {}
                Err(err)
                    if matches!(
                        err.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(err) => result = Err(err),
            }
        }
        *shared.input.lock().unwrap() = None;
        if let Err(err) = result {
            set_status(Some(format!("VM connection lost: {err}")));
            std::thread::sleep(Duration::from_millis(500));
        }
    }
}

/// Where the picture comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    None,
    Vm,
    Host,
}

pub struct LiveScreen {
    dir: Option<PathBuf>,
    focus: FocusHandle,
    link: Option<VmLink>,
    source: Source,
    frame: Option<Arc<RenderImage>>,
    frame_size: (u32, u32),
    /// Frames replaced since the last paint; dropped from the atlas with the
    /// window, which only render has.
    retired: Vec<Arc<RenderImage>>,
    host_frame_mtime: Option<SystemTime>,
    action: Option<Action>,
    action_mtime: Option<SystemTime>,
    /// When the current action was first seen, for the ripple.
    action_seen: Option<Instant>,
    /// The card was closed at this action; it comes back with the next one.
    dismissed_at: Option<u64>,
    expanded: bool,
    in_control: bool,
    buttons: u8,
    pointer: Option<(u16, u16)>,
    status: Option<SharedString>,
    image_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    _poll: Option<Task<()>>,
}

impl LiveScreen {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let dir = computer_dir();
        let poll = dir.is_some().then(|| {
            cx.spawn(async move |this, cx| {
                loop {
                    if this.update(cx, |this, cx| this.poll(cx)).is_err() {
                        break;
                    }
                    cx.background_executor().timer(POLL).await;
                }
            })
        });
        Self {
            dir,
            focus: cx.focus_handle(),
            link: None,
            source: Source::None,
            frame: None,
            frame_size: (0, 0),
            retired: Vec::new(),
            host_frame_mtime: None,
            action: None,
            action_mtime: None,
            action_seen: None,
            dismissed_at: None,
            expanded: false,
            in_control: false,
            buttons: 0,
            pointer: None,
            status: None,
            image_bounds: Rc::new(Cell::new(None)),
            _poll: poll,
        }
    }

    fn replace_frame(&mut self, frame: Arc<RenderImage>, width: u32, height: u32) {
        if let Some(old) = self.frame.replace(frame) {
            self.retired.push(old);
        }
        self.frame_size = (width, height);
    }

    fn clear_frame(&mut self) {
        if let Some(old) = self.frame.take() {
            self.retired.push(old);
        }
    }

    /// Look at `~/.wizard/computer/` and bring the connection and frames in
    /// line with it.
    fn poll(&mut self, cx: &mut Context<Self>) {
        let Some(dir) = self.dir.clone() else {
            return;
        };
        let mut changed = false;

        // The agent's last action.
        let action_path = dir.join("last-action.json");
        let mtime = std::fs::metadata(&action_path)
            .and_then(|m| m.modified())
            .ok();
        if mtime != self.action_mtime {
            self.action_mtime = mtime;
            let action = std::fs::read(&action_path)
                .ok()
                .and_then(|raw| serde_json::from_slice::<Action>(&raw).ok());
            if action.as_ref().map(|a| a.seq) != self.action.as_ref().map(|a| a.seq) {
                self.action_seen = Some(Instant::now());
                self.action = action;
                changed = true;
            }
        }

        // The VM, if one is up.
        let vm_address = std::fs::read(dir.join("vm.json"))
            .ok()
            .and_then(|raw| serde_json::from_slice::<serde_json::Value>(&raw).ok())
            .and_then(|vm| vm["address"].as_str().map(str::to_string));
        match (&vm_address, &self.link) {
            (Some(address), Some(link)) if &link.address == address => {}
            (Some(address), _) => {
                self.link = Some(self.connect(address.clone(), cx));
                self.source = Source::Vm;
                changed = true;
            }
            (None, Some(_)) => {
                self.link = None;
                self.source = Source::None;
                self.clear_frame();
                changed = true;
            }
            (None, None) => {}
        }

        // Otherwise the host's latest screenshot, while the agent is using it.
        if self.link.is_none() {
            let recent_host = self.action.as_ref().is_some_and(|a| {
                a.backend == "host"
                    && now_ms().saturating_sub(a.at_ms) < HOST_RECENT.as_millis() as u64
            });
            let frame_path = dir.join("latest.png");
            let mtime = std::fs::metadata(&frame_path)
                .and_then(|m| m.modified())
                .ok();
            if recent_host && mtime.is_some() && mtime != self.host_frame_mtime {
                self.host_frame_mtime = mtime;
                self.source = Source::Host;
                cx.spawn(async move |this, cx| {
                    let decoded = cx
                        .background_executor()
                        .spawn(async move {
                            let raw = std::fs::read(&frame_path).ok()?;
                            let rgba = image::load_from_memory(&raw).ok()?.to_rgba8();
                            let (w, h) = rgba.dimensions();
                            render_image(rgba.as_raw(), w, h).map(|image| (image, w, h))
                        })
                        .await;
                    if let Some((image, w, h)) = decoded {
                        let _ = this.update(cx, |this, cx| {
                            this.replace_frame(image, w, h);
                            cx.notify();
                        });
                    }
                })
                .detach();
            } else if !recent_host && self.source == Source::Host {
                self.source = Source::None;
                self.clear_frame();
                changed = true;
            }
        }

        // A ripple is still fading.
        if self.action_seen.is_some_and(|seen| seen.elapsed() < RIPPLE) {
            changed = true;
        }
        if changed {
            cx.notify();
        }
    }

    fn connect(&self, address: String, cx: &mut Context<Self>) -> VmLink {
        let shared = Arc::new(Shared::default());
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        {
            let (address, shared, stop) = (address.clone(), shared.clone(), stop.clone());
            let _ = std::thread::Builder::new()
                .name("live-screen-vnc".into())
                .spawn(move || run_vnc(address, shared, stop, tx));
        }
        let feed_shared = shared.clone();
        let feed = cx.spawn(async move |this, cx| {
            while rx.recv().await.is_some() {
                let frame = feed_shared.frame.lock().unwrap().take();
                let status = feed_shared.status.lock().unwrap().clone();
                let alive = this.update(cx, |this, cx| {
                    if let Some((image, w, h)) = frame {
                        this.replace_frame(image, w, h);
                    }
                    this.status = status.map(SharedString::from);
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        VmLink {
            address,
            shared,
            stop,
            _feed: feed,
        }
    }

    fn input(&self) -> Option<rfb::Input> {
        self.link
            .as_ref()
            .and_then(|link| link.shared.input.lock().unwrap().clone())
    }

    fn set_control(&mut self, take: bool, cx: &mut Context<Self>) {
        let Some(dir) = self.dir.clone() else {
            return;
        };
        if take {
            if let Err(err) = write_lease(&dir) {
                self.status = Some(format!("Could not take control: {err}").into());
                cx.notify();
                return;
            }
        } else {
            release_lease(&dir);
            // Let go of anything still held in the remote.
            if self.buttons != 0
                && let (Some(input), Some((x, y))) = (self.input(), self.pointer)
            {
                let _ = input.pointer(x, y, 0);
            }
            self.buttons = 0;
        }
        self.in_control = take;
        cx.notify();
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        if self.in_control {
            self.set_control(false, cx);
        }
        self.expanded = false;
        cx.notify();
    }

    fn pointer_event(&mut self, position: Point<Pixels>, buttons: u8) {
        if !self.in_control {
            return;
        }
        let (Some(bounds), Some(input)) = (self.image_bounds.get(), self.input()) else {
            return;
        };
        if let Some((x, y)) = to_screen(position, bounds, self.frame_size) {
            self.pointer = Some((x, y));
            let _ = input.pointer(x, y, buttons);
        }
    }

    fn render_screen(&self, painted: gpui::Size<Pixels>, ripple: bool) -> AnyElement {
        let image = self.frame.clone();
        let frame = self.frame_size;
        let action = self.action.clone().filter(|_| ripple);
        let seen = self.action_seen;
        let bounds_out = self.image_bounds.clone();
        gpui::canvas(
            |_, _, _| (),
            move |bounds, _, window, _| {
                bounds_out.set(Some(bounds));
                window.paint_quad(gpui::fill(bounds, gpui::black()));
                if let Some(image) = image {
                    let _ = window.paint_image(bounds, gpui::Corners::default(), image, 0, false);
                }
                let Some((action, seen)) = action.zip(seen) else {
                    return;
                };
                let Some((x, y)) = action.point() else {
                    return;
                };
                let t = seen.elapsed().as_secs_f32() / RIPPLE.as_secs_f32();
                if t >= 1.0 || frame.0 == 0 || frame.1 == 0 {
                    return;
                }
                let sx = f32::from(bounds.size.width) / frame.0 as f32;
                let sy = f32::from(bounds.size.height) / frame.1 as f32;
                let center = point(
                    bounds.origin.x + px(x as f32 * sx),
                    bounds.origin.y + px(y as f32 * sy),
                );
                let radius = 6.0 + 26.0 * t;
                let ring = Bounds::new(
                    point(center.x - px(radius), center.y - px(radius)),
                    size(px(radius * 2.0), px(radius * 2.0)),
                );
                let alpha = 1.0 - t;
                window.paint_quad(gpui::quad(
                    ring,
                    px(radius),
                    gpui::hsla(0.12, 1.0, 0.6, 0.18 * alpha),
                    px(2.5),
                    gpui::hsla(0.12, 1.0, 0.6, 0.95 * alpha),
                    gpui::BorderStyle::default(),
                ));
                window.request_animation_frame();
            },
        )
        .w(painted.width)
        .h(painted.height)
        .into_any_element()
    }

    fn source_label(&self) -> &'static str {
        match self.source {
            Source::Vm => "VM",
            Source::Host => "This desktop",
            Source::None => "",
        }
    }

    fn render_card(&mut self, viewport: gpui::Size<Pixels>, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).for_popup();
        let frame = if self.frame_size.0 == 0 {
            (16, 10)
        } else {
            self.frame_size
        };
        let thumb = fit(frame, size(px(CARD_WIDTH), px(CARD_WIDTH)), 1.0);
        let active = self
            .action
            .as_ref()
            .is_some_and(|a| now_ms().saturating_sub(a.at_ms) < 5_000);
        let dot = if self.in_control {
            theme.warning
        } else if active {
            theme.success
        } else {
            theme.text_faint
        };
        let who = if self.in_control {
            "You have control"
        } else if active {
            "Agent is acting"
        } else {
            "Idle"
        };
        let card = div()
            .id("live-screen-card")
            .occlude()
            .w(px(CARD_WIDTH))
            .rounded(px(10.0))
            .overflow_hidden()
            .border_1()
            .border_color(theme.border)
            .bg(theme.surface_dialog)
            .shadow_lg()
            .cursor_pointer()
            .on_click(cx.listener(|this, _, window, cx| {
                this.expanded = true;
                window.focus(&this.focus, cx);
                cx.notify();
            }))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .px(px(10.0))
                    .py(px(6.0))
                    .text_size(crate::typography::ui_rems(11.0))
                    .text_color(theme.text_muted)
                    .child(div().size(px(7.0)).rounded_full().bg(dot))
                    .child(
                        div()
                            .text_color(theme.text)
                            .child(format!("Live screen \u{b7} {}", self.source_label())),
                    )
                    .child(div().flex_1())
                    .child(who)
                    .child(
                        div()
                            .id("live-screen-dismiss")
                            .px(px(4.0))
                            .text_color(theme.text_faint)
                            .hover(|el| el.text_color(theme.text))
                            .child("\u{d7}")
                            .on_click(cx.listener(|this, _, _, cx| {
                                cx.stop_propagation();
                                this.dismissed_at = Some(this.action.as_ref().map_or(0, |a| a.seq));
                                cx.notify();
                            })),
                    ),
            )
            .child(self.render_screen(thumb, true));
        gpui::deferred(
            gpui::anchored()
                .position(point(viewport.width - px(CARD_WIDTH + 16.0), px(56.0)))
                .child(card),
        )
        .priority(1)
        .into_any_element()
    }

    fn render_lightbox(
        &mut self,
        viewport: gpui::Size<Pixels>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).for_popup();
        let frame = if self.frame_size.0 == 0 {
            (16, 10)
        } else {
            self.frame_size
        };
        let avail = size(viewport.width * 0.9, viewport.height * 0.78);
        let painted = fit(frame, avail, 2.0);
        let vm = self.source == Source::Vm;
        let can_forward = vm && self.input().is_some();
        let caption = self.action.as_ref().and_then(Action::caption);

        let control_button = if self.in_control {
            crate::popover::btn_primary(&theme, "Give back")
                .id("live-screen-give-back")
                .on_click(cx.listener(|this, _, _, cx| this.set_control(false, cx)))
        } else {
            crate::popover::btn_primary(&theme, "Take control")
                .id("live-screen-take-control")
                .on_click(cx.listener(|this, _, window, cx| {
                    this.set_control(true, cx);
                    window.focus(&this.focus, cx);
                }))
        };
        let header = div()
            .w(painted.width)
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .text_size(crate::typography::ui_rems(12.0))
            .text_color(crate::theme::ink(0.75))
            .child(
                div()
                    .text_color(crate::theme::ink(0.95))
                    .child(format!("Live screen \u{b7} {}", self.source_label())),
            )
            .when_some(self.status.clone(), |el, status| {
                el.child(div().text_color(theme.warning).child(status))
            })
            .child(div().flex_1())
            .child(control_button)
            .child(
                crate::popover::btn_ghost(&theme, "Close", "live-screen-close")
                    .id("live-screen-close")
                    .on_click(cx.listener(|this, _, _, cx| this.close(cx))),
            );

        let hint = match (self.in_control, vm, can_forward) {
            (true, true, true) => {
                "You have control: the mouse and keyboard here go to the VM, and the agent waits. Give back when you are done."
            }
            (true, true, false) => "You have control, but the VM is not connected yet.",
            (true, false, _) => {
                "You have control: the agent waits. Use your own mouse and keyboard on this desktop, then give back."
            }
            (false, _, _) => {
                "The agent is driving. Take control to pause it and use this screen yourself."
            }
        };

        let screen = div()
            .id("live-screen-view")
            .relative()
            .w(painted.width)
            .h(painted.height)
            .rounded(px(6.0))
            .overflow_hidden()
            .border_2()
            .border_color(if self.in_control {
                theme.warning
            } else {
                theme.border
            })
            .when(self.in_control && can_forward, |el| el.cursor_crosshair())
            .child(self.render_screen(painted, !self.in_control))
            .when_some(caption.filter(|_| !self.in_control), |el, caption| {
                el.child(
                    div()
                        .absolute()
                        .bottom(px(10.0))
                        .left(px(10.0))
                        .px(px(8.0))
                        .py(px(4.0))
                        .rounded(px(6.0))
                        .bg(gpui::hsla(0.0, 0.0, 0.0, 0.7))
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(gpui::white())
                        .child(caption),
                )
            })
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, e: &gpui::MouseDownEvent, _, cx| {
                    this.mouse(e.position, e.button, true, cx)
                }),
            )
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(|this, e: &gpui::MouseDownEvent, _, cx| {
                    this.mouse(e.position, e.button, true, cx)
                }),
            )
            .on_mouse_down(
                gpui::MouseButton::Middle,
                cx.listener(|this, e: &gpui::MouseDownEvent, _, cx| {
                    this.mouse(e.position, e.button, true, cx)
                }),
            )
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|this, e: &gpui::MouseUpEvent, _, cx| {
                    this.mouse(e.position, e.button, false, cx)
                }),
            )
            .on_mouse_up(
                gpui::MouseButton::Right,
                cx.listener(|this, e: &gpui::MouseUpEvent, _, cx| {
                    this.mouse(e.position, e.button, false, cx)
                }),
            )
            .on_mouse_up(
                gpui::MouseButton::Middle,
                cx.listener(|this, e: &gpui::MouseUpEvent, _, cx| {
                    this.mouse(e.position, e.button, false, cx)
                }),
            )
            .on_mouse_move(cx.listener(|this, e: &gpui::MouseMoveEvent, _, _| {
                this.pointer_event(e.position, this.buttons);
            }))
            // A drag released outside the picture still has to let go of the
            // button in the VM. Without stopping propagation: the release may
            // belong to a button up in the header.
            .on_mouse_up_out(
                gpui::MouseButton::Left,
                cx.listener(|this, _: &gpui::MouseUpEvent, _, _| {
                    if this.buttons & rfb::BUTTON_LEFT != 0 {
                        this.buttons &= !rfb::BUTTON_LEFT;
                        if let (Some(input), Some((x, y))) = (this.input(), this.pointer) {
                            let _ = input.pointer(x, y, this.buttons);
                        }
                    }
                }),
            )
            .on_scroll_wheel(cx.listener(|this, e: &gpui::ScrollWheelEvent, _, cx| {
                cx.stop_propagation();
                this.scroll(e);
            }));

        let body = div()
            .id("live-screen-lightbox")
            .occlude()
            .track_focus(&self.focus)
            .key_context("LiveScreen")
            .w(viewport.width)
            .h(viewport.height)
            .bg(crate::popover::scrim_alpha(0.78))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(10.0))
            .on_key_down(cx.listener(|this, e: &gpui::KeyDownEvent, _, cx| {
                cx.stop_propagation();
                if this.in_control {
                    this.key(&e.keystroke, true);
                } else if e.keystroke.key == "escape" {
                    this.close(cx);
                }
            }))
            .on_key_up(cx.listener(|this, e: &gpui::KeyUpEvent, _, cx| {
                cx.stop_propagation();
                if this.in_control {
                    this.key(&e.keystroke, false);
                }
            }))
            .child(header)
            .child(screen)
            .child(
                div()
                    .text_size(crate::typography::ui_rems(11.0))
                    .text_color(crate::theme::ink(0.55))
                    .child(hint),
            );
        gpui::deferred(
            gpui::anchored()
                .position(point(px(0.0), px(0.0)))
                .child(body),
        )
        .priority(3)
        .into_any_element()
    }

    fn mouse(
        &mut self,
        position: Point<Pixels>,
        button: gpui::MouseButton,
        down: bool,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        let bit = button_bit(button);
        if down {
            self.buttons |= bit;
        } else {
            self.buttons &= !bit;
        }
        self.pointer_event(position, self.buttons);
    }

    fn scroll(&mut self, event: &gpui::ScrollWheelEvent) {
        if !self.in_control {
            return;
        }
        let (Some(input), Some(bounds)) = (self.input(), self.image_bounds.get()) else {
            return;
        };
        let Some((x, y)) = to_screen(event.position, bounds, self.frame_size) else {
            return;
        };
        let delta = event.delta.pixel_delta(px(16.0));
        let (dy, dx) = (f32::from(delta.y), f32::from(delta.x));
        let (bit, amount) = if dy.abs() >= dx.abs() {
            (
                if dy > 0.0 {
                    rfb::WHEEL_UP
                } else {
                    rfb::WHEEL_DOWN
                },
                dy.abs(),
            )
        } else {
            (
                if dx > 0.0 {
                    rfb::WHEEL_LEFT
                } else {
                    rfb::WHEEL_RIGHT
                },
                dx.abs(),
            )
        };
        let notches = ((amount / 16.0).round() as u32).clamp(1, 5);
        let _ = input.click(x, y, bit, notches);
    }

    fn key(&mut self, stroke: &gpui::Keystroke, down: bool) {
        let Some(input) = self.input() else {
            return;
        };
        let Some((mods, key)) = keysyms(stroke) else {
            return;
        };
        if down {
            for m in &mods {
                let _ = input.key(*m, true);
            }
            let _ = input.key(key, true);
        } else {
            let _ = input.key(key, false);
            for m in mods.iter().rev() {
                let _ = input.key(*m, false);
            }
        }
    }
}

impl Drop for LiveScreen {
    fn drop(&mut self) {
        if self.in_control
            && let Some(dir) = &self.dir
        {
            release_lease(dir);
        }
    }
}

impl Render for LiveScreen {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        for old in self.retired.drain(..) {
            let _ = window.drop_image(old);
        }
        let viewport = window.viewport_size();
        let visible = self.source != Source::None;
        if !visible {
            if self.expanded {
                self.close(cx);
            }
            return div().into_any_element();
        }
        if self.expanded {
            return self.render_lightbox(viewport, cx);
        }
        let dismissed = self.dismissed_at.is_some()
            && self.dismissed_at == Some(self.action.as_ref().map_or(0, |a| a.seq));
        if dismissed {
            return div().into_any_element();
        }
        self.render_card(viewport, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_keeps_the_aspect_and_caps_the_scale() {
        let s = fit((1280, 800), size(px(640.0), px(640.0)), 1.0);
        assert_eq!((f32::from(s.width), f32::from(s.height)), (640.0, 400.0));
        let s = fit((100, 50), size(px(1000.0), px(1000.0)), 2.0);
        assert_eq!((f32::from(s.width), f32::from(s.height)), (200.0, 100.0));
    }

    #[test]
    fn positions_map_to_screen_pixels_and_outside_is_none() {
        let bounds = Bounds::new(point(px(100.0), px(50.0)), size(px(640.0), px(400.0)));
        assert_eq!(
            to_screen(point(px(100.0), px(50.0)), bounds, (1280, 800)),
            Some((0, 0))
        );
        assert_eq!(
            to_screen(point(px(420.0), px(250.0)), bounds, (1280, 800)),
            Some((640, 400))
        );
        assert_eq!(
            to_screen(point(px(99.0), px(60.0)), bounds, (1280, 800)),
            None
        );
        assert_eq!(
            to_screen(point(px(740.0), px(60.0)), bounds, (1280, 800)),
            None
        );
    }

    fn stroke(key: &str, key_char: Option<&str>, modifiers: gpui::Modifiers) -> gpui::Keystroke {
        gpui::Keystroke {
            modifiers,
            key: key.into(),
            key_char: key_char.map(str::to_string),
        }
    }

    #[test]
    fn keystrokes_become_keysyms() {
        let none = gpui::Modifiers::default();
        let shift = gpui::Modifiers {
            shift: true,
            ..Default::default()
        };
        let ctrl = gpui::Modifiers {
            control: true,
            ..Default::default()
        };
        // A shifted character is sent as the character, without shift.
        assert_eq!(
            keysyms(&stroke("a", Some("A"), shift)),
            Some((vec![], 0x41))
        );
        assert_eq!(
            keysyms(&stroke("enter", None, none)),
            Some((vec![], 0xff0d))
        );
        assert_eq!(
            keysyms(&stroke("c", Some("c"), ctrl)),
            Some((vec![0xffe3], 0x63))
        );
        assert_eq!(keysyms(&stroke("f5", None, none)), Some((vec![], 0xffc2)));
        assert_eq!(keysyms(&stroke("unknownkey", None, none)), None);
    }

    #[test]
    fn the_lease_matches_what_the_tool_reads() {
        let lease: serde_json::Value = serde_json::from_str(&lease_json(42, 7)).unwrap();
        assert_eq!(lease["pid"], 42);
        assert_eq!(lease["holder"], "Wizard GUI");
        assert_eq!(lease["since_ms"], 7);
    }

    #[test]
    fn a_lease_is_only_released_by_its_holder() {
        let dir = std::env::temp_dir().join(format!("live-screen-lease-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("control"), lease_json(1, 0)).unwrap();
        release_lease(&dir);
        assert!(dir.join("control").exists(), "pid 1's lease is not ours");
        write_lease(&dir).unwrap();
        release_lease(&dir);
        assert!(!dir.join("control").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn actions_parse_and_caption() {
        let raw = r#"{"seq":3,"at_ms":1,"backend":"vm","action":"type","text":"cat code.txt"}"#;
        let action: Action = serde_json::from_str(raw).unwrap();
        assert_eq!(
            action.caption().unwrap(),
            "typed \u{201c}cat code.txt\u{201d}"
        );
        assert_eq!(action.point(), None);
        let raw = r#"{"seq":4,"at_ms":1,"backend":"vm","action":"left_click","x":5,"y":6}"#;
        let action: Action = serde_json::from_str(raw).unwrap();
        assert_eq!(action.point(), Some((5, 6)));
        assert_eq!(action.caption().unwrap(), "left click");
    }
}
