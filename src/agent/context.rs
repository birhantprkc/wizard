//! Context-window stewardship over a bare message history: how full the next
//! call's prompt is, and what to drop when it is too full.
//!
//! Nothing here knows about [`Agent`](super::Agent). That is the point. The
//! parent turn loop and [`subagent::spawn`](super::subagent::spawn) both run
//! for as long as the model keeps calling tools, and both therefore outgrow the
//! window the same way; until this module existed only the first of them could
//! do anything about it, so a long sub-run walked off the end of its context
//! and came back as a provider error. `/ultra` made that N sub-runs per turn.
//!
//! Two things are shared, and they are separate on purpose: [`pressure`]
//! *measures* and never mutates, because it is also what feeds the live signal
//! the model reads each step; [`compact`] cuts, and is the only thing that
//! does.
//!
//! # What differs between a conversation and a sub-loop
//!
//! One thing: where the kept tail is allowed to begin (see [`Anchor`]). Every
//! other rule — the reserve of recent messages, the rolling summary, the
//! fallback to truncation — is the same, because the constraint behind them
//! (a request the provider will still accept, carrying what the model needs to
//! keep working) is the same.

use std::sync::Arc;

use anyhow::{Context, Result};
use futures_util::StreamExt;

use crate::llm::provider::LlmProvider;
use crate::llm::{ChatMessage, ChatOptions, ChatRequest, Role};

/// Number of most-recent messages preserved verbatim when compacting history.
pub(crate) const KEEP_RECENT: usize = 10;

/// Heading of the note [`compact`] leaves in place of the span it summarized.
/// Public because it is the only handle anything downstream has on that note:
/// it is an ordinary message like any other, and
/// [`ultra::render_context`](super::ultra) has to be able to tell "everything
/// older than the tail, summarized" apart from an ordinary injected note when
/// it briefs a candidate.
pub const COMPACT_SUMMARY_HEADING: &str = "[Compacted progress summary]";

/// Prefix of the ephemeral per-step pressure line injected (in memory only)
/// before each model completion. Surfaces and compaction can recognize it; it
/// is never persisted to the session file.
pub const CONTEXT_PRESSURE_HEADING: &str = "[context pressure]";

/// Fraction of the provider's context window the last prompt may fill before
/// token-aware compaction kicks in.
const COMPACT_WINDOW_FRACTION: f64 = 0.8;

/// Soft pressure band: the model is nudged to call `compact` once fill crosses
/// this fraction of the known window (auto-compact still waits for
/// [`COMPACT_WINDOW_FRACTION`]).
pub(crate) const PRESSURE_ELEVATED_FRACTION: f64 = 0.5;

/// Strong pressure band: the model is told to compact *before* more tool work.
const PRESSURE_HIGH_FRACTION: f64 = 0.7;

/// Chunk size (chars) fed to one rolling-summary pass during compaction.
const COMPACT_CHUNK_CHARS: usize = 20_000;

/// How long a compaction summary's stream may produce nothing before it is
/// abandoned.
///
/// Shorter than the turn loop's equivalent
/// ([`turn::STREAM_IDLE_TIMEOUT`](super::turn)) because the stakes are the
/// other way round. Abandoning a real completion loses the model's work;
/// abandoning a summary costs a rolling pass and falls back to truncation,
/// which is a worse history but a live one. What must not happen is the third
/// option: a perpetual run parked forever inside a compaction that a proxy
/// stopped answering halfway through, which is silent, indefinite, and
/// indistinguishable from the run having stopped.
const SUMMARY_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// How full the next model call's prompt is, for the live pressure signal and
/// the `compact` tool's reply. Built from the last reported prompt size (or a
/// char/4 estimate) plus the provider's known context window when available.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextPressure {
    /// Tokens that will load into the next model call.
    pub tokens: u64,
    /// Provider context window in tokens, when known.
    pub window: Option<u32>,
    /// `tokens / window` when the window is known; otherwise a byte-threshold
    /// proxy so headless runs without a reported window still get a signal.
    pub fill: f64,
    pub level: PressureLevel,
}

/// Coarse pressure band shown to the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PressureLevel {
    /// Comfortable headroom.
    Ok,
    /// Crossing ~50% of the window (or half the byte threshold) — compact when
    /// convenient.
    Elevated,
    /// Crossing ~70% — compact before more tool work.
    High,
    /// At or past the auto-compact trigger (~80% / byte threshold).
    Critical,
}

impl PressureLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            PressureLevel::Ok => "ok",
            PressureLevel::Elevated => "elevated",
            PressureLevel::High => "high",
            PressureLevel::Critical => "critical",
        }
    }
}

impl ContextPressure {
    /// One-line note the model sees each step (ephemeral, not persisted).
    pub fn signal_line(&self) -> String {
        let tokens = crate::usage::format_tokens(self.tokens);
        let window = match self.window {
            Some(w) => crate::usage::format_tokens(u64::from(w)),
            None => "unknown window".to_string(),
        };
        let pct = (self.fill * 100.0).round() as i32;
        let advice = match self.level {
            PressureLevel::Ok => "headroom ok",
            PressureLevel::Elevated => "consider calling compact soon",
            PressureLevel::High => "call compact before more tool work",
            PressureLevel::Critical => "auto-compact imminent — call compact now",
        };
        format!(
            "{CONTEXT_PRESSURE_HEADING} {} · {tokens} / {window} ({pct}%) — {advice}",
            self.level.as_str()
        )
    }
}

/// The four numbers a pressure reading is derived from, gathered by whoever
/// owns the history. Taken as a struct rather than four arguments because
/// three of them are token counts and transposing two of those is a bug that
/// still compiles.
#[derive(Debug, Clone, Copy)]
pub struct Measured {
    /// Tokens that will load into the next call: the backend's last reported
    /// prompt size when it has one, otherwise a char/4 estimate.
    pub tokens: u64,
    /// The provider's context window for this model, when it is known.
    pub window: Option<u32>,
    /// Serialized size of the history, the fallback measure when it is not.
    pub bytes: usize,
    /// Byte ceiling that fallback is judged against.
    pub byte_threshold: usize,
    /// The last prompt size the backend actually *reported*, as opposed to
    /// [`Self::tokens`], which may be an estimate.
    pub last_prompt: Option<u64>,
}

/// Live fill of the next model call against the provider window (or a
/// byte-threshold proxy when the window is unknown). Powers the per-step
/// pressure signal and the `compact` tool's reply.
///
/// Soft bands (`elevated` / `high`) may use a char/4 estimate when the backend
/// has not reported a prompt size yet. The `critical` band — which drives
/// auto-compaction — needs a *reported* last prompt over 80% of a known
/// window; the byte threshold is only the fallback gate when the window is
/// unknown. A known window makes tokens the authoritative measure — the byte
/// proxy (48 KB default, sized for small local models) would otherwise scream
/// "critical" at a few percent of a large window. Estimates never trip
/// auto-compact on their own (they would fire on the system prompt alone and
/// steal the first completion of a short turn).
pub fn pressure(measured: Measured) -> ContextPressure {
    let Measured {
        tokens,
        window,
        bytes,
        byte_threshold,
        last_prompt,
    } = measured;
    let threshold = byte_threshold.max(1);

    let fill = match window {
        Some(w) if w > 0 => tokens as f64 / f64::from(w),
        _ => bytes as f64 / threshold as f64,
    };

    let auto_critical = match window {
        Some(w) if w > 0 => match last_prompt {
            Some(prompt) => prompt as f64 > f64::from(w) * COMPACT_WINDOW_FRACTION,
            None => false,
        },
        _ => bytes > threshold,
    };

    let level = if auto_critical {
        PressureLevel::Critical
    } else if fill >= PRESSURE_HIGH_FRACTION {
        PressureLevel::High
    } else if fill >= PRESSURE_ELEVATED_FRACTION {
        PressureLevel::Elevated
    } else {
        PressureLevel::Ok
    };
    ContextPressure {
        tokens,
        window,
        fill,
        level,
    }
}

/// What a compaction pass did, reported back so callers (auto-compaction
/// notices and the `/compact` command) can describe the outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactOutcome {
    /// Too little history between the system prompt and the recent tail.
    Nothing,
    /// Summarized `count` middle messages into one progress note.
    Summarized(usize),
    /// The summary LLM failed, so `count` middle messages were dropped.
    Truncated { count: usize, error: String },
}

impl CompactOutcome {
    /// One-line notice describing what the pass did, shared by the
    /// auto-compaction events and the `/compact` command.
    pub fn describe(&self) -> String {
        fn messages(count: usize) -> String {
            if count == 1 {
                "1 message".to_string()
            } else {
                format!("{count} messages")
            }
        }
        match self {
            CompactOutcome::Nothing => "nothing to compact yet".to_string(),
            CompactOutcome::Summarized(count) => {
                format!("compacted {} into a summary", messages(*count))
            }
            CompactOutcome::Truncated { count, error } => format!(
                "compacted {} by truncation (summary failed: {error})",
                messages(*count)
            ),
        }
    }
}

/// Where the kept tail is allowed to begin, which is the one rule a
/// conversation and a sub-loop do not share.
///
/// Both anchors exist to keep the *shrunken* request legal. A tool result
/// separated from the assistant message that asked for it is rejected by every
/// provider, and Anthropic additionally takes the system prompt as a top-level
/// field, so its adapter hoists every `Role::System` message out of the
/// history — which means the first message left standing has to be one the API
/// accepts as an opener.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Anchor {
    /// The tail begins at a `Role::User` message.
    ///
    /// A real conversation has one every turn, so this both keeps tool-call
    /// groups whole and leaves a user message opening the request for free.
    Conversation,
    /// The tail begins at the nearest message that is not a tool result.
    ///
    /// A sub-loop's history has exactly *one* user message — the task at index
    /// 1 — and [`Anchor::Conversation`] would walk the boundary all the way
    /// back onto it and find nothing it was allowed to cut, so a sub-run that
    /// outgrew its window simply failed. Stopping at an assistant turn instead
    /// is safe for tool-call groups (the calls and their results go together),
    /// and the note that replaces the span becomes the user message that opens
    /// the request — see [`compact`].
    SubLoop,
}

impl Anchor {
    /// Whether the kept tail may start at `message`.
    fn may_begin_tail(self, message: &ChatMessage) -> bool {
        match self {
            Anchor::Conversation => message.role == Role::User,
            Anchor::SubLoop => message.role != Role::Tool,
        }
    }
}

/// A completed compaction pass: what it did, and the note it wrote.
#[derive(Debug, Clone)]
pub struct Compacted {
    pub outcome: CompactOutcome,
    /// The note that replaced the span, for a caller that persists one. `None`
    /// when nothing was cut, or when the summary failed and the span was
    /// dropped outright.
    pub note: Option<ChatMessage>,
}

impl Compacted {
    /// A pass that found nothing to do.
    fn nothing() -> Self {
        Self {
            outcome: CompactOutcome::Nothing,
            note: None,
        }
    }
}

/// Summarize the middle span of `history` — everything between the system
/// prompt and the last [`KEEP_RECENT`] messages — into a single note,
/// unconditionally. Callers decide *when* by consulting [`pressure`]; this
/// decides *what*.
///
/// A summarization failure falls back to dropping the middle span, so this
/// never fails and never aborts a run: history that cannot be summarized is
/// still history that has to fit.
///
/// The note's role follows the message the tail now starts with: a system note
/// when that is a user message (the conversation case, where the note is
/// context *about* the transcript), and a user message when it is not, because
/// then the note is the only thing left that can open the request — see
/// [`Anchor`].
pub async fn compact(
    history: &mut Vec<ChatMessage>,
    anchor: Anchor,
    client: &Arc<dyn LlmProvider>,
    model: &str,
) -> Compacted {
    // Need history[0] (system prompt) + a non-empty middle + the recent tail.
    if history.len() <= KEEP_RECENT + 1 {
        return Compacted::nothing();
    }
    let start = 1;
    let mut end = history.len() - KEEP_RECENT;
    // Never cut between an assistant tool-call message and its results: snap
    // the boundary back until the kept tail starts somewhere it is allowed to.
    while end > start && !anchor.may_begin_tail(&history[end]) {
        end -= 1;
    }
    if start >= end {
        return Compacted::nothing();
    }
    let count = end - start;

    match summarize_span(&history[start..end], client, model).await {
        Ok(summary) => {
            let text = format!("{COMPACT_SUMMARY_HEADING}\n{summary}");
            let note = if history[end].role == Role::User {
                ChatMessage::system(text)
            } else {
                ChatMessage::user(text)
            };
            history.splice(start..end, std::iter::once(note.clone()));
            Compacted {
                outcome: CompactOutcome::Summarized(count),
                note: Some(note),
            }
        }
        Err(err) => {
            // Fall back to truncation: drop the middle span outright.
            history.drain(start..end);
            Compacted {
                outcome: CompactOutcome::Truncated {
                    count,
                    error: format!("{err:#}"),
                },
                note: None,
            }
        }
    }
}

/// Summarize `span` with a rolling per-chunk pass, so an arbitrarily large
/// span is fully represented instead of hard-truncated: each chunk is
/// summarized together with the summary of everything before it.
async fn summarize_span(
    span: &[ChatMessage],
    client: &Arc<dyn LlmProvider>,
    model: &str,
) -> Result<String> {
    // Render each message and pack into ~20k-char chunks, splitting oversized
    // messages at char boundaries.
    let mut chunks: Vec<String> = vec![String::new()];
    for msg in span {
        let role = match msg.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        let rendered = format!("{role}: {}\n", msg.text());
        let mut rest = rendered.as_str();
        while !rest.is_empty() {
            let chunk = chunks.last_mut().expect("at least one chunk");
            let room = COMPACT_CHUNK_CHARS.saturating_sub(chunk.len());
            if rest.len() <= room {
                chunk.push_str(rest);
                break;
            }
            if room == 0 {
                chunks.push(String::new());
                continue;
            }
            let mut cut = room;
            while cut > 0 && !rest.is_char_boundary(cut) {
                cut -= 1;
            }
            if cut == 0 {
                chunks.push(String::new());
                continue;
            }
            chunk.push_str(&rest[..cut]);
            rest = &rest[cut..];
            chunks.push(String::new());
        }
    }

    let mut summary: Option<String> = None;
    for chunk in &chunks {
        let blob = match &summary {
            None => chunk.clone(),
            Some(prev) => format!(
                "[Progress summary of the transcript so far]\n{prev}\n\n\
                 [Transcript continues]\n{chunk}"
            ),
        };
        summary = Some(summarize_transcript(&blob, client, model).await?);
    }
    summary.ok_or_else(|| anyhow::anyhow!("nothing to summarize"))
}

/// Summarize a transcript blob into a terse progress note via the model.
/// Deltas are not forwarded to any UI.
async fn summarize_transcript(
    blob: &str,
    client: &Arc<dyn LlmProvider>,
    model: &str,
) -> Result<String> {
    let request = ChatRequest {
        model: model.to_string(),
        messages: vec![
            ChatMessage::system(
                "Summarize the following Wizard agent transcript into a compact progress \
                 note. Preserve: the mission/goal, decisions made, files changed, commands \
                 run, what worked/failed, and open next steps. Preserve the current todo \
                 list state verbatim (every item and its status) if one was maintained, \
                 and mention the plan file path (.wizard/plan.md) if a plan was written. \
                 Be terse and factual.",
            ),
            ChatMessage::user(blob.to_string()),
        ],
        tools: Vec::new(),
        stream: true,
        options: Some(ChatOptions {
            temperature: Some(0.2),
            num_ctx: None,
            // Internal summarization stays at the provider default; the user's
            // `/effort` applies to real turns, not compaction.
            reasoning_effort: None,
        }),
    };

    let mut stream = client
        .chat_stream(request)
        .await
        .context("starting compaction summary")?;
    let mut summary = String::new();
    loop {
        let Some(chunk) = tokio::time::timeout(SUMMARY_IDLE_TIMEOUT, stream.next())
            .await
            .map_err(|_elapsed| {
                anyhow::anyhow!(
                    "the compaction summary stream produced nothing for {}s",
                    SUMMARY_IDLE_TIMEOUT.as_secs()
                )
            })?
        else {
            break;
        };
        let chunk = chunk.context("reading compaction stream")?;
        if let Some(message) = chunk.message
            && !chunk.thinking
        {
            summary.push_str(&message.text());
        }
        if chunk.done {
            break;
        }
    }
    if summary.trim().is_empty() {
        anyhow::bail!("empty summary");
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sub-loop's history: system prompt, the task, then assistant/tool
    /// pairs and nothing else. `Anchor::Conversation` has exactly one user
    /// message to aim at — the task — and it is `start`, so the boundary walks
    /// all the way back and there is nothing left to cut. That is the shape
    /// that used to make a long subagent run un-compactable, and it is
    /// entirely invisible on a parent conversation.
    #[test]
    fn a_sub_loops_history_offers_a_conversation_anchor_no_boundary_at_all() {
        let mut history = vec![
            ChatMessage::system("you are a worker"),
            ChatMessage::user("the task"),
        ];
        for step in 0..8 {
            history.push(ChatMessage::assistant(format!("step {step}")));
            history.push(ChatMessage::tool_result("id", "probe", "output"));
        }
        let naive = history.len() - KEEP_RECENT;
        assert!(naive > 1, "there is a middle span to cut");

        let mut end = naive;
        while end > 1 && !Anchor::Conversation.may_begin_tail(&history[end]) {
            end -= 1;
        }
        assert_eq!(end, 1, "the only user message is the task at index 1");

        let mut end = naive;
        while end > 1 && !Anchor::SubLoop.may_begin_tail(&history[end]) {
            end -= 1;
        }
        assert!(end > 1, "the sub-loop anchor finds a boundary: {end}");
        assert_ne!(
            history[end].role,
            Role::Tool,
            "and never one that orphans a tool result"
        );
    }

    /// The reported prompt size is what trips auto-compaction, and only
    /// against a window the provider actually named. An estimate must not,
    /// or the system prompt alone would compact a fresh session.
    #[test]
    fn only_a_reported_prompt_against_a_known_window_is_critical() {
        let known = Measured {
            tokens: 90_000,
            window: Some(100_000),
            bytes: 10,
            byte_threshold: 48_000,
            last_prompt: Some(90_000),
        };
        assert_eq!(pressure(known).level, PressureLevel::Critical);
        assert_eq!(
            pressure(Measured {
                last_prompt: None,
                ..known
            })
            .level,
            PressureLevel::High,
            "an estimate reaches the soft bands and stops there"
        );
        assert_eq!(
            pressure(Measured {
                bytes: 100_000,
                window: None,
                ..known
            })
            .level,
            PressureLevel::Critical,
            "with no window the byte proxy is the gate"
        );
    }
}
