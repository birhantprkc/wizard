//! Terminal-background detection and alpha blending.
//!
//! What this is for: the `codex` and `grok` skins tint blocks — a user message
//! sits on a slightly lighter slab, a tool block on a slightly darker one —
//! and a tint is only meaningful *relative to the background it sits on*. A
//! hard-coded `#2a2a2a` slab is invisible on a dark theme and a black bar on a
//! light one. So a tint here is an **alpha over the terminal's own
//! background**, which is how both upstreams do it.
//!
//! [`blend`] and [`is_light`] are ports of `codex-rs/tui/src/color.rs` from
//! <https://github.com/openai/codex> (Apache-2.0); [`tint`]'s alphas are the
//! ones `style.rs` uses there. See `docs/ui-skins.md` for the full attribution.
//!
//! **When the background is unknown, nothing is tinted.** That is not a
//! fallback, it is the correct answer: Codex itself returns an empty `Style`
//! when it cannot read the terminal background, because a slab blended against
//! a guess is worse than no slab. Wizard's own `minimal`/`wizard` pairing never
//! asks for one in the first place.

use ratatui::style::Color;

/// Environment variable naming the terminal background as `#rrggbb`, for
/// terminals that report nothing else.
///
/// The escape-sequence route (OSC 11) means putting the terminal in raw mode,
/// writing a query, and racing a reply against a timeout on every start — for
/// a cosmetic slab. `COLORFGBG` is already set by a good share of terminals
/// (xterm, konsole, rxvt, and anything that inherits their profile), and this
/// is the explicit override for the rest.
pub const ENV_BG: &str = "WIZARD_BG";

/// The terminal's background color, if it can be known.
///
/// Order: `WIZARD_BG` (an `#rrggbb` the user set), then `COLORFGBG` (which
/// reports palette *indices*, not colors — see [`ansi_rgb`]), then nothing.
pub fn terminal_bg() -> Option<(u8, u8, u8)> {
    if let Ok(raw) = std::env::var(ENV_BG)
        && let Some(rgb) = parse_hex(raw.trim())
    {
        return Some(rgb);
    }
    // `COLORFGBG=15;0` means foreground 15, background 0. Some terminals emit
    // a third field (rxvt puts the cursor color in the middle), so the
    // background is the *last* field, never the second.
    let raw = std::env::var("COLORFGBG").ok()?;
    let last = raw.rsplit(';').next()?.trim();
    let index: u8 = last.parse().ok()?;
    Some(ansi_rgb(index))
}

/// `#rrggbb` (with or without the `#`) as an RGB triple.
fn parse_hex(text: &str) -> Option<(u8, u8, u8)> {
    let hex = text.strip_prefix('#').unwrap_or(text);
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |at: usize| u8::from_str_radix(&hex[at..at + 2], 16).ok();
    Some((byte(0)?, byte(2)?, byte(4)?))
}

/// A representative RGB value for one of the 256 palette slots.
///
/// Only approximate for 0-15, because those sixteen are whatever the user's
/// color scheme says they are and no program can know it — which is the whole
/// reason a tint is computed against this rather than assumed. The xterm
/// defaults are close enough to decide "dark or light", which is all the
/// blend below needs from the low slots.
fn ansi_rgb(index: u8) -> (u8, u8, u8) {
    const BASE: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (128, 0, 0),
        (0, 128, 0),
        (128, 128, 0),
        (0, 0, 128),
        (128, 0, 128),
        (0, 128, 128),
        (192, 192, 192),
        (128, 128, 128),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (0, 0, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    match index {
        0..=15 => BASE[index as usize],
        // The 6×6×6 color cube, at the levels xterm actually uses.
        16..=231 => {
            const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
            let n = index as usize - 16;
            (LEVELS[n / 36], LEVELS[(n / 6) % 6], LEVELS[n % 6])
        }
        // The 24-step grayscale ramp.
        _ => {
            let level = 8 + 10 * (index as u16 - 232);
            let level = level.min(255) as u8;
            (level, level, level)
        }
    }
}

/// Is this background a light one? Rec. 601 luma against the midpoint.
///
/// Ported from `codex-rs/tui/src/color.rs` (openai/codex, Apache-2.0).
pub fn is_light(bg: (u8, u8, u8)) -> bool {
    let (r, g, b) = bg;
    let luma = 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
    luma > 128.0
}

/// `fg` over `bg` at `alpha`, straight alpha, no gamma correction.
///
/// Ported from `codex-rs/tui/src/color.rs` (openai/codex, Apache-2.0).
pub fn blend(fg: (u8, u8, u8), bg: (u8, u8, u8), alpha: f32) -> (u8, u8, u8) {
    let mix = |fg: u8, bg: u8| (fg as f32 * alpha + bg as f32 * (1.0 - alpha)) as u8;
    (mix(fg.0, bg.0), mix(fg.1, bg.1), mix(fg.2, bg.2))
}

/// How far a tint lifts (or drops) off the terminal background.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tint {
    /// A slab a step *toward* the foreground: lighter on a dark terminal,
    /// darker on a light one. Codex's user-message block, Grok Build's
    /// `bg_light`.
    Raised,
    /// A slab a step *away*: Grok Build's `bg_dark`, used behind code and
    /// terminal output.
    Sunken,
}

impl Tint {
    /// The slab color for this tint, in the order that gets it right most
    /// often:
    ///
    /// 1. **Blended against the real background**, when the environment says
    ///    what that is. This is the only route that adapts — the slab lifts
    ///    off a dark terminal and settles onto a light one.
    /// 2. **The theme's declared color** otherwise. Wizard cannot ask the
    ///    terminal directly: that means writing an escape query and blocking
    ///    on the reply, which this codebase removed once already for hanging
    ///    the TUI at startup. So the theme names a value, and it renders.
    /// 3. **Nothing**, when the theme declares `reset` — which is what both
    ///    house themes do, and how the default look keeps its rule of never
    ///    painting a background.
    pub fn resolve(self) -> Option<Color> {
        if let Some(blended) = self.over(terminal_bg()) {
            return Some(blended);
        }
        match crate::theme::color(self.token()) {
            Color::Reset => None,
            declared => Some(declared),
        }
    }

    /// The theme token this tint reads when it cannot blend.
    fn token(self) -> crate::theme::Token {
        match self {
            Tint::Raised => crate::theme::Token::BgRaised,
            Tint::Sunken => crate::theme::Token::BgSunken,
        }
    }

    /// The blended color, or `None` when the terminal background is unknown
    /// and there is therefore nothing honest to blend against.
    pub fn over(self, bg: Option<(u8, u8, u8)>) -> Option<Color> {
        let bg = bg?;
        // The alphas are Codex's: a dark terminal takes a heavier lift than a
        // light one, because the same delta reads as less contrast going up
        // from black than it does going down from white.
        let (toward, alpha) = match (self, is_light(bg)) {
            (Tint::Raised, true) => ((0, 0, 0), 0.04),
            (Tint::Raised, false) => ((255, 255, 255), 0.12),
            (Tint::Sunken, true) => ((0, 0, 0), 0.08),
            (Tint::Sunken, false) => ((0, 0, 0), 0.35),
        };
        let (r, g, b) = blend(toward, bg, alpha);
        Some(Color::Rgb(r, g, b))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blend_at_the_extremes_is_the_endpoints() {
        let black = (0, 0, 0);
        let white = (255, 255, 255);
        assert_eq!(blend(white, black, 0.0), black);
        assert_eq!(blend(white, black, 1.0), white);
    }

    #[test]
    fn luma_decides_light_from_dark_rather_than_the_mean() {
        // Pure green is the case a naive average gets wrong: (0,255,0)
        // averages to 85 and reads "dark", but it is the brightest primary.
        assert!(is_light((0, 255, 0)));
        assert!(!is_light((0, 0, 255)));
        assert!(is_light((255, 255, 255)));
        assert!(!is_light((0, 0, 0)));
    }

    #[test]
    fn a_raised_tint_lifts_off_dark_and_drops_off_light() {
        let dark = Tint::Raised.over(Some((0, 0, 0))).expect("dark tints");
        let Color::Rgb(r, _, _) = dark else {
            panic!("a tint is an rgb blend");
        };
        assert!(r > 0, "a raised slab on black is lighter than black");

        let light = Tint::Raised
            .over(Some((255, 255, 255)))
            .expect("light tints");
        let Color::Rgb(r, _, _) = light else {
            panic!("a tint is an rgb blend");
        };
        assert!(r < 255, "and on white it is darker than white");
    }

    #[test]
    fn an_unknown_background_is_left_alone() {
        // The important one: no guessing. Codex renders no slab in this case
        // and so do we, because a slab blended against an assumption is the
        // one outcome worse than a flat transcript.
        assert_eq!(Tint::Raised.over(None), None);
        assert_eq!(Tint::Sunken.over(None), None);
    }

    #[test]
    fn colorfgbg_reports_a_palette_index_and_the_last_field_is_the_background() {
        // rxvt emits three fields with the cursor color in the middle, so
        // "the second field" is wrong on exactly the terminals that set it.
        assert_eq!(ansi_rgb(0), (0, 0, 0));
        assert_eq!(ansi_rgb(15), (255, 255, 255));
        // The cube's corners and one ramp step, at xterm's levels.
        assert_eq!(ansi_rgb(16), (0, 0, 0));
        assert_eq!(ansi_rgb(231), (255, 255, 255));
        assert_eq!(ansi_rgb(232), (8, 8, 8));
    }

    #[test]
    fn an_explicit_hex_background_parses_with_or_without_the_hash() {
        assert_eq!(parse_hex("#1a1b26"), Some((26, 27, 38)));
        assert_eq!(parse_hex("1a1b26"), Some((26, 27, 38)));
        assert_eq!(parse_hex("#xyzxyz"), None);
        assert_eq!(parse_hex("#1a1b2"), None);
    }
}
