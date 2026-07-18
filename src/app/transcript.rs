//! The chat transcript and the subagent rail: entries, pane state, and the
//! shared scroll/collapse helpers.

use std::time::{Duration, Instant};

use serde_json::Value;

use crate::agent::ImageSource;
use crate::images::ImageRef;

/// One rendered entry in the chat transcript.
#[derive(Debug, Clone)]
pub enum TranscriptEntry {
    User(String),
    Assistant(String),
    /// Model reasoning ("thinking") that preceded an assistant reply,
    /// rendered dimmed.
    Thinking(String),
    /// Collapsible tool invocation card.
    ToolCard {
        name: String,
        args: Value,
        /// `None` while the tool is still running.
        output: Option<String>,
        is_error: bool,
        collapsed: bool,
    },
    /// An image the turn produced — by the model, or by a tool
    /// ([`AgentEvent::Images`](crate::agent::AgentEvent::Images)). The file is already on disk; the entry holds
    /// only the reference to it, and [`crate::ui`] draws it (or, in a terminal
    /// that can draw nothing, prints where it is).
    Image {
        source: ImageSource,
        image: ImageRef,
    },
    /// System notice (mode switch, reload result, errors).
    Notice(String),
}

/// Lifecycle of one subagent run, as shown on its rail dot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneStatus {
    /// The sub-loop is still going.
    Running,
    /// The sub-loop finished on its own and reported back.
    Done,
    /// The run hit its step budget, errored out, or was killed.
    Failed,
}

impl PaneStatus {
    /// The rail's status glyph. Running panes animate via
    /// [`SubagentPane::glyph`]; these are the resting shapes.
    pub fn glyph(self) -> &'static str {
        match self {
            PaneStatus::Running => "●",
            PaneStatus::Done => "✔",
            PaneStatus::Failed => "✗",
        }
    }
}

/// Frames for a running pane's dot, cycled off the app tick so an active
/// subagent visibly pulses on the rail.
const PANE_SPINNER: [&str; 4] = ["●", "◉", "○", "◉"];

/// How long a finished run rests on the rail before it retires: long enough to
/// see it land, short enough that the rail stays a picture of live work. Its
/// report stays in the main chat either way.
pub(super) const PANE_LINGER: Duration = Duration::from_secs(8);

/// One subagent run, surfaced on the rail below the composer and openable as
/// a full chat view.
///
/// This is the faithful record the old transcript-scraping monitor could not
/// build: the subagent's own messages and tool cards, streamed live off the
/// `AgentEvent::SubagentRun*` events and keyed by [`SubagentPane::run`].
#[derive(Debug, Clone)]
pub struct SubagentPane {
    /// Session-unique run id (`agent::subagent::next_run_id`).
    pub run: u64,
    /// Background-registry id, when the run was detached. `None` for a
    /// foreground run — which cannot be killed independently, since the
    /// parent turn is blocked on it.
    pub bg: Option<u32>,
    /// Subagent name (`researcher`, `reviewer`, …).
    pub name: String,
    /// The task it was handed.
    pub task: String,
    pub status: PaneStatus,
    /// The subagent's own conversation, rendered exactly like the main chat.
    pub transcript: Vec<TranscriptEntry>,
    /// Steps (model round-trips) completed so far.
    pub steps: u32,
    pub started: Instant,
    /// Set once the run ends; freezes the elapsed clock on the rail.
    pub finished: Option<Instant>,
    /// Entries appended since the user last had this pane open. Drives the
    /// unread badge, so you can tell which agent did something while you were
    /// looking elsewhere.
    pub unread: usize,
    /// First visible line of the pane transcript, measured from the top of the
    /// rendered content. Only consulted while [`Self::scroll_follow`] is false;
    /// when following, the live tail is always in view.
    pub scroll: u16,
    /// When true the pane sticks to the bottom as new output arrives. Scrolling
    /// up clears it; scrolling back to the bottom (or Ctrl-End) restores it.
    pub scroll_follow: bool,
    /// Last-drawn max scroll for this pane (content lines past the viewport).
    /// Written by the renderer so key handlers can convert a follow-tail view
    /// into a stable top-anchored offset without re-wrapping the transcript.
    pub max_scroll: std::cell::Cell<u16>,
}

impl SubagentPane {
    pub(super) fn new(run: u64, bg: Option<u32>, name: String, task: String) -> Self {
        Self {
            run,
            bg,
            name,
            task,
            status: PaneStatus::Running,
            transcript: Vec::new(),
            steps: 0,
            started: Instant::now(),
            finished: None,
            unread: 0,
            scroll: 0,
            scroll_follow: true,
            max_scroll: std::cell::Cell::new(0),
        }
    }

    /// How long the run has been going, frozen once it ends.
    pub fn elapsed(&self) -> Duration {
        self.finished.unwrap_or_else(Instant::now) - self.started
    }

    /// The rail dot: a pulsing glyph while running, a resting one once done.
    pub fn glyph(&self, tick: u64) -> &'static str {
        match self.status {
            PaneStatus::Running => PANE_SPINNER[(tick / 2) as usize % PANE_SPINNER.len()],
            other => other.glyph(),
        }
    }

    /// One-line summary of what the subagent is doing right now: the tool it
    /// is in the middle of, else its latest message, else the task.
    pub fn activity(&self) -> &str {
        if self.status != PaneStatus::Running {
            return match self.transcript.iter().rev().find_map(|entry| match entry {
                TranscriptEntry::Assistant(text) => Some(text.as_str()),
                _ => None,
            }) {
                Some(text) => text,
                None => self.task.as_str(),
            };
        }
        for entry in self.transcript.iter().rev() {
            match entry {
                // A card still running is the most specific thing to show.
                TranscriptEntry::ToolCard {
                    name, output: None, ..
                } => return name.as_str(),
                TranscriptEntry::Assistant(text) if !text.trim().is_empty() => {
                    return text.as_str();
                }
                _ => {}
            }
        }
        self.task.as_str()
    }
}

/// Whether a finished tool's output is long enough to start collapsed: more
/// than six source lines, or a payload that would wrap well past that (one
/// giant minified line counts as 1 by `lines()` but fills the screen anyway).
pub(super) fn collapse_long(content: &str) -> bool {
    content.lines().count() > 6 || content.chars().count() > 600
}

/// Fill the newest still-open [`TranscriptEntry::ToolCard`] for `name` with
/// `output`. Long successful outputs start collapsed, and so do errors — the
/// ✗ glyph carries the signal without dumping the payload; a click or Ctrl-T
/// expands it. Returns the output back when no open card matched, so the
/// caller can decide what to do with it.
pub(super) fn fill_open_card(
    transcript: &mut [TranscriptEntry],
    name: &str,
    output: crate::tools::ToolOutput,
) -> Option<crate::tools::ToolOutput> {
    let card = transcript.iter_mut().rev().find_map(|entry| match entry {
        TranscriptEntry::ToolCard {
            name: card_name,
            output: slot,
            is_error,
            collapsed,
            ..
        } if *card_name == name && slot.is_none() => Some((slot, is_error, collapsed)),
        _ => None,
    });
    match card {
        Some((slot, is_error, collapsed)) => {
            *is_error = output.is_error;
            *collapsed = output.is_error || collapse_long(&output.content);
            *slot = Some(output.content);
            None
        }
        None => Some(output),
    }
}

/// One step of the shared stick-to-bottom scroll rule. Positive `delta` moves
/// toward older content (up); negative toward the live tail (down). `current`
/// is the stored first-visible-line offset from the top. Returns the new
/// `(scroll, follow)` pair: leaving the bottom clears follow so new output
/// does not yank the view; returning to the bottom re-enables it (and resets
/// the offset to 0).
pub(super) fn scroll_step(follow: bool, current: u16, max: u16, delta: i16) -> (u16, bool) {
    let current = if follow { max } else { current.min(max) };
    // Top-anchored: older content is a smaller start offset.
    let next = if delta >= 0 {
        current.saturating_sub(delta as u16)
    } else {
        current.saturating_add(delta.unsigned_abs()).min(max)
    };
    if next >= max {
        (0, true)
    } else {
        (next, false)
    }
}

/// The images on a replayed message, as the references the live
/// [`AgentEvent::Images`] carried. An image the store could not write has no file
/// to draw and no path to print, so it is not replayed.
pub(super) fn replayed_refs(images: &[crate::llm::Image]) -> Vec<ImageRef> {
    images
        .iter()
        .filter_map(|image| {
            Some(ImageRef {
                path: image.path.clone()?,
                mime: image.mime.clone(),
                bytes: image.decoded_len(),
            })
        })
        .collect()
}

/// The transcript entries an [`AgentEvent::Images`] becomes: one per image, each
/// carrying where it came from and where it landed.
pub(super) fn image_entries(source: &ImageSource, images: Vec<ImageRef>) -> Vec<TranscriptEntry> {
    images
        .into_iter()
        .map(|image| TranscriptEntry::Image {
            source: source.clone(),
            image,
        })
        .collect()
}
