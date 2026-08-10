//! Linux desktop backend: `ydotool` (kernel uinput) for input and
//! `grim`/`maim`/ImageMagick for screen capture. Works on both Wayland and
//! X11 because uinput sits below the display server. Set up with
//! `wizard desktop-setup`.

use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};

use super::{Backend, MouseButton, Screenshot, ScrollDirection, png_dimensions};

/// Linux backend. Stateless — every method shells out fresh.
pub(crate) struct LinuxBackend;

impl LinuxBackend {
    pub(crate) fn new() -> Self {
        Self
    }

    /// Run `ydotool` with `args`, turning a missing binary or a stopped daemon
    /// into an actionable message pointing at `wizard desktop-setup`.
    fn ydotool(&self, args: &[&str]) -> Result<()> {
        if which("ydotool").is_none() {
            bail!(
                "`ydotool` is not installed. Run `wizard desktop-setup` to install and enable \
                 desktop control."
            );
        }
        let out = Command::new("ydotool")
            .args(args)
            .output()
            .with_context(|| format!("spawning ydotool {}", args.join(" ")))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if stderr.contains("socket") || stderr.contains("ydotoold") {
                bail!(
                    "the ydotoold daemon is not running ({}). Start it with \
                     `systemctl --user start ydotoold` (or run `wizard desktop-setup`).",
                    stderr.trim()
                );
            }
            bail!("ydotool failed: {}", stderr.trim());
        }
        Ok(())
    }
}

impl Backend for LinuxBackend {
    fn label(&self) -> String {
        let server = if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            "Wayland"
        } else if std::env::var_os("DISPLAY").is_some() {
            "X11"
        } else {
            "no display server detected"
        };
        format!("linux ({server}): ydotool input, grim/maim/import capture")
    }

    fn screenshot(&self) -> Result<Screenshot> {
        let png = capture_screen()?;
        let (width, height) = png_dimensions(&png)
            .ok_or_else(|| anyhow!("screen capture did not return a valid PNG"))?;
        Ok(Screenshot { png, width, height })
    }

    fn mouse_move(&self, x: i32, y: i32) -> Result<()> {
        self.ydotool(&["mousemove", "--absolute", &x.to_string(), &y.to_string()])
    }

    fn click(&self, button: MouseButton, count: u32) -> Result<()> {
        // ydotool button codes: 0xC0 left, 0xC1 right, 0xC2 middle (the 0xC0
        // bit pattern means press+release).
        let code = match button {
            MouseButton::Left => "0xC0",
            MouseButton::Right => "0xC1",
            MouseButton::Middle => "0xC2",
        };
        let count = count.max(1).to_string();
        self.ydotool(&["click", "--repeat", &count, code])
    }

    fn drag(&self, x: i32, y: i32) -> Result<()> {
        // Left button down (0x40), move, left button up (0x80).
        self.ydotool(&["click", "0x40"])?;
        self.mouse_move(x, y)?;
        self.ydotool(&["click", "0x80"])
    }

    fn type_text(&self, text: &str) -> Result<()> {
        self.ydotool(&["type", "--", text])
    }

    fn key(&self, chord: &str) -> Result<()> {
        let (mods, key) = parse_chord(chord)?;
        // Press modifiers, press+release the key, release modifiers (reverse).
        let mut events: Vec<String> = Vec::new();
        for m in &mods {
            events.push(format!("{m}:1"));
        }
        events.push(format!("{key}:1"));
        events.push(format!("{key}:0"));
        for m in mods.iter().rev() {
            events.push(format!("{m}:0"));
        }
        let mut args = vec!["key".to_string()];
        args.extend(events);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.ydotool(&refs)
    }

    fn scroll(&self, direction: ScrollDirection, amount: u32) -> Result<()> {
        // ydotool 1.0 has no wheel command, so approximate with arrow keys —
        // the common effect for scrollable views. Reported as a scroll by the
        // caller; documented as an approximation.
        let key = match direction {
            ScrollDirection::Up => "Up",
            ScrollDirection::Down => "Down",
            ScrollDirection::Left => "Left",
            ScrollDirection::Right => "Right",
        };
        for _ in 0..amount.max(1) {
            self.key(key)?;
        }
        Ok(())
    }

    fn cursor_position(&self) -> Result<(i32, i32)> {
        // Hyprland can report it; most other Wayland compositors cannot.
        if which("hyprctl").is_some() {
            let out = Command::new("hyprctl")
                .arg("cursorpos")
                .output()
                .context("running hyprctl cursorpos")?;
            if out.status.success() {
                let text = String::from_utf8_lossy(&out.stdout);
                if let Some((x, y)) = text.trim().split_once(',') {
                    let x = x.trim().parse::<i32>().ok();
                    let y = y.trim().parse::<i32>().ok();
                    if let (Some(x), Some(y)) = (x, y) {
                        return Ok((x, y));
                    }
                }
            }
        }
        bail!("cursor position is not queryable on this compositor")
    }
}

/// Capture the whole screen as PNG, trying capture tools appropriate to the
/// session type (Wayland first if present, then X11), with universal
/// fallbacks. Returns the bytes of the first tool that produces a valid PNG.
fn capture_screen() -> Result<Vec<u8>> {
    // (binary, args) candidates in priority order.
    let mut candidates: Vec<(&str, Vec<&str>)> = Vec::new();
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        candidates.push(("grim", vec!["-"]));
    }
    if std::env::var_os("DISPLAY").is_some() {
        candidates.push(("maim", vec![]));
        candidates.push(("import", vec!["-window", "root", "png:-"]));
    }
    // Universal fallbacks regardless of detected session.
    candidates.push(("grim", vec!["-"]));
    candidates.push(("import", vec!["-window", "root", "png:-"]));

    let mut tried = Vec::new();
    let mut last_err = String::new();
    for (bin, args) in candidates {
        if tried.contains(&bin) || which(bin).is_none() {
            continue;
        }
        tried.push(bin);
        match Command::new(bin).args(&args).output() {
            Ok(out) if out.status.success() && png_dimensions(&out.stdout).is_some() => {
                return Ok(out.stdout);
            }
            Ok(out) => {
                last_err = format!("{bin}: {}", String::from_utf8_lossy(&out.stderr).trim());
            }
            Err(err) => last_err = format!("{bin}: {err}"),
        }
    }

    if tried.is_empty() {
        bail!(
            "no screenshot tool found (need grim on Wayland, or maim/ImageMagick on X11). \
             Run `wizard desktop-setup`."
        );
    }
    bail!(
        "screen capture failed (tried: {}). Last error: {last_err}",
        tried.join(", ")
    )
}

/// Locate `bin` on `PATH`. Returns `None` when absent.
fn which(bin: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(bin))
        .find(|candidate| candidate.is_file())
}

/// Parse a key chord like `"ctrl+c"`, `"Return"`, `"cmd+shift+t"` into a list
/// of modifier evdev codes and the main-key evdev code.
fn parse_chord(chord: &str) -> Result<(Vec<u16>, u16)> {
    let mut mods = Vec::new();
    let mut main = None;
    for token in chord.split('+').map(str::trim).filter(|t| !t.is_empty()) {
        if let Some(code) = modifier_code(token) {
            mods.push(code);
        } else if let Some(code) = evdev_code(token) {
            if main.is_some() {
                bail!("chord '{chord}' has more than one non-modifier key");
            }
            main = Some(code);
        } else {
            bail!("unknown key '{token}' in chord '{chord}'");
        }
    }
    let main = main.ok_or_else(|| anyhow!("chord '{chord}' has no main key"))?;
    Ok((mods, main))
}

/// Map a modifier name (case-insensitive) to its left-side evdev code.
fn modifier_code(name: &str) -> Option<u16> {
    Some(match name.to_ascii_lowercase().as_str() {
        "ctrl" | "control" => 29,                            // KEY_LEFTCTRL
        "shift" => 42,                                       // KEY_LEFTSHIFT
        "alt" | "option" | "opt" => 56,                      // KEY_LEFTALT
        "meta" | "super" | "win" | "cmd" | "command" => 125, // KEY_LEFTMETA
        _ => return None,
    })
}

/// Map a key name (case-insensitive) to a Linux evdev keycode (a subset of
/// `linux/input-event-codes.h`).
fn evdev_code(name: &str) -> Option<u16> {
    let lower = name.to_ascii_lowercase();
    // Single printable characters map by name.
    let named = match lower.as_str() {
        "return" | "enter" => 28,
        "tab" => 15,
        "space" | "spacebar" => 57,
        "backspace" => 14,
        "delete" | "del" => 111,
        "escape" | "esc" => 1,
        "up" => 103,
        "down" => 108,
        "left" => 105,
        "right" => 106,
        "home" => 102,
        "end" => 107,
        "pageup" | "pgup" | "prior" => 104,
        "pagedown" | "pgdn" | "next" => 109,
        "insert" | "ins" => 110,
        "minus" | "-" => 12,
        "equal" | "=" => 13,
        "comma" | "," => 51,
        "period" | "dot" | "." => 52,
        "slash" | "/" => 53,
        "backslash" | "\\" => 43,
        "semicolon" | ";" => 39,
        "apostrophe" | "'" => 40,
        "grave" | "`" => 41,
        "leftbracket" | "[" => 26,
        "rightbracket" | "]" => 27,
        "capslock" => 58,
        _ => 0,
    };
    if named != 0 {
        return Some(named);
    }
    // Letters a-z.
    if lower.len() == 1 {
        let c = lower.as_bytes()[0];
        if c.is_ascii_lowercase() {
            const LETTERS: [u16; 26] = [
                30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22,
                47, 17, 45, 21, 44,
            ];
            return Some(LETTERS[(c - b'a') as usize]);
        }
        // Digits 0-9.
        if c.is_ascii_digit() {
            const DIGITS: [u16; 10] = [11, 2, 3, 4, 5, 6, 7, 8, 9, 10];
            return Some(DIGITS[(c - b'0') as usize]);
        }
    }
    // Function keys F1-F12.
    if let Some(rest) = lower.strip_prefix('f')
        && let Ok(n) = rest.parse::<u16>()
        && (1..=12).contains(&n)
    {
        return Some(if n <= 10 { 58 + n } else { 76 + n }); // F1=59..F10=68, F11=87, F12=88
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_chord() {
        let (mods, key) = parse_chord("ctrl+c").unwrap();
        assert_eq!(mods, vec![29]);
        assert_eq!(key, 46); // KEY_C
    }

    #[test]
    fn parses_multi_modifier_chord() {
        let (mods, key) = parse_chord("cmd+shift+t").unwrap();
        assert_eq!(mods, vec![125, 42]);
        assert_eq!(key, 20); // KEY_T
    }

    #[test]
    fn parses_named_keys_case_insensitively() {
        assert_eq!(parse_chord("Return").unwrap().1, 28);
        assert_eq!(parse_chord("ESC").unwrap().1, 1);
        assert_eq!(parse_chord("PageDown").unwrap().1, 109);
    }

    #[test]
    fn function_keys_map_across_the_gap() {
        assert_eq!(evdev_code("f1"), Some(59));
        assert_eq!(evdev_code("f10"), Some(68));
        assert_eq!(evdev_code("f11"), Some(87));
        assert_eq!(evdev_code("f12"), Some(88));
        assert_eq!(evdev_code("f13"), None);
    }

    #[test]
    fn digits_and_letters_map() {
        assert_eq!(evdev_code("a"), Some(30));
        assert_eq!(evdev_code("z"), Some(44));
        assert_eq!(evdev_code("0"), Some(11));
        assert_eq!(evdev_code("9"), Some(10));
    }

    #[test]
    fn rejects_unknown_and_double_main_keys() {
        assert!(parse_chord("ctrl+zzz").is_err());
        assert!(parse_chord("a+b").is_err());
        assert!(parse_chord("ctrl").is_err());
    }

    /// Exercises the real capture path. Needs a live Wayland/X11 session, so
    /// it is ignored by default; run with
    /// `cargo test -- --ignored captures_a_real_screenshot`.
    #[test]
    #[ignore = "requires a live display server"]
    fn captures_a_real_screenshot() {
        let png = capture_screen().expect("capture a screenshot");
        let (w, h) = png_dimensions(&png).expect("valid PNG");
        assert!(w > 0 && h > 0, "screen is {w}x{h}");
    }
}
