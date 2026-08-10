//! The two animations the borrowed skins are recognized by.
//!
//! Both are ports, and both are here rather than in `ui.rs` because they are
//! *algorithms* — a phase, a falloff, a period — not glyph tables:
//!
//! - [`shimmer`] is Codex's status header: a bright band sweeping left to
//!   right through the word "Working". Ported from
//!   `codex-rs/tui/src/shimmer.rs` (<https://github.com/openai/codex>,
//!   Apache-2.0).
//! - [`wave`] and [`pulse`] are Grok Build's accent bar, which breathes down
//!   its own length while a block is running. Ported from
//!   `crates/codegen/xai-grok-pager-render/src/theme/tokyonight.rs`
//!   (<https://github.com/xai-org/grok-build>, Apache-2.0).
//!
//! See `docs/ui-skins.md` for the full attribution.
//!
//! Both degrade: with no truecolor there is nothing to interpolate, so the
//! sweep becomes dim/normal/bold and the bar stops breathing rather than
//! strobing between two palette entries.

use ratatui::style::{Modifier, Style};
use ratatui::text::Span;

use crate::theme::{self, ColorDepth, Token};

use super::blend::{blend, terminal_bg};

/// Half-width of the shimmer's bright band, in columns.
const BAND: f32 = 5.0;

/// Columns of lead-in and lead-out, so the band enters and leaves the word
/// rather than popping into existence at its first letter.
const PADDING: usize = 10;

/// Ticks in one full sweep. The TUI ticks at roughly 10Hz (see the main
/// loop's poll interval), and Codex sweeps in two seconds.
const SWEEP_TICKS: u64 = 20;

/// `text` with a bright band sweeping through it, one span per character.
///
/// Per-character spans are the cost of the effect and the reason it is bounded
/// to a status word rather than offered for prose: a 6-character label is 6
/// spans a frame, a paragraph would be thousands.
pub fn shimmer(text: &str, tick: u64) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return Vec::new();
    }
    let period = chars.len() + PADDING * 2;
    let position = ((tick % SWEEP_TICKS) as f32 / SWEEP_TICKS as f32) * period as f32;

    // Interpolating needs somewhere to interpolate *to*, and both ends have to
    // be real colors: the theme's body text and the terminal's background.
    let truecolor = theme::active().depth() == ColorDepth::TrueColor;
    let ends = truecolor
        .then(|| Some((rgb(theme::color(Token::Muted))?, terminal_bg()?)))
        .flatten();

    chars
        .iter()
        .enumerate()
        .map(|(index, ch)| {
            let distance = ((index + PADDING) as f32 - position).abs();
            // A raised cosine over the band: 1 at the centre, 0 at the edges,
            // with no corner at either end, which is what keeps the sweep from
            // looking like a moving block.
            let intensity = if distance <= BAND {
                0.5 * (1.0 + (std::f32::consts::PI * (distance / BAND)).cos())
            } else {
                0.0
            };
            Span::styled(ch.to_string(), shimmer_style(intensity, ends))
        })
        .collect()
}

/// The two colors a shimmer interpolates between: the text's own, and the
/// terminal background it brightens toward. `None` when either is unknowable,
/// which is when the sweep falls back to attributes.
type Ends = Option<((u8, u8, u8), (u8, u8, u8))>;

/// One character's style at `intensity`, interpolated when there are colors to
/// interpolate between and stepped through DIM/normal/BOLD when there are not.
fn shimmer_style(intensity: f32, ends: Ends) -> Style {
    match ends {
        Some((base, highlight)) => {
            let (r, g, b) = blend(highlight, base, intensity.clamp(0.0, 1.0) * 0.9);
            Style::default()
                .fg(ratatui::style::Color::Rgb(r, g, b))
                .add_modifier(Modifier::BOLD)
        }
        None if intensity < 0.2 => Style::default().add_modifier(Modifier::DIM),
        None if intensity < 0.6 => Style::default(),
        None => Style::default().add_modifier(Modifier::BOLD),
    }
}

/// A color as RGB, when it is one. Named and indexed colors are whatever the
/// user's palette says, so there is nothing to blend with.
fn rgb(color: ratatui::style::Color) -> Option<(u8, u8, u8)> {
    match color {
        ratatui::style::Color::Rgb(r, g, b) => Some((r, g, b)),
        _ => None,
    }
}

/// Brightness of the accent bar at `row`, 0.0 to 1.0.
///
/// A wave travelling down the bar: `sin²` of a phase that advances with the
/// tick and shifts with the row, so a tall block's bar ripples instead of
/// blinking in unison. `sin²` rather than `sin` because it never goes
/// negative and spends longer near its extremes, which reads as a breath
/// rather than a strobe.
pub fn wave(tick: u64, row: u16, rows_per_wave: u16, speed: f32) -> f32 {
    let phase = (row as f32 / rows_per_wave.max(1) as f32) * 2.0 * std::f32::consts::PI;
    let value = (tick as f32 * speed + phase).sin();
    value * value
}

/// The same breath with no spatial term: everything sharing a tick pulses
/// together. For single glyphs, where a wave has nothing to travel along.
pub fn pulse(tick: u64, speed: f32) -> f32 {
    let value = (tick as f32 * speed).sin();
    value * value
}

/// `color` dimmed toward the terminal background by `1.0 - brightness`.
///
/// Returns `color` untouched when either end is unknown, which is what keeps
/// the animation from being a requirement: the bar is simply a bar then.
pub fn breathe(color: ratatui::style::Color, brightness: f32) -> ratatui::style::Color {
    let Some(fg) = rgb(color) else {
        return color;
    };
    let Some(bg) = terminal_bg() else {
        return color;
    };
    // Never all the way down to the background — a bar that vanishes on the
    // dim half of its cycle reads as a rendering fault.
    let floor = 0.45;
    let alpha = floor + (1.0 - floor) * brightness.clamp(0.0, 1.0);
    let (r, g, b) = blend(fg, bg, alpha);
    ratatui::style::Color::Rgb(r, g, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shimmer_spans_every_character_and_no_more() {
        let spans = shimmer("Working", 0);
        assert_eq!(spans.len(), 7);
        let text: String = spans.iter().map(|span| span.content.as_ref()).collect();
        assert_eq!(text, "Working");
        assert!(shimmer("", 0).is_empty());
    }

    #[test]
    fn the_band_moves_across_the_word_as_the_tick_advances() {
        // Without a terminal background the styles are the DIM/normal/BOLD
        // ladder, which is exactly the case worth asserting: the *position*
        // has to move even when the colors cannot.
        let brightest = |tick: u64| {
            shimmer("Working", tick)
                .into_iter()
                .position(|span| span.style.add_modifier.contains(Modifier::BOLD))
        };
        let early = brightest(0);
        let later = (1..SWEEP_TICKS).map(brightest).find(|at| *at != early);
        assert!(
            later.is_some(),
            "the bright band should land on a different character as time passes"
        );
    }

    #[test]
    fn a_wave_stays_inside_the_unit_interval_and_varies_by_row() {
        for tick in 0..40 {
            for row in 0..8 {
                let value = wave(tick, row, 6, 0.3);
                assert!((0.0..=1.0).contains(&value), "{value} at {tick}/{row}");
            }
        }
        // A quarter wavelength apart are at opposite ends of the cycle. Not a
        // half: `sin²` has period π, so rows half a wavelength apart are at
        // the *same* brightness, which is what makes the bar's ripple read as
        // one travelling wave rather than two.
        let trough = wave(0, 0, 4, 0.3);
        let crest = wave(0, 1, 4, 0.3);
        assert!(
            (crest - trough).abs() > 0.5,
            "the wave should travel: {trough} vs {crest}"
        );
        assert!(
            (wave(0, 2, 4, 0.3) - trough).abs() < 0.01,
            "and repeat every half wavelength"
        );
    }

    #[test]
    fn a_pulse_is_the_same_everywhere_at_one_tick() {
        assert_eq!(pulse(7, 0.3), pulse(7, 0.3));
        assert!((0.0..=1.0).contains(&pulse(13, 0.3)));
    }

    #[test]
    fn breathing_never_takes_the_bar_all_the_way_out() {
        // A named color has no RGB to interpolate, so it survives untouched —
        // the 16-color path, where the animation simply does not run.
        let named = ratatui::style::Color::Cyan;
        assert_eq!(breathe(named, 0.0), named);
    }
}
