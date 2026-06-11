//! Progress reporting for Wizard's plain-terminal surfaces.
//!
//! The interactive genie TUI draws its own spinner (`src/ui.rs`); everything
//! here covers the paths that print to a normal terminal instead — bench
//! replays, headless sovereign turns, and llama-server startup waits — with
//! one shared look: the TUI's braille frames, white accent, dim text.
//!
//! All drawing goes to stderr and indicatif hides itself when stderr is not
//! a terminal, so piped or redirected runs see no escape sequences; status
//! that matters on non-terminals (server wait results) falls back to plain
//! lines.

use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};

/// Braille frames matching the TUI spinner in `src/ui.rs`, plus a final
/// check mark shown when a spinner finishes with a message.
const TICK_CHARS: &str = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏✓";

/// Steady-tick cadence, close to the TUI's redraw feel.
const TICK_INTERVAL: Duration = Duration::from_millis(80);

/// Spinner style: accent (white) spinner, dim message — the plain-terminal
/// cousin of the TUI busy line.
fn spinner_style() -> ProgressStyle {
    ProgressStyle::with_template("{spinner:.white} {msg:.dim}")
        .expect("static spinner template is valid")
        .tick_chars(TICK_CHARS)
}

/// Counted-bar style (bench cases): position/length, current item, elapsed.
fn bar_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{spinner:.white} [{bar:24.white/black}] {pos}/{len} {msg:.dim} {elapsed:.dim}",
    )
    .expect("static bar template is valid")
    .tick_chars(TICK_CHARS)
    .progress_chars("█▓░")
}

/// A counted progress bar over `len` items, drawn to stderr (hidden
/// automatically when stderr is not a terminal). Wrap interleaved output in
/// [`ProgressBar::suspend`] so lines land above the bar.
pub fn bar(len: u64) -> ProgressBar {
    let bar = ProgressBar::new(len).with_style(bar_style());
    bar.enable_steady_tick(TICK_INTERVAL);
    bar
}

/// The headless-turn spinner: shows a configured verb ("Conjuring…") while
/// the model is thinking or a tool is running, and gets out of the way while
/// the agent streams real output. Shared between the run loop and the
/// event-printer task, so all state is interior and methods take `&self`.
pub struct TurnSpinner {
    bar: ProgressBar,
    /// Whether stderr is a terminal at all. When false the spinner is a
    /// permanent no-op (no ticker thread, no escapes) and lines print
    /// plainly.
    enabled: bool,
    visible: AtomicBool,
}

impl TurnSpinner {
    /// Create a hidden spinner; [`TurnSpinner::show`] makes it visible.
    pub fn new() -> Self {
        let bar = ProgressBar::with_draw_target(None, ProgressDrawTarget::hidden())
            .with_style(spinner_style());
        Self {
            bar,
            enabled: std::io::stderr().is_terminal(),
            visible: AtomicBool::new(false),
        }
    }

    /// Set the verb shown next to the spinner (rendered as "{verb}…").
    pub fn set_verb(&self, verb: &str) {
        self.bar.set_message(format!("{verb}…"));
    }

    /// Show the spinner. No-op when stderr is not a terminal.
    pub fn show(&self) {
        if !self.enabled || self.visible.swap(true, Ordering::SeqCst) {
            return;
        }
        self.bar.set_draw_target(ProgressDrawTarget::stderr());
        self.bar.enable_steady_tick(TICK_INTERVAL);
    }

    /// Hide the spinner, clearing it from the terminal (streamed model
    /// output owns the screen until the next [`TurnSpinner::show`]).
    pub fn hide(&self) {
        if !self.enabled || !self.visible.swap(false, Ordering::SeqCst) {
            return;
        }
        self.bar.disable_steady_tick();
        self.bar.set_draw_target(ProgressDrawTarget::hidden());
    }

    /// Print a full line to stdout without tearing a visible spinner.
    pub fn println(&self, line: &str) {
        if self.visible.load(Ordering::SeqCst) {
            self.bar.suspend(|| println!("{line}"));
        } else {
            println!("{line}");
        }
    }

    /// Clear the spinner for good at the end of a run.
    pub fn finish(&self) {
        self.visible.store(false, Ordering::SeqCst);
        self.bar.finish_and_clear();
    }
}

impl Default for TurnSpinner {
    fn default() -> Self {
        Self::new()
    }
}

/// Progress reporter for llama-server startup (spawn + model load): a
/// spinner whose message tracks the latest status on a terminal, plain
/// stdout lines otherwise (matching the previous behavior for scripts).
pub struct ServerSpinner {
    bar: ProgressBar,
    /// Whether any status line arrived — i.e. the server was actually
    /// spawned or waited on rather than already answering.
    waited: AtomicBool,
}

impl ServerSpinner {
    /// Start the spinner. The fast path (server already ready) finishes it
    /// before a status ever lands, leaving no output.
    pub fn start() -> Self {
        let bar = ProgressBar::new_spinner()
            .with_style(spinner_style())
            .with_message("Starting llama-server…");
        bar.enable_steady_tick(TICK_INTERVAL);
        Self {
            bar,
            waited: AtomicBool::new(false),
        }
    }

    /// Report a status line ("waiting for the model to load…").
    pub fn update(&self, line: &str) {
        self.waited.store(true, Ordering::SeqCst);
        if self.bar.is_hidden() {
            println!("{line}");
        } else {
            self.bar.set_message(line.to_string());
        }
    }

    /// Finish the wait. A successful wait that actually had to spawn or
    /// poll leaves a final "llama-server ready" status (plain println on
    /// non-terminals); the fast path and failures clear silently — errors
    /// are reported by the caller.
    pub fn finish(&self, ok: bool) {
        if ok && self.waited.load(Ordering::SeqCst) {
            if self.bar.is_hidden() {
                self.bar.finish_and_clear();
                println!("llama-server ready");
            } else {
                self.bar.finish_with_message("llama-server ready");
            }
        } else {
            self.bar.finish_and_clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::UiConfig;

    #[test]
    fn styles_construct_without_panicking() {
        let _ = spinner_style();
        let _ = bar_style();
    }

    #[test]
    fn bar_counts_and_finishes() {
        let bar = bar(3);
        bar.set_message("case-1");
        bar.inc(1);
        bar.suspend(|| {});
        assert_eq!(bar.position(), 1);
        // finish_and_clear advances the bar to its length.
        bar.finish_and_clear();
        assert_eq!(bar.position(), 3);
        assert!(bar.is_finished());
    }

    #[test]
    fn turn_spinner_lifecycle_is_safe_off_terminal() {
        // Under `cargo test` stderr is captured (not a terminal), so every
        // call must be a quiet no-op rather than escape-sequence garbage.
        let spinner = TurnSpinner::new();
        spinner.set_verb("Conjuring");
        spinner.show();
        spinner.hide();
        spinner.show();
        spinner.finish();
    }

    #[test]
    fn turn_spinner_accepts_configured_verbs() {
        // The headless path feeds verbs from the same UiConfig mechanism the
        // TUI uses; any verb from any seed must be representable.
        let ui = UiConfig::default();
        let spinner = TurnSpinner::new();
        for seed in 0..16 {
            spinner.set_verb(ui.spinner_verb(seed));
        }
        spinner.finish();
    }

    #[test]
    fn server_spinner_is_silent_without_a_wait() {
        let wait = ServerSpinner::start();
        assert!(!wait.waited.load(Ordering::SeqCst));
        wait.finish(true); // fast path: no "ready" line, just a clear

        let wait = ServerSpinner::start();
        wait.update("waiting for the model to load…");
        assert!(wait.waited.load(Ordering::SeqCst));
        wait.finish(false); // failure: cleared, the caller prints the error
    }
}
