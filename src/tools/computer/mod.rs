//! Native `computer` tool: control the local desktop (mouse, keyboard,
//! screenshots) so a vision-capable model can operate GUI applications —
//! "computer use".
//!
//! The model works in **real screen-pixel coordinates**. A `screenshot`
//! action captures the whole virtual desktop, reports its true pixel size,
//! and returns the image (downscaled only for transport — the coordinate
//! space the model reasons in is always the real screen size, so clicks map
//! back 1:1 without any stateful scaling).
//!
//! Input and capture are delegated to a per-OS [`Backend`]:
//! - **Linux** ([`linux`]): `ydotool` for input (works on Wayland and X11 via
//!   the kernel uinput interface) and `grim`/`maim`/ImageMagick for capture.
//!   Run `wizard desktop-setup` once to install and enable these.
//! - **macOS** ([`macos`]): the CoreGraphics event API (`CGEvent`) for input
//!   and `screencapture` for capture. Requires Accessibility and Screen
//!   Recording permission for the terminal running Wizard.
//!
//! Like `execute`, this is real control of the user's machine — and, like
//! `execute`, nothing gates an individual action. [`ToolAccess::Execute`]
//! here means plan mode refuses the tool, a checkpoint is taken around it,
//! and a read-only subagent is never given it; it is not a prompt, and
//! `ToolAccess`'s own doc says so. There is no per-action approval gate in
//! Wizard, for this tool or any other (see `SECURITY.md`).

pub mod setup;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::llm::Image;

use super::{Tool, ToolAccess, ToolContext, ToolError, ToolOutput, parse_args};

/// Longest-side pixel cap applied to a screenshot before it is sent to the
/// model. Keeps a 4K / multi-monitor capture from ballooning the request;
/// the model is still told the true screen size for coordinates.
const SCREENSHOT_MAX_EDGE: u32 = 1568;

/// A captured screen image plus its true pixel dimensions.
pub(crate) struct Screenshot {
    /// PNG bytes as captured (full resolution).
    pub png: Vec<u8>,
    /// True screen width in pixels (the model's X coordinate range).
    pub width: u32,
    /// True screen height in pixels (the model's Y coordinate range).
    pub height: u32,
}

/// A mouse button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MouseButton {
    Left,
    Right,
    Middle,
}

/// Scroll direction for the `scroll` action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScrollDirection {
    Up,
    Down,
    Left,
    Right,
}

/// Per-OS desktop control. Methods shell out (Linux) or call the platform
/// input API (macOS); they are synchronous and run on a blocking thread.
pub(crate) trait Backend: Send + Sync {
    /// Short human label naming the backend and the tools it uses.
    fn label(&self) -> String;
    /// Capture the whole virtual desktop as PNG.
    fn screenshot(&self) -> anyhow::Result<Screenshot>;
    /// Move the pointer to absolute screen coordinates.
    fn mouse_move(&self, x: i32, y: i32) -> anyhow::Result<()>;
    /// Click `button` `count` times at the current pointer position.
    fn click(&self, button: MouseButton, count: u32) -> anyhow::Result<()>;
    /// Press the left button at the current position, drag to `(x, y)`, release.
    fn drag(&self, x: i32, y: i32) -> anyhow::Result<()>;
    /// Type a Unicode string as keystrokes.
    fn type_text(&self, text: &str) -> anyhow::Result<()>;
    /// Press a key chord like `"Return"`, `"ctrl+c"`, `"cmd+shift+t"`.
    fn key(&self, chord: &str) -> anyhow::Result<()>;
    /// Scroll `amount` notches in `direction` at the current position.
    fn scroll(&self, direction: ScrollDirection, amount: u32) -> anyhow::Result<()>;
    /// Current pointer position, if the platform can report it.
    fn cursor_position(&self) -> anyhow::Result<(i32, i32)>;
}

/// Build the backend for the current OS. Always succeeds (the backend checks
/// its own dependencies per operation); unsupported OSes get a stub whose
/// every method explains the limitation.
pub(crate) fn detect() -> Box<dyn Backend> {
    #[cfg(target_os = "linux")]
    {
        Box::new(linux::LinuxBackend::new())
    }
    #[cfg(target_os = "macos")]
    {
        Box::new(macos::MacosBackend::new())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Box::new(UnsupportedBackend)
    }
}

/// Fallback backend for OSes Wizard does not yet drive (e.g. Windows).
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
struct UnsupportedBackend;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
impl Backend for UnsupportedBackend {
    fn label(&self) -> String {
        "unsupported on this OS".to_string()
    }
    fn screenshot(&self) -> anyhow::Result<Screenshot> {
        anyhow::bail!("the computer tool supports only Linux and macOS")
    }
    fn mouse_move(&self, _x: i32, _y: i32) -> anyhow::Result<()> {
        anyhow::bail!("the computer tool supports only Linux and macOS")
    }
    fn click(&self, _button: MouseButton, _count: u32) -> anyhow::Result<()> {
        anyhow::bail!("the computer tool supports only Linux and macOS")
    }
    fn drag(&self, _x: i32, _y: i32) -> anyhow::Result<()> {
        anyhow::bail!("the computer tool supports only Linux and macOS")
    }
    fn type_text(&self, _text: &str) -> anyhow::Result<()> {
        anyhow::bail!("the computer tool supports only Linux and macOS")
    }
    fn key(&self, _chord: &str) -> anyhow::Result<()> {
        anyhow::bail!("the computer tool supports only Linux and macOS")
    }
    fn scroll(&self, _direction: ScrollDirection, _amount: u32) -> anyhow::Result<()> {
        anyhow::bail!("the computer tool supports only Linux and macOS")
    }
    fn cursor_position(&self) -> anyhow::Result<(i32, i32)> {
        anyhow::bail!("the computer tool supports only Linux and macOS")
    }
}

/// Arguments for [`ComputerTool`]. Only `action` is always required; the rest
/// are validated per action.
#[derive(Debug, Deserialize)]
pub struct ComputerArgs {
    /// The action to perform.
    pub action: String,
    /// X coordinate (real screen pixels). Used by mouse actions.
    #[serde(default)]
    pub x: Option<i32>,
    /// Y coordinate (real screen pixels). Used by mouse actions.
    #[serde(default)]
    pub y: Option<i32>,
    /// `[x, y]` shorthand accepted in place of separate `x`/`y`.
    #[serde(default)]
    pub coordinate: Option<Vec<i32>>,
    /// Text to type (`type`) or the key chord to press (`key`).
    #[serde(default)]
    pub text: Option<String>,
    /// Scroll direction: `up`, `down`, `left`, `right`.
    #[serde(default)]
    pub scroll_direction: Option<String>,
    /// Scroll notches (default 3).
    #[serde(default)]
    pub scroll_amount: Option<u32>,
    /// Seconds to wait for the `wait` action (default 1, max 10).
    #[serde(default)]
    pub duration: Option<f64>,
}

impl ComputerArgs {
    /// Resolve `(x, y)`, accepting either the `coordinate` array or the
    /// separate fields.
    fn xy(&self) -> Option<(i32, i32)> {
        if let Some(c) = &self.coordinate
            && c.len() == 2
        {
            return Some((c[0], c[1]));
        }
        match (self.x, self.y) {
            (Some(x), Some(y)) => Some((x, y)),
            _ => None,
        }
    }
}

/// `computer` — drive the local desktop: screenshot, move/click the mouse,
/// type, press keys, scroll.
pub struct ComputerTool;

impl ComputerTool {
    fn invalid(&self, message: impl Into<String>) -> ToolError {
        ToolError::InvalidArgs {
            tool: "computer".to_string(),
            message: message.into(),
        }
    }

    /// Require `(x, y)` for an action that needs a target.
    fn require_xy(&self, args: &ComputerArgs, action: &str) -> Result<(i32, i32), ToolError> {
        args.xy()
            .ok_or_else(|| self.invalid(format!("action '{action}' requires x and y coordinates")))
    }
}

#[async_trait]
impl Tool for ComputerTool {
    fn name(&self) -> &str {
        "computer"
    }

    fn description(&self) -> &str {
        "Control the local desktop (computer use): take a screenshot, move and click the mouse, \
         type text, press key chords, and scroll. Coordinates are real screen pixels with the \
         origin at the top-left; a 'screenshot' reports the true screen size and returns the \
         image, so call it first to see the screen and after actions to observe the result. \
         Requires desktop control to be set up (run `wizard desktop-setup` on Linux; grant \
         Accessibility + Screen Recording permission on macOS)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": [
                        "screenshot", "mouse_move", "left_click", "right_click",
                        "middle_click", "double_click", "left_click_drag",
                        "type", "key", "scroll", "cursor_position", "wait"
                    ],
                    "description": "The desktop action to perform"
                },
                "x": { "type": "integer", "description": "Target X in screen pixels (mouse actions)" },
                "y": { "type": "integer", "description": "Target Y in screen pixels (mouse actions)" },
                "coordinate": {
                    "type": "array", "items": { "type": "integer" },
                    "description": "[x, y] shorthand for the target, in screen pixels"
                },
                "text": { "type": "string", "description": "Text to type (type), or key chord like 'ctrl+c' / 'Return' (key)" },
                "scroll_direction": { "type": "string", "enum": ["up", "down", "left", "right"], "description": "Scroll direction (scroll)" },
                "scroll_amount": { "type": "integer", "description": "Number of scroll notches (scroll); default 3" },
                "duration": { "type": "number", "description": "Seconds to wait (wait); default 1, max 10" }
            },
            "required": ["action"]
        })
    }

    /// Driving the mouse and keyboard is a side effect on the world, and one
    /// that reaches well past the working directory, so the plan-mode
    /// read-only gate has to refuse it. That is what `Execute` buys, together
    /// with a checkpoint around the call and exclusion from read-only
    /// subagents. It buys no approval prompt: nothing gates an individual
    /// action here or anywhere else in Wizard (see the module header and
    /// `SECURITY.md`).
    fn access(&self) -> ToolAccess {
        ToolAccess::Execute
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: ComputerArgs = parse_args(self.name(), args)?;
        let action = args.action.to_ascii_lowercase();

        // `wait` is handled here (async sleep) rather than in the backend.
        if action == "wait" {
            let secs = args.duration.unwrap_or(1.0).clamp(0.0, 10.0);
            tokio::time::sleep(std::time::Duration::from_secs_f64(secs)).await;
            return Ok(ToolOutput::ok(format!("Waited {secs:.2}s.")));
        }

        // Validate coordinate-bearing actions up front, before the blocking
        // hop, so argument errors surface as `InvalidArgs`.
        let target = match action.as_str() {
            "mouse_move" | "left_click_drag" => Some(self.require_xy(&args, &action)?),
            "left_click" | "right_click" | "middle_click" | "double_click" => args.xy(),
            _ => None,
        };
        let scroll = if action == "scroll" {
            let dir = match args
                .scroll_direction
                .as_deref()
                .map(|s| s.to_ascii_lowercase())
                .as_deref()
            {
                Some("up") => ScrollDirection::Up,
                Some("down") => ScrollDirection::Down,
                Some("left") => ScrollDirection::Left,
                Some("right") => ScrollDirection::Right,
                _ => {
                    return Err(self.invalid(
                        "action 'scroll' requires scroll_direction (up|down|left|right)",
                    ));
                }
            };
            Some((dir, args.scroll_amount.unwrap_or(3).clamp(1, 100)))
        } else {
            None
        };
        let text = args.text.clone();

        // All backend work is synchronous (shell-outs / native API calls), so
        // run it off the async runtime.
        let result = tokio::task::spawn_blocking(move || {
            let backend = detect();
            // Tag failures with the active backend so the model (and user)
            // can see which control path was used.
            run_action(&*backend, &action, target, scroll, text.as_deref())
                .map_err(|err| err.context(format!("backend: {}", backend.label())))
        })
        .await
        .map_err(|err| ToolError::Execution {
            tool: "computer".to_string(),
            source: anyhow::Error::new(err).context("computer action task panicked"),
        })?;

        Ok(match result {
            Ok(output) => output,
            Err(err) => ToolOutput::error(format!("{err:#}")),
        })
    }
}

/// Dispatch one validated action against `backend`. Returns the model-facing
/// [`ToolOutput`] (screenshots attach the captured image).
fn run_action(
    backend: &dyn Backend,
    action: &str,
    target: Option<(i32, i32)>,
    scroll: Option<(ScrollDirection, u32)>,
    text: Option<&str>,
) -> anyhow::Result<ToolOutput> {
    match action {
        "screenshot" => capture(backend),
        "mouse_move" => {
            let (x, y) = target.expect("validated");
            backend.mouse_move(x, y)?;
            Ok(ToolOutput::ok(format!("Moved pointer to ({x}, {y}).")))
        }
        "left_click" | "right_click" | "middle_click" | "double_click" => {
            let button = match action {
                "right_click" => MouseButton::Right,
                "middle_click" => MouseButton::Middle,
                _ => MouseButton::Left,
            };
            let count = if action == "double_click" { 2 } else { 1 };
            if let Some((x, y)) = target {
                backend.mouse_move(x, y)?;
            }
            backend.click(button, count)?;
            let where_ = target
                .map(|(x, y)| format!(" at ({x}, {y})"))
                .unwrap_or_default();
            Ok(ToolOutput::ok(format!("{action}{where_}.")))
        }
        "left_click_drag" => {
            let (x, y) = target.expect("validated");
            backend.drag(x, y)?;
            Ok(ToolOutput::ok(format!("Dragged to ({x}, {y}).")))
        }
        "type" => {
            let text = text.ok_or_else(|| anyhow::anyhow!("action 'type' requires text"))?;
            backend.type_text(text)?;
            Ok(ToolOutput::ok(format!(
                "Typed {} characters.",
                text.chars().count()
            )))
        }
        "key" => {
            let chord =
                text.ok_or_else(|| anyhow::anyhow!("action 'key' requires text (the chord)"))?;
            backend.key(chord)?;
            Ok(ToolOutput::ok(format!("Pressed '{chord}'.")))
        }
        "scroll" => {
            let (dir, amount) = scroll.expect("validated");
            backend.scroll(dir, amount)?;
            Ok(ToolOutput::ok(format!("Scrolled {dir:?} x{amount}.")))
        }
        "cursor_position" => {
            let (x, y) = backend.cursor_position()?;
            Ok(ToolOutput::ok(format!("Cursor at ({x}, {y}).")))
        }
        other => Err(anyhow::anyhow!("unknown action '{other}'")),
    }
}

/// Take a screenshot, downscale for transport, and package it as a tool
/// output carrying the base64 PNG plus a text note giving the real screen
/// size (the model's coordinate space).
fn capture(backend: &dyn Backend) -> anyhow::Result<ToolOutput> {
    let shot = backend.screenshot()?;
    let (w, h) = (shot.width, shot.height);
    let delivered = downscale_png(shot.png, SCREENSHOT_MAX_EDGE);
    // `from_bytes` sniffs the media type and enforces the shared image size
    // cap, so an oversized capture is refused here rather than by the provider
    // mid-request. `downscale_png` has already brought the common cases under
    // it; a display large enough to still exceed the cap surfaces as a tool
    // error naming the byte count.
    let image = Image::from_bytes(&delivered)?;
    let note = format!(
        "Screenshot captured. Screen size is {w}x{h} pixels — give click coordinates in that \
         space (top-left origin). The image may be downscaled for transport; reason about \
         positions proportionally."
    );
    Ok(ToolOutput {
        content: note,
        is_error: false,
        images: vec![image],
    })
}

/// Read width/height from a PNG's IHDR chunk without decoding the image.
/// Returns `None` if the bytes are not a PNG.
pub(crate) fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    const SIG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    if bytes.len() < 24 || bytes[..8] != SIG {
        return None;
    }
    // IHDR data begins at offset 16: width (4 bytes BE) then height (4 bytes BE).
    let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    Some((width, height))
}

/// Best-effort downscale of a PNG so its longest edge is at most `max_edge`,
/// via ImageMagick (`magick` or `convert`). The `>` geometry flag only shrinks
/// images larger than the box, so smaller captures pass through untouched. Any
/// failure (no ImageMagick, decode error) returns the original bytes — the
/// screenshot is still usable, just larger.
fn downscale_png(png: Vec<u8>, max_edge: u32) -> Vec<u8> {
    match png_dimensions(&png) {
        Some((w, h)) if w <= max_edge && h <= max_edge => return png,
        None => return png,
        _ => {}
    }
    let geometry = format!("{max_edge}x{max_edge}>");
    for bin in ["magick", "convert"] {
        if let Ok(out) = run_capture_stdin(bin, &["png:-", "-resize", &geometry, "png:-"], &png)
            && png_dimensions(&out).is_some()
        {
            return out;
        }
    }
    png
}

/// Run `bin args...`, feed `input` on stdin, and return stdout bytes on a
/// zero exit. Shared by the capture/downscale paths and the Linux backend.
pub(crate) fn run_capture_stdin(bin: &str, args: &[&str], input: &[u8]) -> anyhow::Result<Vec<u8>> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = Command::new(bin)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| anyhow::anyhow!("could not run '{bin}': {err}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(input)
            .map_err(|err| anyhow::anyhow!("writing to '{bin}' stdin: {err}"))?;
    }
    let out = child
        .wait_with_output()
        .map_err(|err| anyhow::anyhow!("waiting on '{bin}': {err}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "'{bin}' exited {}: {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn png_dimensions_reads_ihdr() {
        // Minimal 1x1 PNG header: signature + IHDR length/type + 1x1 dims.
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        bytes.extend_from_slice(&[0, 0, 0, 13]); // IHDR length
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&7040u32.to_be_bytes());
        bytes.extend_from_slice(&1440u32.to_be_bytes());
        bytes.extend_from_slice(&[8, 2, 0, 0, 0]); // bit depth/color/etc.
        assert_eq!(png_dimensions(&bytes), Some((7040, 1440)));
    }

    #[test]
    fn png_dimensions_rejects_non_png() {
        assert_eq!(png_dimensions(b"not a png at all...."), None);
        assert_eq!(png_dimensions(&[]), None);
    }

    /// The documentation about this tool says what the code does.
    ///
    /// Three places claimed `computer` "opts into the approval gate" and that
    /// "the surfaces prompt before it runs". No such gate exists: every
    /// `.access()` call site drives plan mode, checkpointing and read-only
    /// subagent scoping, and `ToolAccess`'s own doc says "never prompting".
    /// Of everything Wizard ships this is the tool where an imagined
    /// safeguard costs the most — it moves the reader's mouse and types on
    /// their keyboard — so the claim is asserted rather than trusted.
    #[test]
    fn the_docs_do_not_promise_an_approval_gate_this_tool_does_not_have() {
        assert_eq!(
            ComputerTool.access(),
            ToolAccess::Execute,
            "if this ever stops being execute-class the docs below are wrong too"
        );

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let guide = std::fs::read_to_string(root.join("docs/computer-use.md"))
            .expect("read docs/computer-use.md");
        assert!(
            guide.contains("There is no per-action approval gate"),
            "docs/computer-use.md has to say the gate does not exist"
        );

        // README.md and SECURITY.md already state the general rule; the threat
        // model additionally has to list this tool among the ungated ones,
        // which it did not, though it enumerates every other one.
        let security = std::fs::read_to_string(root.join("SECURITY.md")).expect("read SECURITY.md");
        let (ungated, _) = security
            .split_once("Read-only tools include")
            .expect("SECURITY.md lists the state-changing tools before the read-only ones");
        assert!(
            ungated.contains("`computer`"),
            "SECURITY.md's state-changing list has to name `computer`"
        );

        // This module's own header made the same claim. Everything before the
        // test module, so the phrase quoted here is not what is matched.
        let (production, _) = include_str!("mod.rs")
            .split_once("#[cfg(test)]\nmod tests {")
            .expect("this module ends with its test module");
        let changelog =
            std::fs::read_to_string(root.join("CHANGELOG.md")).expect("read CHANGELOG.md");
        // Both wordings, because fixing the first one on the module header
        // left the second alive on `access()` — a grep guard that matches one
        // phrasing of a claim protects only that phrasing.
        for doc in [production, guide.as_str(), changelog.as_str()] {
            for claim in ["opts into the approval gate", "must prompt before it runs"] {
                assert!(
                    !doc.contains(claim),
                    "no document may claim an approval gate that is not implemented: {claim:?}"
                );
            }
        }
    }

    #[test]
    fn xy_prefers_coordinate_array() {
        let args = ComputerArgs {
            action: "left_click".into(),
            x: Some(1),
            y: Some(2),
            coordinate: Some(vec![10, 20]),
            text: None,
            scroll_direction: None,
            scroll_amount: None,
            duration: None,
        };
        assert_eq!(args.xy(), Some((10, 20)));
    }

    #[test]
    fn xy_falls_back_to_fields_and_requires_both() {
        let mut args = ComputerArgs {
            action: "mouse_move".into(),
            x: Some(5),
            y: Some(6),
            coordinate: None,
            text: None,
            scroll_direction: None,
            scroll_amount: None,
            duration: None,
        };
        assert_eq!(args.xy(), Some((5, 6)));
        args.y = None;
        assert_eq!(args.xy(), None);
    }

    #[tokio::test]
    async fn missing_coordinates_for_mouse_move_is_invalid_args() {
        let ctx = ToolContext::new(std::env::temp_dir());
        let err = ComputerTool
            .execute(json!({ "action": "mouse_move" }), &ctx)
            .await
            .expect_err("mouse_move without coords must be rejected");
        assert!(matches!(err, ToolError::InvalidArgs { tool, .. } if tool == "computer"));
    }

    #[tokio::test]
    async fn scroll_without_direction_is_invalid_args() {
        let ctx = ToolContext::new(std::env::temp_dir());
        let err = ComputerTool
            .execute(json!({ "action": "scroll" }), &ctx)
            .await
            .expect_err("scroll without direction must be rejected");
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn wait_action_returns_without_a_backend() {
        let ctx = ToolContext::new(std::env::temp_dir());
        let out = ComputerTool
            .execute(json!({ "action": "wait", "duration": 0.01 }), &ctx)
            .await
            .expect("wait runs");
        assert!(!out.is_error);
        assert!(out.content.contains("Waited"));
    }

    #[test]
    fn computer_tool_is_execute_access() {
        assert_eq!(ComputerTool.access(), ToolAccess::Execute);
        assert_eq!(ComputerTool.name(), "computer");
    }

    /// Full screenshot path: capture → downscale → base64 → tool output with
    /// an attached image. Needs a live display, so it is ignored by default;
    /// run with `cargo test -- --ignored screenshot_action`.
    #[tokio::test]
    #[ignore = "requires a live display server"]
    async fn screenshot_action_returns_a_valid_png_image() {
        let ctx = ToolContext::new(std::env::temp_dir());
        let out = ComputerTool
            .execute(json!({ "action": "screenshot" }), &ctx)
            .await
            .expect("screenshot executes");
        assert!(!out.is_error, "screenshot failed: {}", out.content);
        assert_eq!(out.images.len(), 1, "exactly one image attached");
        assert!(out.content.contains("Screen size"));
        assert_eq!(out.images[0].mime, "image/png", "tagged as PNG");
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&out.images[0].b64)
            .expect("image is valid base64");
        assert!(
            png_dimensions(&bytes).is_some(),
            "attached image decodes to a PNG"
        );
    }
}
