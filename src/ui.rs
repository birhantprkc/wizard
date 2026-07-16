//! Ratatui rendering: pure functions from [`App`] state to widgets.
//! Layout: chat transcript (with optional git diff sidebar) above the input
//! line and a quiet status line. Floating layers: the compact todo overlay
//! just above the composer, the command-suggestion popup, and the
//! model/mode/rewind/subagent picker.
//!
//! Design rules (do not regress):
//! - **Transparent**: never paint a background color; everything renders on
//!   `Color::Reset` so the user's terminal background shows through.
//!   Selection reads through an accent marker + bold, not opaque slabs.
//! - **Monochrome**: white accent plus dim grays only — no hues anywhere.
//!   Emphasis reads through brightness and bold, semantics through glyphs
//!   (✓/✗), never color.
//! - **No heavy boxes**: borderless sections separated by padding and dim
//!   rules; rounded dim borders only on floating layers.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use pulldown_cmark::{
    Alignment as MdAlignment, CodeBlockKind, Event as MdEvent, Options, Parser, Tag, TagEnd,
};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Layout, Margin, Position, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::agent::ImageSource;
use crate::app::{App, InputMode, PaneStatus, Selection, SubagentPane, TranscriptEntry};
use crate::config::Mode;
use crate::image_view::{self, ImageBlock, ImageBox, ImageCache};
use crate::images::ImageRef;
use crate::session_registry::SessionState;
use crate::vim::VimMode;

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Tallest the multi-line composer grows before it scrolls internally.
const MAX_INPUT_ROWS: u16 = 10;

/// Column an image block hangs in — the same two-space text column a tool
/// card's output and a notice use, so a tool's image lines up under its card.
const IMAGE_INDENT: u16 = 2;

/// The single accent color used for chrome (prompt, gutters, names,
/// attention borders).
const ACCENT: Color = Color::White;
/// Dim chrome: rules, gutter marks, hints, secondary borders.
const DIM: Color = Color::DarkGray;
/// Secondary text (tool output, user echo, details).
const TEXT_DIM: Color = Color::Gray;
/// Inline code (block code gets grayscale syntect foregrounds, or
/// [`TEXT_DIM`] when plain).
const CODE: Color = Color::White;

fn dim() -> Style {
    Style::default().fg(DIM)
}

fn accent() -> Style {
    Style::default().fg(ACCENT)
}

/// Render one frame. The only entry point the main loop calls; everything
/// else in this module is a helper.
pub fn draw(frame: &mut Frame, app: &App) {
    let [main_area, input_area, rail_area, status_area] = regions(app, frame.area());

    if let Some(pane) = app.attached_pane() {
        // Inside a subagent: its conversation takes over the main area and
        // renders exactly like the main chat.
        draw_pane(frame, app, pane, main_area);
    } else if app.show_diff {
        let [chat_area, side_area] =
            Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)])
                .areas(main_area);
        draw_transcript(frame, app, chat_area);
        draw_diff_sidebar(frame, app, side_area);
    } else {
        draw_transcript(frame, app, main_area);
    }

    draw_input(frame, app, input_area);
    if rail_area.height > 0 {
        draw_rail(frame, app, rail_area);
    }
    draw_status_bar(frame, app, status_area);

    // Floating layers, back to front. Todos sit just above the composer (a
    // few rows, full width) so the chat keeps its full width; suggestions
    // stack on top when the user is typing a slash command.
    if !overlay_open(app) {
        if app.show_todos {
            draw_todo_overlay(frame, app, input_area);
        }
        draw_suggestions(frame, app, input_area);
    }
    if app.picker.is_some() {
        draw_picker(frame, app);
    }
    if app.plan_review.is_some() {
        draw_plan_review(frame, app);
    }
    if app.interview.is_some() {
        draw_interview(frame, app);
    }
    // The dashboard is modal and full-screen, so it paints last (on top).
    if app.show_dashboard {
        draw_dashboard(frame, app);
    }

    // With any overlay floating above the transcript, a click belongs to the
    // overlay — drop the card hit map so it can't toggle a card underneath.
    if overlay_open(app) {
        app.card_hits.borrow_mut().clear();
    }

    // The selection highlight paints last so it reverses whatever ended up on
    // screen — transcript, sidebar, or an overlay the user dragged across.
    if let Some(selection) = app.selection {
        let area = frame.area();
        let buf = frame.buffer_mut();
        for (y, start, end) in selection_rows(&selection, area.width, area.height) {
            for x in start..end {
                if let Some(cell) = buf.cell_mut(Position::new(x, y)) {
                    cell.modifier.insert(Modifier::REVERSED);
                }
            }
        }
    }
}

/// The rows a frame is laid out into, top to bottom: the transcript body, the
/// composer, the subagent rail, and the status line. One place, so what [`draw`]
/// paints and what anything else measures cannot drift apart.
pub fn regions(app: &App, area: Rect) -> [Rect; 4] {
    // The composer grows with its content (hard line breaks plus soft-wrapped
    // continuations) up to MAX_INPUT_ROWS, then scrolls vertically. +2 for the
    // rules above/below.
    let budget = composer_budget(area.width);
    let input_rows =
        (wrap_rows(&composer_chars(app), budget).len() as u16).clamp(1, MAX_INPUT_ROWS);
    // The rail sits between the composer and the status bar: one row per
    // subagent, so the dots are always in the same place, right under the bar.
    let rail_rows = rail_height(app);
    Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(input_rows + 2),
        Constraint::Length(rail_rows),
        Constraint::Length(1),
    ])
    .areas(area)
}

/// Per-row spans `(y, start_x, end_x_exclusive)` a selection covers over a grid
/// `width` × `height`. Reading-order flow: the first row runs from the start
/// column to the edge, full rows in between, the last row from the edge to the
/// head column (inclusive). Shared by the highlight overlay and the clipboard
/// extraction so what's shown is exactly what's copied.
fn selection_rows(selection: &Selection, width: u16, height: u16) -> Vec<(u16, u16, u16)> {
    let ((start_x, start_y), (end_x, end_y)) = selection.ordered();
    let mut rows = Vec::new();
    let last_y = end_y.min(height.saturating_sub(1));
    for y in start_y..=last_y {
        let row_start = if y == start_y { start_x } else { 0 }.min(width);
        // Include the cell under the head: end column + 1, clamped to the edge.
        let row_end = if y == end_y {
            end_x.saturating_add(1)
        } else {
            width
        }
        .min(width);
        if row_start < row_end {
            rows.push((y, row_start, row_end));
        }
    }
    rows
}

/// Extract the text under a selection from a rendered cell buffer, in reading
/// order, one `\n` per screen row. Trailing whitespace is trimmed per line so
/// the copy isn't padded out to the row width.
pub fn selection_text(buf: &Buffer, selection: &Selection) -> String {
    let area = buf.area;
    let rows = selection_rows(selection, area.width, area.height);
    let mut out = String::new();
    for (i, (y, start, end)) in rows.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let mut line = String::new();
        for x in *start..*end {
            if let Some(cell) = buf.cell(Position::new(x, *y)) {
                line.push_str(cell.symbol());
            }
        }
        out.push_str(line.trim_end());
    }
    // A selection of only blank cells trims to nothing; report it as empty so
    // the caller skips the copy.
    if out.trim().is_empty() {
        String::new()
    } else {
        out
    }
}

/// Chat transcript: user/assistant messages with streaming markdown and
/// collapsible tool cards. Borderless; a one-column side margin keeps the
/// text off the terminal edge. Shows the welcome screen while empty.
fn draw_transcript(frame: &mut Frame, app: &App, area: Rect) {
    // Rebuilt from scratch every frame; cleared up front so the early
    // returns below can't leave stale clickable rows behind.
    app.card_hits.borrow_mut().clear();

    // Stay on the welcome screen until the conversation actually begins (see
    // `App::welcome_visible`: early system notices alone don't dismiss it,
    // but any submission — even a slash command — does).
    if app.welcome_visible() {
        // A slash-command menu (e.g. `/provider`) or other modal floats over a
        // small centered area; the welcome card would show through around it.
        // Drop the card while any overlay is open so there's no text overlay.
        if !overlay_open(app) {
            draw_welcome(frame, app, area);
        }
        return;
    }

    let inner = area.inner(Margin {
        horizontal: 1,
        vertical: 0,
    });
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let inner_width = inner.width as usize;
    let inner_height = inner.height as usize;

    let mut cache = app.images.borrow_mut();
    // Tables lay out to the content column (past the two-column gutter), so
    // their rows fit the terminal instead of soft-wrapping mid-grid.
    let content_width = inner_width.saturating_sub(2);
    let rendered = transcript_text(app, &mut cache, image_box(inner), content_width);
    let (lines, row_tags) = wrap_tagged(rendered.lines, rendered.tags, inner_width);
    let total = lines.len();
    let max_scroll = total.saturating_sub(inner_height);
    // Cache for key handlers so they can convert a follow-tail view into a
    // stable top-anchored offset without re-wrapping the transcript.
    app.transcript_max_scroll.set(max_scroll as u16);
    // Stick-to-bottom: when following (or the content still fits), pin to the
    // live tail. Otherwise hold the absolute top-of-viewport offset so new
    // streaming lines do not yank the user away from what they were reading.
    let start = if app.scroll_follow || max_scroll == 0 {
        max_scroll
    } else {
        (app.scroll as usize).min(max_scroll)
    };
    let remaining = max_scroll.saturating_sub(start);
    let end = (start + inner_height).min(total);
    let visible: Vec<Line<'static>> = lines[start..end].to_vec();

    // Record where card headers landed on screen for click-to-toggle.
    {
        let mut hits = app.card_hits.borrow_mut();
        for (offset, tag) in row_tags[start..end].iter().enumerate() {
            if let RowTag::Card(index) = tag {
                hits.push((inner.y + offset as u16, *index));
            }
        }
    }

    frame.render_widget(Paragraph::new(Text::from(visible)), inner);

    // Then the pixels, into the rows the text left blank for them. Skipped
    // under an overlay: the modal owns the screen, and an image drawn with a
    // terminal graphics protocol must not paint through it.
    if !overlay_open(app) {
        paint_images(
            frame,
            inner,
            &row_tags[start..end],
            &rendered.blocks,
            &mut cache,
        );
    }

    // Scrolled away from the tail: a quiet hint in the top-right corner.
    if remaining > 0 {
        let label = format!("↓ {remaining} more ");
        let width = (label.width() as u16).min(inner.width);
        let hint = Rect {
            x: inner.right().saturating_sub(width),
            y: inner.y,
            width,
            height: 1,
        };
        frame.render_widget(Clear, hint);
        frame.render_widget(Paragraph::new(Span::styled(label, dim())), hint);
    }

    // A whisper of a scrollbar in the right margin once content overflows.
    if total > inner_height {
        let mut state = ScrollbarState::new(max_scroll + 1).position(start);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(None)
            .thumb_symbol("▐")
            .thumb_style(dim());
        frame.render_stateful_widget(scrollbar, area, &mut state);
    }
}

/// Welcome screen shown before the first message: a small centered card,
/// no borders, no banner art.
fn draw_welcome(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line<'static>> = vec![
        Line::from(Span::styled("✦", accent())),
        Line::raw(""),
        Line::from(Span::styled(
            "w i z a r d",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled("your sovereign agent", dim().italic())),
        Line::raw(""),
        Line::from(vec![
            model_span(app),
            Span::styled(" · ", dim()),
            mode_span(app.status.mode),
        ]),
        Line::raw(""),
        Line::raw(""),
        Line::from(vec![
            Span::styled("type a message", Style::default().fg(TEXT_DIM)),
            Span::styled(" and press Enter to begin", dim()),
        ]),
        Line::raw(""),
        Line::from(vec![
            Span::styled("/", accent()),
            Span::styled("  commands — Tab completes, ↑/↓ select", dim()),
        ]),
        Line::from(vec![
            Span::styled("/model", accent()),
            Span::styled("  pick a model", dim()),
        ]),
        Line::from(vec![
            Span::styled("/help", accent()),
            Span::styled("  all commands & keys", dim()),
        ]),
    ];

    // A broken provider, caught by the deferred health probe, surfaces under the
    // model line so it's visible at launch rather than only when a turn fails.
    if let Some(err) = &app.provider_health_error {
        lines.insert(
            6,
            Line::from(Span::styled(
                format!("⚠ provider unreachable: {err}"),
                Style::default().fg(Color::White).bold(),
            )),
        );
    }

    let height = lines.len() as u16;
    let top = area.height.saturating_sub(height) / 2;
    let centered = Rect {
        x: area.x,
        y: area.y + top,
        width: area.width,
        height: height.min(area.height),
    };
    frame.render_widget(
        Paragraph::new(Text::from(lines)).alignment(Alignment::Center),
        centered,
    );
}

/// Colored span for a mode name: genie is quiet, sovereign is a warning.
fn mode_span(mode: Mode) -> Span<'static> {
    match mode {
        Mode::Genie => Span::styled("genie", Style::default().fg(TEXT_DIM)),
        Mode::Sovereign => Span::styled("sovereign", Style::default().fg(Color::White).bold()),
    }
}

/// The status-bar model label. Loud (accent, bold) while `/fusion` is on — it
/// runs every turn through a panel of models, several× the tokens — so the mode
/// is never left running unnoticed; dim otherwise.
fn model_span(app: &App) -> Span<'static> {
    if app.fusion_active {
        Span::styled(app.status.model.clone(), accent().bold())
    } else {
        Span::styled(app.status.model.clone(), Style::default().fg(TEXT_DIM))
    }
}

/// Prefix a rendered block with a gutter: `marker` on the first line, a
/// two-column indent on the rest, so the message hangs off its mark.
fn gutter_block(lines: &mut Vec<Line<'static>>, text: Text<'static>, marker: Span<'static>) {
    for (index, mut line) in text.lines.into_iter().enumerate() {
        let mut spans = Vec::with_capacity(line.spans.len() + 1);
        if index == 0 {
            spans.push(marker.clone());
        } else if !line.spans.is_empty() {
            spans.push(Span::raw("  "));
        }
        spans.append(&mut line.spans);
        lines.push(Line::from(spans));
    }
}

/// Render model reasoning ("thinking") as plain dimmed-italic lines.
/// No markdown: reasoning is background noise, not the answer.
fn thinking_text(message: &str) -> Text<'static> {
    let style = dim().italic();
    let lines: Vec<Line<'static>> = message
        .lines()
        .map(|line| Line::from(Span::styled(line.to_string(), style)))
        .collect();
    Text::from(lines)
}

/// What a rendered transcript row belongs to. Rows are wrapped and sliced by
/// the scroll before anything is painted, so this is how a row on screen is
/// traced back to what put it there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowTag {
    /// Ordinary text.
    Text,
    /// The *header* line of the tool card at this transcript index — the
    /// click-to-toggle target.
    Card(usize),
    /// Row `row` of the image block at `slot` in [`Rendered::blocks`]. The row
    /// is left blank in the text and the pixels are painted into it afterwards
    /// (see [`paint_images`]), so an image scrolls and clips like any other
    /// content.
    Image { slot: usize, row: u16 },
}

/// A transcript rendered to lines: what each row is, and the image blocks whose
/// rows are waiting to be painted.
struct Rendered {
    lines: Vec<Line<'static>>,
    tags: Vec<RowTag>,
    blocks: Vec<ImageBlock>,
    /// Nothing has been pushed yet, so a caller appending more gets the
    /// blank-line spacing right.
    empty: bool,
}

/// The cells an image may take in a transcript body: the text column it hangs
/// in, capped so it stays a thumbnail, and never more than half the viewport —
/// one image cannot push the conversation off the screen.
fn image_box(inner: Rect) -> ImageBox {
    ImageBox {
        cols: inner
            .width
            .saturating_sub(IMAGE_INDENT)
            .min(image_view::MAX_COLS),
        rows: (inner.height / 2).clamp(1, image_view::MAX_ROWS),
    }
}

/// Is a floating layer covering the transcript? Then a click belongs to the
/// overlay rather than a card underneath, and nothing of the transcript's own —
/// its images least of all — should paint through.
fn overlay_open(app: &App) -> bool {
    app.picker.is_some()
        || app.plan_review.is_some()
        || app.interview.is_some()
        || app.show_dashboard
}

/// The two lines that always accompany an image, whatever the terminal can
/// draw: what made it and how big it is, then the file itself. The path gets a
/// line to itself so it survives a drag-select as one string — it is how the
/// user opens the real thing.
fn image_caption(source: &ImageSource, image: &ImageRef) -> Vec<Line<'static>> {
    let from = match source.tool() {
        Some(tool) => format!("image from `{tool}`"),
        None => "image".to_string(),
    };
    vec![
        Line::from(Span::styled(
            format!(
                "  {from} · {} · {} KB",
                image.mime,
                image.bytes.div_ceil(1024)
            ),
            dim().italic(),
        )),
        Line::from(Span::styled(
            format!("  {}", image.path.display()),
            Style::default().fg(TEXT_DIM),
        )),
    ]
}

/// Build the full (unwrapped) transcript text from app state, plus the per-row
/// tags and the image blocks the rows were reserved for.
///
/// `content_width` is the display columns available for markdown body text
/// (terminal inner width minus the two-column gutter). Tables use it so their
/// columns shrink and cells wrap instead of the whole grid soft-wrapping.
fn transcript_text(
    app: &App,
    cache: &mut ImageCache,
    budget: ImageBox,
    content_width: usize,
) -> Rendered {
    let Rendered {
        mut lines,
        mut tags,
        blocks,
        empty,
    } = entries_text(&app.transcript, app.tick, cache, budget, content_width);
    let mut first = empty;

    if !app.streaming_thinking.is_empty() {
        if !first {
            lines.push(Line::raw(""));
        }
        first = false;
        // In-flight reasoning, dimmed so it reads as background noise.
        gutter_block(
            &mut lines,
            thinking_text(&app.streaming_thinking),
            Span::styled("· ", dim()),
        );
    }
    if !app.streaming.is_empty() {
        if !first {
            lines.push(Line::raw(""));
        }
        // Streaming: the text itself arriving, with a soft cursor at the
        // tail. Code blocks stay unhighlighted while in flight (cheap to
        // re-render every frame).
        let mut text = render_markdown_streaming(&app.streaming, content_width);
        let tail = Span::styled("▍", dim());
        match text.lines.last_mut() {
            Some(last) => last.spans.push(tail),
            None => text.lines.push(Line::from(tail)),
        }
        gutter_block(&mut lines, text, Span::styled("· ", accent()));
    } else if app.status.busy {
        if !first {
            lines.push(Line::raw(""));
        }
        let spinner = SPINNER[(app.tick as usize) % SPINNER.len()];
        lines.push(Line::from(vec![
            Span::styled(format!("{spinner} "), accent()),
            Span::styled(format!("{}…", app.spinner_verb), dim().italic()),
        ]));
    }

    tags.resize(lines.len(), RowTag::Text);
    Rendered {
        lines,
        tags,
        blocks,
        empty: first,
    }
}

/// Render a list of transcript entries to lines, tagging each row with what it
/// belongs to and reserving the rows an image block will be painted into.
///
/// Shared by the main transcript and by a subagent's pane, which is what makes
/// an attached pane render identically to the main chat — images included.
/// `content_width` is the body column tables may fill (see [`transcript_text`]).
fn entries_text(
    entries: &[TranscriptEntry],
    tick: u64,
    cache: &mut ImageCache,
    budget: ImageBox,
    content_width: usize,
) -> Rendered {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut tags: Vec<RowTag> = Vec::new();
    let mut blocks: Vec<ImageBlock> = Vec::new();
    let mut prev_tool = false;
    let mut prev_notice = false;
    let mut first = true;

    for (index, entry) in entries.iter().enumerate() {
        let is_tool = matches!(entry, TranscriptEntry::ToolCard { .. });
        let is_notice = matches!(entry, TranscriptEntry::Notice(_));
        let is_image = matches!(entry, TranscriptEntry::Image { .. });
        // Comfortable spacing between turns; runs of tool cards or notices
        // stay tight so they read as one group. An image is always tight: it
        // belongs to whatever produced it, so it hangs directly under that
        // message or card rather than floating between them.
        let tight = (is_tool && prev_tool) || (is_notice && prev_notice) || is_image;
        if !first && !tight {
            lines.push(Line::raw(""));
        }
        first = false;
        prev_tool = is_tool;
        prev_notice = is_notice;

        // A tool card's first pushed line is its header (glyph + name).
        let header_at = lines.len();
        // Where an image block's rows begin, once it is known to have any.
        let mut image_at = None;

        match entry {
            TranscriptEntry::User(message) => {
                let mut user_lines: Vec<Line<'static>> = Vec::new();
                for line in message.lines() {
                    user_lines.push(Line::from(Span::styled(
                        line.to_string(),
                        Style::default().fg(TEXT_DIM),
                    )));
                }
                gutter_block(
                    &mut lines,
                    Text::from(user_lines),
                    Span::styled("❯ ", dim().bold()),
                );
            }
            TranscriptEntry::Assistant(message) => {
                gutter_block(
                    &mut lines,
                    render_markdown_at(message, content_width),
                    Span::styled("· ", accent()),
                );
            }
            TranscriptEntry::Thinking(message) => {
                gutter_block(
                    &mut lines,
                    thinking_text(message),
                    Span::styled("· ", dim()),
                );
            }
            TranscriptEntry::ToolCard {
                name,
                args,
                output,
                is_error,
                collapsed,
            } => {
                tool_card_lines(
                    &mut lines,
                    name,
                    args,
                    output.as_deref(),
                    *is_error,
                    *collapsed,
                    tick,
                );
            }
            TranscriptEntry::Image { source, image } => {
                // Reserve the block's rows as blank lines. They wrap to
                // themselves, scroll with the text, and are painted last —
                // which is what keeps the pixels inside their own rows however
                // the transcript moves. A terminal that can draw nothing
                // reserves nothing, and the caption below stands alone.
                if let Some(block) = cache.layout(image, budget) {
                    image_at = Some((lines.len(), blocks.len(), block.rows));
                    lines.extend(std::iter::repeat_n(Line::raw(""), block.rows as usize));
                    blocks.push(block);
                }
                lines.extend(image_caption(source, image));
            }
            TranscriptEntry::Notice(message) => {
                let style = if message.starts_with("error") {
                    Style::default().fg(Color::White).bold()
                } else {
                    dim().italic()
                };
                for line in message.lines() {
                    lines.push(Line::from(Span::styled(format!("  {line}"), style)));
                }
            }
        }

        // Keep the tags in lockstep with whatever the entry pushed: a tool
        // card's header line is clickable, an image block's rows are paintable,
        // everything else is text.
        tags.resize(lines.len(), RowTag::Text);
        if is_tool && header_at < lines.len() {
            tags[header_at] = RowTag::Card(index);
        }
        if let Some((at, slot, rows)) = image_at {
            for row in 0..rows {
                tags[at + row as usize] = RowTag::Image { slot, row };
            }
        }
    }

    Rendered {
        lines,
        tags,
        blocks,
        empty: first,
    }
}

/// Human label + one-line summary for a tool call. `spawn_subagent` reads as
/// "subagent <name> · <task>" so the user can see which subagent is working
/// and on what; every other tool is its own name plus its JSON args. The
/// summary is returned untruncated — callers clip it to their width.
fn tool_label(name: &str, args: &serde_json::Value) -> (String, String) {
    if name == "spawn_subagent" {
        let who = args.get("subagent").and_then(|v| v.as_str()).unwrap_or("?");
        let task = args.get("task").and_then(|v| v.as_str()).unwrap_or("");
        let summary = if task.is_empty() {
            who.to_string()
        } else {
            format!("{who} · {task}")
        };
        ("subagent".to_string(), summary)
    } else if args.is_null() {
        (name.to_string(), String::new())
    } else {
        (
            name.to_string(),
            serde_json::to_string(args).unwrap_or_default(),
        )
    }
}

/// Render one tool invocation as a compact single-line card: status glyph,
/// tool name in accent, truncated args in dim. Output expands below only
/// when relevant (short successful outputs, or Ctrl-T).
fn tool_card_lines(
    lines: &mut Vec<Line<'static>>,
    name: &str,
    args: &serde_json::Value,
    output: Option<&str>,
    is_error: bool,
    collapsed: bool,
    tick: u64,
) {
    const MAX_OUTPUT_LINES: usize = 200;

    let glyph = match (output, is_error) {
        (None, _) => Span::styled(
            SPINNER[(tick as usize) % SPINNER.len()].to_string(),
            accent(),
        ),
        (Some(_), false) => Span::styled("✓", Style::default().fg(TEXT_DIM)),
        (Some(_), true) => Span::styled("✗", Style::default().fg(Color::White).bold()),
    };

    let (label, summary) = tool_label(name, args);
    let summary = truncate_width(&summary, 64);
    let mut card = vec![glyph, Span::raw(" "), Span::styled(label, accent())];
    if !summary.is_empty() {
        card.push(Span::styled(format!("  {summary}"), dim()));
    }
    let hidden = output.map(|text| text.lines().count()).unwrap_or(0);
    if collapsed && hidden > 0 {
        card.push(Span::styled(format!("  +{hidden} lines"), dim().italic()));
    }
    lines.push(Line::from(card));

    if !collapsed && let Some(text) = output {
        let body = Style::default().fg(TEXT_DIM);
        let out_lines: Vec<&str> = text.lines().collect();
        for line in out_lines.iter().take(MAX_OUTPUT_LINES) {
            lines.push(Line::from(Span::styled(format!("  {line}"), body)));
        }
        if out_lines.len() > MAX_OUTPUT_LINES {
            lines.push(Line::from(Span::styled(
                format!("  … +{} lines", out_lines.len() - MAX_OUTPUT_LINES),
                dim(),
            )));
        }
    }
}

/// Compact todo overlay just above the composer (`/todos`, auto-shown on
/// the first todo update). A few rows tall, full input width — Claude Code
/// style — so the chat keeps its full width instead of losing a side panel.
/// Glyphs: ✓ completed (dim, struck-through), ▸ in progress (accent), ☐ pending.
fn draw_todo_overlay(frame: &mut Frame, app: &App, input_area: Rect) {
    let (done, total) = crate::tools::todo::progress(&app.todos);
    let item_rows = if app.todos.is_empty() {
        1u16
    } else {
        app.todos.len() as u16
    };
    // +2 for the rounded border. Cap so a long list can't swallow the chat;
    // leave at least one row of transcript above when the terminal is tall.
    let max_height = input_area.y.saturating_sub(1).clamp(3, 12);
    let height = (item_rows + 2).clamp(3, max_height);
    let area = Rect {
        x: input_area.x,
        y: input_area.y.saturating_sub(height),
        width: input_area.width,
        height,
    }
    .intersection(frame.area());
    if area.height < 3 || area.width < 8 {
        return;
    }
    frame.render_widget(Clear, area);

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(dim())
        .title(Line::from(vec![
            Span::styled(" ≡ ", accent()),
            Span::styled(
                format!("todos {done}/{total}"),
                Style::default().fg(TEXT_DIM),
            ),
            Span::styled(" · esc", dim()),
        ]));
    let inner = block.inner(area);
    let inner_width = inner.width as usize;
    let visible = inner.height as usize;
    let lines: Vec<Line<'static>> = if app.todos.is_empty() {
        vec![Line::from(Span::styled("(empty)", dim().italic()))]
    } else {
        // Prefer the in-progress item and its neighbors when the list is
        // taller than the overlay: scroll so the current work stays visible.
        let focus = app
            .todos
            .iter()
            .position(|item| item.status == crate::tools::todo::TodoStatus::InProgress)
            .unwrap_or(0);
        let start = if app.todos.len() <= visible {
            0
        } else {
            focus
                .saturating_sub(visible.saturating_sub(1) / 2)
                .min(app.todos.len().saturating_sub(visible))
        };
        app.todos
            .iter()
            .skip(start)
            .take(visible)
            .map(|item| {
                use crate::tools::todo::TodoStatus;
                let (glyph_style, text_style) = match item.status {
                    TodoStatus::Completed => (dim(), dim().add_modifier(Modifier::CROSSED_OUT)),
                    TodoStatus::InProgress => (accent(), accent().bold()),
                    TodoStatus::Pending => (dim(), Style::default().fg(TEXT_DIM)),
                };
                truncate_line(
                    Line::from(vec![
                        Span::styled(format!("{} ", item.status.glyph()), glyph_style),
                        Span::styled(item.content.clone(), text_style),
                    ]),
                    inner_width,
                )
            })
            .collect()
    };
    frame.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
}

/// Git diff sidebar (`/diff`): separated from the chat by a single dim
/// rule, syntax-highlighted (foreground colors only). Lines wider than
/// the sidebar are cut with a dim `…` instead of clipping silently.
fn draw_diff_sidebar(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::new()
        .borders(Borders::LEFT)
        .border_style(dim())
        .title(Line::from(vec![
            Span::styled(" ± ", accent()),
            Span::styled("git diff", Style::default().fg(TEXT_DIM)),
            Span::styled(" · esc closes", dim()),
        ]));
    let inner = block.inner(area);
    let inner_width = inner.width as usize;
    let inner_height = inner.height as usize;
    let lines: Vec<Line<'static>> = highlight_diff(&app.diff_text)
        .lines
        .into_iter()
        .map(|line| truncate_line(line, inner_width))
        .collect();
    // Clamp the scroll to the content so PgDn can't strand the view past the
    // end; the key handler lets diff_scroll grow unbounded (mirroring the
    // transcript), and render is the single source of truth for the bound.
    let max_scroll = lines.len().saturating_sub(inner_height);
    let scroll = (app.diff_scroll as usize).min(max_scroll);
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .scroll((scroll as u16, 0))
            .block(block),
        area,
    );
    // Quiet "↕ N more" hint in the top-right when the diff overflows, so it's
    // discoverable that there's more below (and that PgUp/PgDn page it).
    if max_scroll > 0 {
        let remaining = max_scroll - scroll;
        if remaining > 0 {
            let label = format!(" ↓ {remaining} more ");
            let label_width = label.width() as u16;
            if inner.width > label_width {
                let hint = Rect {
                    x: inner.x + inner.width - label_width,
                    y: inner.y,
                    width: label_width,
                    height: 1,
                };
                frame.render_widget(Paragraph::new(Line::from(Span::styled(label, dim()))), hint);
            }
        }
    }
}

/// Bottom status line: model, mode, and turn state on the left; contextual
/// key hints on the right. One quiet line, no background fill.
/// An indeterminate, indicatif-style block bar: a lit window of `█` slides
/// across a dim `░` track, wrapping. Driven by `tick` so it animates frame to
/// frame without knowing a total (compaction is one opaque LLM call).
fn indeterminate_bar(width: usize, tick: u64) -> Line<'static> {
    let width = width.max(4);
    let window = (width / 5).max(3);
    let offset = (tick as usize) % width;
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_lit: Option<bool> = None;
    for i in 0..width {
        // Lit cells are the `window` columns starting at `offset`, wrapping.
        let lit = (i + width - offset) % width < window;
        if run_lit != Some(lit) {
            if let Some(prev) = run_lit {
                let style = if prev { accent() } else { dim() };
                spans.push(Span::styled(std::mem::take(&mut run), style));
            }
            run_lit = Some(lit);
        }
        run.push(if lit { '█' } else { '░' });
    }
    if let Some(prev) = run_lit {
        let style = if prev { accent() } else { dim() };
        spans.push(Span::styled(run, style));
    }
    Line::from(spans)
}

fn draw_status_bar(frame: &mut Frame, app: &App, area: Rect) {
    // Compaction owns the status line while it runs: a label plus the animated
    // bar, full width.
    if app.compacting {
        let label = " compacting… ";
        let bar_width = (area.width as usize)
            .saturating_sub(label.width() + 1)
            .max(4);
        let mut spans = vec![Span::styled(label, accent().bold())];
        spans.extend(indeterminate_bar(bar_width, app.tick).spans);
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }
    let spinner = SPINNER[(app.tick as usize) % SPINNER.len()];
    let mut spans = vec![
        Span::raw(" "),
        model_span(app),
        Span::styled(" · ", dim()),
        mode_span(app.status.mode),
    ];
    // Vim mode indicator: NORMAL stands out (bold accent), INSERT stays quiet.
    if let Some(label) = app.vim.label() {
        spans.push(Span::styled(" · ", dim()));
        let style = if app.vim.mode == VimMode::Normal {
            accent().bold()
        } else {
            dim()
        };
        spans.push(Span::styled(label, style));
    }
    if app.omakase {
        spans.push(Span::styled(" · ", dim()));
        spans.push(Span::styled("OMAKASE", accent().bold()));
    } else if app.plan_mode {
        spans.push(Span::styled(" · ", dim()));
        spans.push(Span::styled("PLAN", accent().bold()));
    }
    spans.push(Span::styled(" · ", dim()));
    spans.push(Span::styled(format_cwd(&app.project_root, 32), dim()));
    // Context meter: tokens that will load into the next model call — last
    // reported prompt size, or a post-compact / post-clear estimate. Not the
    // session-lifetime sum (that double-counts multi-step history and stays
    // inflated after /clear).
    if app.status.context_tokens > 0 {
        spans.push(Span::styled(" · ", dim()));
        spans.push(Span::styled(
            crate::usage::format_tokens(app.status.context_tokens),
            dim(),
        ));
    }
    if let Some(label) = &app.rebuilding {
        spans.push(Span::styled(" · ", dim()));
        spans.push(Span::styled(format!("{spinner} "), accent()));
        spans.push(Span::styled(format!("{label}…"), dim().italic()));
    } else if app.status.busy {
        let elapsed = app
            .turn_started
            .map(|started| started.elapsed().as_secs())
            .unwrap_or(0);
        spans.push(Span::styled(" · ", dim()));
        spans.push(Span::styled(format!("{spinner} "), accent()));
        // Capped budget shows the denominator; the default unlimited budget has
        // none to show, so the step is just a count.
        let step = match app.status.max_steps.cap() {
            Some(cap) => format!("step {}/{cap}", app.status.step),
            None => format!("step {}", app.status.step),
        };
        spans.push(Span::styled(format!("{step} · {elapsed}s"), dim()));
        // How many user prompts are waiting behind this turn.
        if !app.message_queue.is_empty() {
            spans.push(Span::styled(" · ", dim()));
            spans.push(Span::styled(
                format!("queued {}", app.message_queue.len()),
                accent(),
            ));
        }
    }
    // Background tasks (`/bashes`): a persistent marker while any are
    // running, so a detached command doesn't silently vanish from view.
    if app.status.background_tasks > 0 {
        spans.push(Span::styled(" · ", dim()));
        spans.push(Span::styled(
            format!(
                "⏵ {} bg task{}",
                app.status.background_tasks,
                if app.status.background_tasks == 1 {
                    ""
                } else {
                    "s"
                }
            ),
            accent(),
        ));
    }
    // Backgrounded subagents (`spawn_subagent` with `background: true`):
    // same persistent marker, so a delegated task stays visible while the
    // user is free to keep talking.
    if app.status.background_subagents > 0 {
        spans.push(Span::styled(" · ", dim()));
        spans.push(Span::styled(
            format!(
                "⏵ {} bg subagent{}",
                app.status.background_subagents,
                if app.status.background_subagents == 1 {
                    ""
                } else {
                    "s"
                }
            ),
            accent(),
        ));
    }
    // MCP is still connecting in the background: a transient marker, shown
    // alongside the busy/step indicator (a turn can start before tools arrive)
    // so the missing-tools window isn't a silent surprise. Vanishes when the
    // connect finishes.
    if app.mcp_connecting {
        spans.push(Span::styled(" · ", dim()));
        spans.push(Span::styled(format!("{spinner} "), accent()));
        spans.push(Span::styled("connecting tools…", dim().italic()));
    }
    // A failed health probe leaves a persistent marker so the breakage survives
    // once the user starts typing and the welcome screen is gone.
    if app.provider_health_error.is_some() {
        spans.push(Span::styled(" · ", dim()));
        spans.push(Span::styled(
            "⚠ provider",
            Style::default().fg(Color::White).bold(),
        ));
    }
    let line = Line::from(spans);
    let left_width = line.width() as u16;
    frame.render_widget(Paragraph::new(line), area);

    // Contextual key hints, right-aligned in a sub-rect so the left side is
    // never overdrawn.
    let hints = if let Some(review) = &app.plan_review {
        if review.feedback.is_some() {
            "type feedback · Enter reject · Esc back"
        } else {
            "y/Enter approve · n reject · ↑↓ scroll"
        }
    } else if app.interview.is_some() {
        "1-9 pick · type answer · Enter next · Esc skip"
    } else if app.picker.is_some() {
        "↑↓ move · Enter select · Esc cancel"
    } else if !app.suggestions.is_empty() {
        "↑↓ select · Tab complete · Enter run"
    } else if app.show_diff {
        "PgUp/PgDn diff · Esc close"
    } else if app.status.busy {
        "PgUp/PgDn scroll · Enter queues"
    } else {
        "/ commands · ↑ history"
    };
    let width = hints.width() as u16 + 1;
    if area.width > left_width + width {
        let hint_area = Rect {
            x: area.right().saturating_sub(width),
            y: area.y,
            width,
            height: 1,
        };
        frame.render_widget(Paragraph::new(Span::styled(hints, dim())), hint_area);
    }
}

/// Columns available for composer text: one of left padding, two for the
/// prompt glyph, and one spare so the caret can sit just past a full row.
fn composer_budget(width: u16) -> usize {
    (width as usize).saturating_sub(4).max(1)
}

/// The composer buffer as chars for layout. In the inline provider-setup
/// prompt the API-key field is masked: each typed character renders as a
/// width-1 bullet (so the cursor math is unaffected) and the real key never
/// reaches the screen.
fn composer_chars(app: &App) -> Vec<char> {
    if app.prompt_is_masked() {
        vec!['•'; app.input.chars().count()]
    } else {
        app.input.chars().collect()
    }
}

/// Soft-wrap the composer buffer at `budget` display columns. Each visual row
/// is the half-open char range `[start, end)`: the buffer splits on hard line
/// breaks (a '\n' belongs to no row), then each logical line packs greedily by
/// display width. Wide chars never split across rows, and every row keeps at
/// least one char so a pathological budget cannot loop.
fn wrap_rows(chars: &[char], budget: usize) -> Vec<(usize, usize)> {
    let budget = budget.max(1);
    let breaks = chars
        .iter()
        .enumerate()
        .filter(|&(_, &c)| c == '\n')
        .map(|(i, _)| i);
    let mut rows = Vec::new();
    let mut ls = 0usize;
    for le in breaks.chain(std::iter::once(chars.len())) {
        let mut rs = ls;
        let mut used = 0usize;
        for (i, c) in chars.iter().enumerate().take(le).skip(ls) {
            let w = c.width().unwrap_or(0);
            if i > rs && used + w > budget {
                rows.push((rs, i));
                rs = i;
                used = 0;
            }
            used += w;
        }
        rows.push((rs, le));
        ls = le + 1;
    }
    rows
}

/// Map a cursor (char offset) to its visual (row, column-in-chars) position
/// in `rows` from [`wrap_rows`]. A cursor exactly on a soft-wrap boundary
/// belongs to the start of the next visual row (that is where the next char
/// would land); at a hard break or end of text it stays at the end of its row.
fn cursor_visual(rows: &[(usize, usize)], cursor: usize) -> (usize, usize) {
    for (ri, &(rs, re)) in rows.iter().enumerate() {
        // The next row continues this logical line iff it starts where this
        // row ends (a hard break consumes the '\n', leaving a gap of one).
        let continues = rows.get(ri + 1).is_some_and(|&(ns, _)| ns == re);
        if cursor < re || (cursor == re && !continues) {
            return (ri, cursor.saturating_sub(rs));
        }
    }
    (rows.len().saturating_sub(1), 0)
}

/// Input: a clean accent prompt bracketed by dim rules above and below — no
/// box. Soft-wraps long lines onto continuation rows and handles inline
/// ghost-text completion.
fn draw_input(frame: &mut Frame, app: &App, area: Rect) {
    if area.height < 3 || area.width < 6 {
        return;
    }
    let rule = Line::from(Span::styled("─".repeat(area.width as usize), dim()));

    // One column of left padding keeps the prompt aligned with the
    // transcript margin.
    let pad = 1usize;
    let prompt_width = 2usize;
    let budget = composer_budget(area.width);

    let chars = composer_chars(app);
    let cursor = app.cursor.min(chars.len());
    let normal = app.vim.is_normal();

    let rows = wrap_rows(&chars, budget);
    let (crow, ccol) = cursor_visual(&rows, cursor);

    // Vertical window: show a block of rows that keeps the cursor row in view.
    let content_h = (area.height as usize).saturating_sub(2).max(1);
    let voff = if crow < content_h {
        0
    } else {
        crow - content_h + 1
    };
    let last = (voff + content_h).min(rows.len());

    let block = Style::default().add_modifier(Modifier::REVERSED);
    let mut lines: Vec<Line> = vec![rule.clone()];
    let mut cursor_xy: Option<(u16, u16)> = None;

    for ri in voff..last {
        let (rs, re) = rows[ri];
        let row: &[char] = &chars[rs..re];
        let widths: Vec<usize> = row.iter().map(|c| c.width().unwrap_or(0)).collect();
        let is_cursor_row = ri == crow;

        // First row carries the prompt glyph; continuation rows (wrapped or
        // hard-broken) indent to match.
        let leading = if ri == 0 {
            Span::styled("❯ ", accent().bold())
        } else {
            Span::raw("  ")
        };
        let mut spans = vec![Span::raw(" "), leading];

        if normal && is_cursor_row {
            // Vim Normal mode paints its own block cursor (reversed cell) so the
            // mode is legible without a hardware caret.
            let rel = ccol.min(row.len());
            spans.push(Span::raw(row[..rel].iter().collect::<String>()));
            if rel < row.len() {
                spans.push(Span::styled(row[rel].to_string(), block));
                spans.push(Span::raw(row[rel + 1..].iter().collect::<String>()));
            } else {
                spans.push(Span::styled(" ", block));
            }
        } else {
            spans.push(Span::raw(row.iter().collect::<String>()));

            // Ghost text (command completion) only makes sense on a single-row
            // line with the cursor at the very end, where → can accept it.
            if is_cursor_row
                && !normal
                && rows.len() == 1
                && cursor == chars.len()
                && app.picker.is_none()
                && app.input_mode == InputMode::Command
                && let Some(spec) = app.suggestions.get(app.suggestion_index)
            {
                let typed = app.input.trim_start().strip_prefix('/').unwrap_or_default();
                if let Some(remainder) = spec.name.strip_prefix(typed) {
                    let mut ghost = remainder.to_string();
                    if !spec.args.is_empty() {
                        ghost.push(' ');
                        ghost.push_str(&spec.args);
                    }
                    let used: usize = widths.iter().sum();
                    let room = budget.saturating_sub(used);
                    if !ghost.is_empty() && room > 0 {
                        let ghost: String = ghost.chars().take(room).collect();
                        spans.push(Span::styled(ghost, dim().italic()));
                    }
                }
            }

            if is_cursor_row && !normal {
                let cols: usize = widths[..ccol.min(widths.len())].iter().sum();
                let x = area.x + (pad + prompt_width) as u16 + cols as u16;
                let y = area.y + 1 + (ri - voff) as u16;
                cursor_xy = Some((x, y));
            }
        }
        lines.push(Line::from(spans));
    }
    lines.push(rule);

    frame.render_widget(Paragraph::new(Text::from(lines)), area);

    // In Normal mode the block cursor above is the only cursor; otherwise place
    // the terminal's caret on the cursor row.
    if !normal
        && app.picker.is_none()
        && app.plan_review.is_none()
        && app.interview.is_none()
        && let Some((x, y)) = cursor_xy
    {
        frame.set_cursor_position(Position::new(x, y));
    }
}

/// Command-suggestion popup floating directly above the input rule.
fn draw_suggestions(frame: &mut Frame, app: &App, input_area: Rect) {
    if app.suggestions.is_empty() {
        return;
    }

    let rows = app.suggestions.len() as u16;
    let bottom = input_area.y;
    let height = (rows + 2).min(bottom);
    let area = Rect {
        x: input_area.x,
        y: bottom.saturating_sub(height),
        width: input_area.width,
        height,
    }
    .intersection(frame.area());
    if area.height < 3 || area.width < 4 {
        return;
    }
    frame.render_widget(Clear, area);

    let usage_width = app
        .suggestions
        .iter()
        .map(|spec| spec.name.len() + spec.args.len() + 2)
        .max()
        .unwrap_or(0);
    let inner_width = area.width.saturating_sub(2) as usize;
    // Columns left for the description: marker + padded usage + gap.
    let description_room = inner_width.saturating_sub(usage_width + 5);

    // Window the rows so the ❯ selection stays visible on short terminals
    // (selection pinned to the bottom edge while moving down).
    let visible_rows = area.height.saturating_sub(2) as usize;
    let start = if app.suggestion_index >= visible_rows {
        app.suggestion_index + 1 - visible_rows
    } else {
        0
    };

    let lines: Vec<Line<'static>> = app
        .suggestions
        .iter()
        .enumerate()
        .skip(start)
        .take(visible_rows)
        .map(|(index, spec)| {
            let selected = index == app.suggestion_index;
            let (marker, name_style) = if selected {
                ("❯ ", accent().bold())
            } else {
                ("  ", Style::default().fg(TEXT_DIM))
            };
            let usage = format!("/{} {}", spec.name, spec.args);
            Line::from(vec![
                Span::styled(marker, accent()),
                Span::styled(format!("{usage:<usage_width$}"), name_style),
                Span::styled(
                    format!("  {}", truncate_width(&spec.description, description_room)),
                    dim(),
                ),
            ])
        })
        .collect();

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(dim());
    frame.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
}

/// Centered modal for the model / mode / rewind / subagent picker.
fn draw_picker(frame: &mut Frame, app: &App) {
    let Some(picker) = &app.picker else {
        return;
    };

    let frame_area = frame.area();
    let width = (frame_area.width.saturating_sub(8)).clamp(24, 56);
    let max_rows = frame_area.height.saturating_sub(6).max(1) as usize;
    let height = picker.items.len().min(max_rows) as u16 + 2;
    let area = Rect {
        x: frame_area.x + (frame_area.width.saturating_sub(width)) / 2,
        y: frame_area.y + (frame_area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
    .intersection(frame_area);
    if area.height < 3 || area.width < 4 {
        return;
    }
    frame.render_widget(Clear, area);

    // Window the items so the selection stays visible when the list
    // overflows (selection pinned to the bottom edge while scrolling down).
    let rows = area.height.saturating_sub(2) as usize;
    let start = if picker.selected >= rows {
        picker.selected + 1 - rows
    } else {
        0
    };
    let inner_width = area.width.saturating_sub(2) as usize;

    let lines: Vec<Line<'static>> = picker
        .items
        .iter()
        .enumerate()
        .skip(start)
        .take(rows)
        .map(|(index, item)| {
            let selected = index == picker.selected;
            let marker = if selected { "❯ " } else { "  " };
            let value_style = if selected {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(TEXT_DIM)
            };
            // Ellipsize long model tags so the current marker stays visible.
            let suffix = if item.current { " ●".width() } else { 0 };
            let value_room = inner_width.saturating_sub(2 + suffix + 1);
            let value = truncate_width(&item.value, value_room);
            // Width consumed so far: marker (2) + value + the current-marker.
            let consumed = 2 + value.width() + suffix;
            let mut spans = vec![
                Span::styled(marker, accent()),
                Span::styled(value, value_style),
            ];
            if item.current {
                spans.push(Span::styled(" ●", Style::default().fg(Color::White)));
            }
            // Truncate the detail to the room left on the line (after a two-space
            // gap) so long descriptions never spill past the modal border.
            if !item.detail.is_empty() {
                let room = inner_width.saturating_sub(consumed + 2);
                if room > 0 {
                    let detail = truncate_width(&item.detail, room);
                    spans.push(Span::styled(format!("  {detail}"), dim()));
                }
            }
            Line::from(spans)
        })
        .collect();

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(dim())
        .title(Line::from(vec![
            Span::styled(" ✦", accent()),
            Span::styled(picker.title.clone(), Style::default().fg(TEXT_DIM)),
        ]))
        .title_bottom(Line::from(Span::styled(picker.footer_hint(), dim())).centered());
    frame.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
}

/// A dim "· text" placeholder row for an empty modal section.
fn dash_bullet(text: &str, style: Style) -> Line<'static> {
    Line::from(Span::styled(format!("· {text}"), style))
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Compact "how long ago" label: `12s`, `4m`, `2h`, `3d`.
fn fmt_age(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// Machine-wide session manager (`/dashboard`): every live Wizard session on
/// the machine, grouped by state, refreshed from the registry while open.
/// Modal — ↑/↓ move the selection, Esc/q close. Dispatch and attach arrive in
/// later milestones.
fn draw_dashboard(frame: &mut Frame, app: &App) {
    let area = frame.area();
    frame.render_widget(Clear, area);

    let count = app.sessions.len();
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(dim())
        .title(Line::from(vec![
            Span::styled(" ✦ ", accent()),
            Span::styled(
                "wizard sessions",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  ({count} live on this machine)"), dim()),
        ]))
        .title_bottom(
            Line::from(Span::styled(" ↑↓ select · Ctrl-X stop · Esc close ", dim())).centered(),
        );
    let outer = block.inner(area);
    frame.render_widget(block, area);
    if outer.width < 8 || outer.height < 5 {
        return;
    }
    // On a wide terminal, a peek panel of the selected session sits on the
    // right; the list and dispatch input take the left.
    let (body_area, peek_area) = if outer.width >= 80 {
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)])
                .areas(outer);
        (left, Some(right))
    } else {
        (outer, None)
    };
    if let Some(peek_area) = peek_area {
        draw_peek(frame, app, peek_area);
    }
    // Reserve the bottom rows for the dispatch input.
    let [inner, input_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(2)]).areas(body_area);
    let width = inner.width as usize;
    let spinner = SPINNER[(app.tick as usize) % SPINNER.len()];
    let now = now_unix();

    let mut lines: Vec<Line<'static>> = Vec::new();
    if app.sessions.is_empty() {
        lines.push(dash_bullet("no running sessions", dim().italic()));
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "every running wizard registers here — start another to see it appear",
            dim().italic(),
        )));
    } else {
        // Sessions arrive pre-sorted by state then recency; emit a group header
        // whenever the state group changes.
        let mut current_group = "";
        for (i, session) in app.sessions.iter().enumerate() {
            let group = session.state.group();
            if group != current_group {
                if !lines.is_empty() {
                    lines.push(Line::raw(""));
                }
                lines.push(Line::from(Span::styled(
                    group.to_string(),
                    accent().add_modifier(Modifier::BOLD),
                )));
                current_group = group;
            }

            let selected = i == app.dashboard_selected;
            let marker = if selected { "❯ " } else { "  " };
            let (icon, icon_style) = match session.state {
                SessionState::Working => (spinner.to_string(), accent()),
                SessionState::NeedsInput => ("?".to_string(), accent().bold()),
                SessionState::Idle => ("·".to_string(), dim()),
                SessionState::Completed => ("✓".to_string(), Style::default().fg(TEXT_DIM)),
                SessionState::Failed => ("✗".to_string(), Style::default().fg(Color::White).bold()),
            };
            let name_style = if selected {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(TEXT_DIM)
            };
            // Mark this very session so the user can spot which row is them.
            let you = if session.id == app.session_id {
                " (this one)"
            } else {
                ""
            };
            let age = fmt_age(now.saturating_sub(session.updated_unix));
            lines.push(truncate_line(
                Line::from(vec![
                    Span::styled(marker, accent()),
                    Span::styled(format!("{icon} "), icon_style),
                    Span::styled(format!("{}{you}", session.name), name_style),
                    Span::styled(format!("  {}", session.activity), dim()),
                    Span::styled(format!("  · {} · {age}", session.mode), dim()),
                ]),
                width,
            ));
        }
    }

    let max = inner.height as usize;
    if lines.len() > max {
        lines.truncate(max);
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);

    // Dispatch input: type a task + Enter to spawn a background session.
    let prompt_line = truncate_line(
        Line::from(vec![
            Span::styled("› ", accent()),
            if app.dashboard_input.is_empty() {
                Span::styled("dispatch a task…", dim().italic())
            } else {
                Span::styled(app.dashboard_input.clone(), Style::default().fg(CODE))
            },
        ]),
        input_area.width as usize,
    );
    let hint = Line::from(Span::styled(
        "Enter dispatch · type to compose",
        dim().italic(),
    ));
    frame.render_widget(
        Paragraph::new(Text::from(vec![prompt_line, hint])),
        input_area,
    );
}

/// The dashboard's peek panel: the selected session's recent transcript,
/// role-prefixed, pinned to the latest output. Read-only.
fn draw_peek(frame: &mut Frame, app: &App, area: Rect) {
    let title = app
        .sessions
        .get(app.dashboard_selected)
        .map(|session| format!(" peek · {} ", session.name))
        .unwrap_or_else(|| " peek ".to_string());
    let pblock = Block::new()
        .borders(Borders::LEFT)
        .border_style(dim())
        .title(Line::from(Span::styled(title, accent())));
    let pinner = pblock.inner(area);
    frame.render_widget(pblock, area);
    if pinner.width < 2 || pinner.height < 1 {
        return;
    }
    let pwidth = pinner.width as usize;
    let height = pinner.height as usize;

    if app.peek_lines.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled("(no transcript yet)", dim().italic())),
            pinner,
        );
        return;
    }

    // Build only the visible tail: walk messages newest-first, emit each
    // message's lines bottom-up, and stop once the panel is full. This keeps
    // rendering O(panel height) instead of O(whole transcript).
    let mut lines: Vec<Line<'static>> = Vec::new();
    'outer: for (role, text) in app.peek_lines.iter().rev() {
        let role_style = match role.as_str() {
            "user" => accent().add_modifier(Modifier::BOLD),
            "assistant" => Style::default().fg(TEXT_DIM).add_modifier(Modifier::BOLD),
            _ => dim().add_modifier(Modifier::BOLD),
        };
        let mut block: Vec<Line<'static>> =
            vec![Line::from(Span::styled(role.clone(), role_style))];
        for line in text.lines() {
            block.push(truncate_line(
                Line::from(Span::styled(
                    line.to_string(),
                    Style::default().fg(TEXT_DIM),
                )),
                pwidth,
            ));
        }
        for line in block.into_iter().rev() {
            lines.push(line);
            if lines.len() >= height {
                break 'outer;
            }
        }
    }
    lines.reverse();
    frame.render_widget(Paragraph::new(Text::from(lines)), pinner);
}

/// Most rail rows drawn at once. Past this the rail scrolls around the
/// selection rather than eating the transcript.
const MAX_RAIL_ROWS: usize = 5;

/// Rows the rail needs: one per subagent, capped, plus a row for the "+N more"
/// marker when it is capped. Zero when nothing has been delegated — the rail
/// costs no screen space until there is something to show.
fn rail_height(app: &App) -> u16 {
    if app.panes.is_empty() {
        return 0;
    }
    let shown = app.panes.len().min(MAX_RAIL_ROWS);
    let overflow = usize::from(app.panes.len() > MAX_RAIL_ROWS);
    (shown + overflow) as u16
}

/// The subagent rail: one dot per run, directly under the composer.
///
/// ```text
///   ◉ researcher   read_file                     0:12 +3
/// ❯ ● reviewer     Checking token expiry…        0:04
///   ✔ tester       214 passed                    1:31 +1
/// ```
///
/// ↓ from the composer focuses it, ↑/↓ move, Enter opens the selected run as a
/// full chat view. `❯` marks the selection while the rail has focus. `+N` is
/// the unread count: what that subagent did while you were looking elsewhere.
fn draw_rail(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 || area.width < 8 {
        return;
    }
    let focused = app.rail_focus;
    let selected = focused.or(app.attached);

    // Scroll the window so the selection stays visible once there are more
    // runs than rows.
    let visible = app.panes.len().min(MAX_RAIL_ROWS);
    let start = match selected {
        Some(index) if index >= visible => index + 1 - visible,
        _ => 0,
    };
    let end = (start + visible).min(app.panes.len());

    let mut lines: Vec<Line<'static>> = Vec::new();
    for (index, pane) in app.panes.iter().enumerate().take(end).skip(start) {
        let is_selected = selected == Some(index);
        // Only the *focused* rail shows a cursor; when focus is in the
        // composer the rail is just a status readout.
        let cursor = if is_selected && focused.is_some() {
            "❯"
        } else {
            " "
        };
        let dot_style = match pane.status {
            PaneStatus::Running => accent(),
            PaneStatus::Done => Style::default().fg(Color::Green),
            PaneStatus::Failed => Style::default().fg(Color::Red),
        };
        let name_style = if is_selected {
            Style::default().fg(ACCENT).bold()
        } else {
            dim()
        };

        let elapsed = pane.elapsed().as_secs();
        let clock = format!("{}:{:02}", elapsed / 60, elapsed % 60);
        let unread = if pane.unread > 0 && Some(index) != app.attached {
            format!(" +{}", pane.unread)
        } else {
            String::new()
        };

        // Name column is fixed-width so the activity text lines up down the
        // rail and reads as a column, not a ragged list.
        let name = clip(&pane.name, 12);
        let meta_width = clock.len() + unread.len() + 4;
        let activity_width = (area.width as usize).saturating_sub(18 + meta_width).max(8);
        let activity = clip(
            pane.activity().trim().lines().next().unwrap_or(""),
            activity_width,
        );

        lines.push(Line::from(vec![
            Span::styled(format!(" {cursor} "), accent()),
            Span::styled(format!("{} ", pane.glyph(app.tick)), dot_style),
            Span::styled(format!("{name:<12} "), name_style),
            Span::styled(format!("{activity:<activity_width$} "), dim()),
            Span::styled(clock, dim()),
            Span::styled(unread, accent().bold()),
        ]));
    }

    if app.panes.len() > MAX_RAIL_ROWS {
        let hidden = app.panes.len() - visible;
        lines.push(Line::from(Span::styled(
            format!("   +{hidden} more"),
            dim().italic(),
        )));
    }

    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

/// A subagent's pane: its own conversation, rendered with the same machinery as
/// the main chat, under a header naming the run. Esc goes back.
fn draw_pane(frame: &mut Frame, app: &App, pane: &SubagentPane, area: Rect) {
    // The pane owns the screen, so no main-transcript card is clickable.
    app.card_hits.borrow_mut().clear();

    let [header_area, body_area] =
        Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).areas(area);

    let status = match pane.status {
        PaneStatus::Running => ("running", accent()),
        PaneStatus::Done => ("done", Style::default().fg(Color::Green)),
        PaneStatus::Failed => ("failed", Style::default().fg(Color::Red)),
    };
    let elapsed = pane.elapsed().as_secs();
    let steps = if pane.steps == 1 {
        "1 step".to_string()
    } else {
        format!("{} steps", pane.steps)
    };
    let mut header = vec![
        Span::styled(" ▌ ", accent()),
        Span::styled(pane.name.clone(), Style::default().fg(ACCENT).bold()),
        Span::styled(" · ", dim()),
        Span::styled(status.0, status.1),
        Span::styled(
            format!(" · {}:{:02} · {steps}", elapsed / 60, elapsed % 60),
            dim(),
        ),
    ];
    if pane.bg.is_none() {
        // Worth flagging: the parent turn is blocked until this one reports.
        header.push(Span::styled(" · foreground", dim().italic()));
    }
    let hint = if app.panes.len() > 1 {
        "esc back to chat · ↑↓ next agent · shift+↑↓ scroll"
    } else {
        "esc back to chat · ↑↓ scroll"
    };
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::from(header),
            Line::from(vec![
                Span::styled("   ", dim()),
                Span::styled(
                    clip(&pane.task, area.width.saturating_sub(6) as usize),
                    dim().italic(),
                ),
                Span::styled(format!("  {hint}"), dim()),
            ]),
        ])),
        header_area,
    );

    let inner = body_area.inner(Margin {
        horizontal: 1,
        vertical: 0,
    });
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let inner_width = inner.width as usize;
    let inner_height = inner.height as usize;

    let mut cache = app.images.borrow_mut();
    let content_width = inner_width.saturating_sub(2);
    let rendered = entries_text(
        &pane.transcript,
        app.tick,
        &mut cache,
        image_box(inner),
        content_width,
    );
    let (mut lines, mut row_tags) = wrap_tagged(rendered.lines, rendered.tags, inner_width);
    if lines.is_empty() {
        let spinner = SPINNER[(app.tick as usize) % SPINNER.len()];
        lines.push(Line::from(vec![
            Span::styled(format!("{spinner} "), accent()),
            Span::styled("starting…", dim().italic()),
        ]));
    } else if pane.status == PaneStatus::Running {
        // Same live tail as the main chat, so a running pane reads as alive.
        let spinner = SPINNER[(app.tick as usize) % SPINNER.len()];
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled(format!("{spinner} "), accent()),
            Span::styled("working…", dim().italic()),
        ]));
    }
    row_tags.resize(lines.len(), RowTag::Text);

    // Stick-to-bottom like the main transcript: follow the live tail by
    // default; once the user scrolls up, hold their top-anchored offset so
    // PageUp/Shift+↑ stay put while the run keeps writing.
    let total = lines.len();
    let max_scroll = total.saturating_sub(inner_height);
    pane.max_scroll.set(max_scroll as u16);
    let start = if pane.scroll_follow || max_scroll == 0 {
        max_scroll
    } else {
        (pane.scroll as usize).min(max_scroll)
    };
    let end = (start + inner_height).min(total);
    frame.render_widget(
        Paragraph::new(Text::from(lines[start..end].to_vec())),
        inner,
    );
    // A subagent's images belong to its run, so they are drawn here — in its
    // pane — and nowhere else.
    if !overlay_open(app) {
        paint_images(
            frame,
            inner,
            &row_tags[start..end],
            &rendered.blocks,
            &mut cache,
        );
    }
}

/// Plan-review modal (plan mode): the plan markdown with a verdict footer.
/// The turn is paused inside `exit_plan` until the user answers, so this
/// floats above everything else. While rejecting, a feedback line replaces
/// the bottom edge of the body.
fn draw_plan_review(frame: &mut Frame, app: &App) {
    let Some(review) = &app.plan_review else {
        return;
    };

    let frame_area = frame.area();
    let width = frame_area.width.saturating_sub(6).clamp(24, 100);
    let height = frame_area.height.saturating_sub(2).max(5);
    let area = Rect {
        x: frame_area.x + (frame_area.width.saturating_sub(width)) / 2,
        y: frame_area.y + (frame_area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
    .intersection(frame_area);
    if area.height < 5 || area.width < 10 {
        return;
    }
    frame.render_widget(Clear, area);

    let hints = if review.feedback.is_some() {
        " feedback · Enter reject · Esc back "
    } else {
        " y approve · n reject · ↑↓ scroll "
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(dim())
        .title(Line::from(vec![
            Span::styled(" ✦", accent()),
            Span::styled(" plan review ", Style::default().fg(TEXT_DIM)),
        ]))
        .title_bottom(Line::from(Span::styled(hints, dim())).centered());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Body: the plan, wrapped and scrolled; the bottom line is reserved for
    // the feedback input while rejecting.
    let body_area = if review.feedback.is_some() {
        Rect {
            height: inner.height.saturating_sub(1),
            ..inner
        }
    } else {
        inner
    };
    if body_area.height > 0 {
        let width = body_area.width as usize;
        // Plan markdown (incl. tables) lays out to the review body width so
        // tables don't soft-wrap mid-grid.
        let lines = wrap_lines(render_markdown_at(&review.plan, width), width);
        let max_scroll = lines.len().saturating_sub(body_area.height as usize);
        let scroll = (review.scroll as usize).min(max_scroll);
        let visible: Vec<Line<'static>> = lines
            .into_iter()
            .skip(scroll)
            .take(body_area.height as usize)
            .collect();
        frame.render_widget(Paragraph::new(Text::from(visible)), body_area);
    }

    if let Some(feedback) = &review.feedback {
        let feedback_area = Rect {
            y: inner.bottom().saturating_sub(1),
            height: 1,
            ..inner
        };
        let budget =
            (feedback_area.width as usize).saturating_sub("rejection feedback ❯  ".width());
        let shown: String = feedback
            .chars()
            .rev()
            .take(budget)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("rejection feedback ❯ ", accent().bold()),
                Span::raw(shown),
                Span::styled("▍", dim()),
            ])),
            feedback_area,
        );
    }
}

/// Centered modal for the plan-mode interview: the agent's clarifying
/// questions with their answer-so-far status, and a free-text input for the
/// current question. The turn is paused inside the `interview` tool until the
/// user answers every question or dismisses the modal.
fn draw_interview(frame: &mut Frame, app: &App) {
    let Some(interview) = &app.interview else {
        return;
    };

    let frame_area = frame.area();
    let width = frame_area.width.saturating_sub(6).clamp(24, 92);
    let height = frame_area.height.saturating_sub(2).max(5);
    let area = Rect {
        x: frame_area.x + (frame_area.width.saturating_sub(width)) / 2,
        y: frame_area.y + (frame_area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
    .intersection(frame_area);
    if area.height < 5 || area.width < 10 {
        return;
    }
    frame.render_widget(Clear, area);

    let total = interview.questions.len();
    let title = format!(
        " question {} of {total} ",
        (interview.current + 1).min(total)
    );
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(dim())
        .title(Line::from(vec![
            Span::styled(" ✦", accent()),
            Span::styled(" interview ", Style::default().fg(TEXT_DIM)),
            Span::styled(title, dim()),
        ]))
        .title_bottom(
            Line::from(Span::styled(
                " 1-9 pick · type answer · Enter next · Esc skip ",
                dim(),
            ))
            .centered(),
        );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height < 2 {
        return;
    }

    // Body: every question with its status; the current one gets its options
    // and the live answer input. The input occupies the bottom line.
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (i, q) in interview.questions.iter().enumerate() {
        if i < interview.current {
            // Answered: show the question dimmed with its answer.
            let answer = interview.answers.get(i).map(String::as_str).unwrap_or("");
            let answer = if answer.trim().is_empty() {
                "(skipped)".to_string()
            } else {
                answer.to_string()
            };
            lines.push(Line::from(vec![
                Span::styled("✓ ", Style::default().fg(Color::Green)),
                Span::styled(q.question.clone(), dim()),
            ]));
            lines.push(Line::from(Span::styled(
                format!("    {answer}"),
                dim().italic(),
            )));
        } else if i == interview.current {
            lines.push(Line::from(vec![
                Span::styled("▶ ", accent().bold()),
                Span::styled(q.question.clone(), Style::default().fg(Color::White).bold()),
            ]));
            for (n, option) in q.options.iter().enumerate() {
                lines.push(Line::from(vec![
                    Span::styled(format!("    {}) ", n + 1), accent()),
                    Span::raw(option.clone()),
                ]));
            }
        } else {
            lines.push(Line::from(Span::styled(format!("  {}", q.question), dim())));
        }
    }

    let body_area = Rect {
        height: inner.height.saturating_sub(2),
        ..inner
    };
    if body_area.height > 0 {
        let wrapped = wrap_lines(Text::from(lines), body_area.width as usize);
        let skip = wrapped.len().saturating_sub(body_area.height as usize);
        let visible: Vec<Line<'static>> = wrapped.into_iter().skip(skip).collect();
        frame.render_widget(Paragraph::new(Text::from(visible)), body_area);
    }

    // Answer input on the bottom line, scrolled to keep the tail visible.
    let input_area = Rect {
        y: inner.bottom().saturating_sub(1),
        height: 1,
        ..inner
    };
    let prompt = "answer ❯ ";
    let budget = (input_area.width as usize).saturating_sub(prompt.width() + 1);
    let shown: String = interview
        .input
        .chars()
        .rev()
        .take(budget)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(prompt, accent().bold()),
            Span::raw(shown),
            Span::styled("▍", dim()),
        ])),
        input_area,
    );
}

/// Soft-wrap each source line on its own, fanning its tag out over the rows it
/// becomes, so a click — or an image block's row — still traces back to what put
/// it there. An image block's rows are blank, so they never wrap and stay one
/// row to one row.
fn wrap_tagged(
    lines: Vec<Line<'static>>,
    tags: Vec<RowTag>,
    width: usize,
) -> (Vec<Line<'static>>, Vec<RowTag>) {
    let mut wrapped: Vec<Line<'static>> = Vec::with_capacity(lines.len());
    let mut row_tags: Vec<RowTag> = Vec::with_capacity(tags.len());
    for (line, tag) in lines.into_iter().zip(tags) {
        let before = wrapped.len();
        wrapped.append(&mut wrap_lines(Text::from(vec![line]), width));
        row_tags.extend(std::iter::repeat_n(tag, wrapped.len() - before));
    }
    (wrapped, row_tags)
}

/// Paint the image blocks whose rows are on screen, into exactly those rows.
///
/// `tags` are the rows of `inner` that survived the scroll, in order, so a block
/// half-way off the top arrives here as the run of rows it has left — and
/// [`ImageCache::draw`] is told which row of the block that run starts at. Rows
/// the scroll took are never painted, which is the whole reason an image here
/// cannot smear across the screen.
fn paint_images(
    frame: &mut Frame,
    inner: Rect,
    tags: &[RowTag],
    blocks: &[ImageBlock],
    cache: &mut ImageCache,
) {
    let mut row = 0usize;
    while row < tags.len() {
        let RowTag::Image { slot, row: top } = tags[row] else {
            row += 1;
            continue;
        };
        // The run of this block's rows that made it onto the screen.
        let mut height = 1u16;
        loop {
            match tags.get(row + height as usize) {
                Some(RowTag::Image {
                    slot: next,
                    row: at,
                }) if *next == slot && *at == top.saturating_add(height) => {
                    height += 1;
                }
                _ => break,
            }
        }
        if let Some(block) = blocks.get(slot) {
            let at = Rect {
                x: inner.x + IMAGE_INDENT,
                y: inner.y + row as u16,
                width: block.cols.min(inner.width.saturating_sub(IMAGE_INDENT)),
                height,
            };
            cache.draw(frame.buffer_mut(), at, block, top);
        }
        row += height as usize;
    }
}

/// Wrap styled lines at `width` display columns (wide CJK/emoji glyphs
/// count as two) so the transcript can be pinned exactly to its bottom.
/// Wrapping is word-aware: a line breaks at the last space that fits, and
/// only falls back to splitting mid-word when a single word exceeds the
/// content width. Continuation lines keep the hanging indent of their
/// source line (see [`hanging_indent`]), so gutter-indented content stays
/// aligned under its text column. A wide char that no longer fits wraps to
/// the next line first; zero-width chars (combining marks) always stay
/// with their base char.
fn wrap_lines(text: Text<'static>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut out: Vec<Line<'static>> = Vec::new();
    for line in text.lines {
        if line.width() <= width {
            out.push(line);
            continue;
        }
        let indent = hanging_indent(&line).min(width.saturating_sub(1));
        let mut wrapper = LineWrapper::new(width, indent, &line);
        for span in line.spans {
            let style = span.style;
            for ch in span.content.chars() {
                wrapper.feed(ch, style);
            }
        }
        out.append(&mut wrapper.finish());
    }
    out
}

/// Hanging indent (in display columns) for wrapped continuations of `line`:
/// its leading spaces plus one optional short gutter mark — at most two
/// columns of non-alphanumeric glyphs followed by a space, e.g. `❯ `, `· `,
/// `✓ `, `  • `, `▌ `. This is how [`gutter_block`], tool cards, notices,
/// and markdown bullets communicate their text column, so continuation
/// lines stay aligned under it. Lines without such a prefix wrap to
/// column 0.
fn hanging_indent(line: &Line) -> usize {
    let mut chars = line.spans.iter().flat_map(|span| span.content.chars());
    let mut indent = 0usize;
    let mut next = chars.next();
    while let Some(ch) = next {
        if ch != ' ' {
            break;
        }
        indent += 1;
        next = chars.next();
    }
    let mut mark = 0usize;
    while let Some(ch) = next {
        if ch == ' ' {
            // Mark plus its trailing space hang the rest of the message.
            return if mark > 0 { indent + mark + 1 } else { indent };
        }
        if ch.is_alphanumeric() || mark + ch.width().unwrap_or(0) > 2 {
            return indent;
        }
        mark += ch.width().unwrap_or(0);
        next = chars.next();
    }
    indent
}

/// Word-aware wrapping state for one source line. Characters are fed in
/// one at a time (with their span style); words are held back until a
/// space proves they are complete, then committed to the current output
/// line or wrapped whole onto the next. Styles are preserved across
/// breaks by carrying (text, style) runs rather than raw strings.
struct LineWrapper {
    width: usize,
    /// Columns every continuation line starts with (hanging indent).
    indent: usize,
    /// Column the current output line started at: 0 for the first line,
    /// `indent` afterwards.
    start: usize,
    /// Display columns used on the current output line.
    used: usize,
    current: Vec<(String, Style)>,
    /// Word being accumulated, not yet committed to `current`.
    word: Vec<(String, Style)>,
    word_cols: usize,
    /// Spaces seen since the last word, held back so a wrap can eat them.
    spaces: Vec<(String, Style)>,
    space_cols: usize,
    /// Line-level style/alignment of the source line, re-applied to every
    /// wrapped piece.
    line_style: Style,
    alignment: Option<Alignment>,
    done: Vec<Line<'static>>,
}

impl LineWrapper {
    fn new(width: usize, indent: usize, line: &Line<'static>) -> Self {
        Self {
            width,
            indent,
            start: 0,
            used: 0,
            current: Vec::new(),
            word: Vec::new(),
            word_cols: 0,
            spaces: Vec::new(),
            space_cols: 0,
            line_style: line.style,
            alignment: line.alignment,
            done: Vec::new(),
        }
    }

    /// Append one char to a run buffer, merging consecutive equal styles.
    fn push_run(buffer: &mut Vec<(String, Style)>, ch: char, style: Style) {
        match buffer.last_mut() {
            Some((text, last)) if *last == style => text.push(ch),
            _ => buffer.push((ch.to_string(), style)),
        }
    }

    /// Move every run in `from` onto the end of `to`, merging styles.
    fn append_runs(to: &mut Vec<(String, Style)>, from: &mut Vec<(String, Style)>) {
        for (text, style) in from.drain(..) {
            match to.last_mut() {
                Some((last_text, last)) if *last == style => last_text.push_str(&text),
                _ => to.push((text, style)),
            }
        }
    }

    fn feed(&mut self, ch: char, style: Style) {
        if ch == ' ' {
            self.commit_word();
            Self::push_run(&mut self.spaces, ch, style);
            self.space_cols += 1;
            return;
        }
        let ch_width = ch.width().unwrap_or(0);
        if ch_width == 0 && self.word.is_empty() && !self.spaces.is_empty() {
            // A combining mark right after a space stays with that space.
            Self::push_run(&mut self.spaces, ch, style);
            return;
        }
        Self::push_run(&mut self.word, ch, style);
        self.word_cols += ch_width;
    }

    /// Commit the buffered word: onto the current line when it fits after
    /// the held spaces, else onto a fresh continuation line (the break
    /// eats the spaces), hard-splitting only when the word alone exceeds
    /// the content width.
    fn commit_word(&mut self) {
        if self.word.is_empty() {
            return;
        }
        if self.used + self.space_cols + self.word_cols <= self.width {
            self.flush_spaces();
            self.flush_word();
            return;
        }
        if self.used > self.start {
            self.spaces.clear();
            self.space_cols = 0;
            self.newline();
        } else {
            // Line start: keep the source line's leading whitespace.
            self.flush_spaces();
        }
        if self.used + self.word_cols <= self.width {
            self.flush_word();
        } else {
            self.hard_split();
        }
    }

    fn flush_spaces(&mut self) {
        Self::append_runs(&mut self.current, &mut self.spaces);
        self.used += self.space_cols;
        self.space_cols = 0;
    }

    fn flush_word(&mut self) {
        Self::append_runs(&mut self.current, &mut self.word);
        self.used += self.word_cols;
        self.word_cols = 0;
    }

    /// Char-level fallback for a word wider than the content width. A wide
    /// char that no longer fits wraps first; zero-width chars (combining
    /// marks) never split from their base char.
    fn hard_split(&mut self) {
        for (text, style) in std::mem::take(&mut self.word) {
            for ch in text.chars() {
                let ch_width = ch.width().unwrap_or(0);
                if ch_width > 0 && self.used + ch_width > self.width && self.used > self.start {
                    self.newline();
                }
                Self::push_run(&mut self.current, ch, style);
                self.used += ch_width;
            }
        }
        self.word_cols = 0;
    }

    /// Emit the current line and open a continuation at the hanging indent.
    fn newline(&mut self) {
        self.emit();
        if self.indent > 0 {
            self.current
                .push((" ".repeat(self.indent), Style::default()));
        }
        self.used = self.indent;
        self.start = self.indent;
    }

    fn emit(&mut self) {
        let spans: Vec<Span<'static>> = std::mem::take(&mut self.current)
            .into_iter()
            .map(|(text, style)| Span::styled(text, style))
            .collect();
        let mut line = Line::from(spans);
        line.style = self.line_style;
        line.alignment = self.alignment;
        self.done.push(line);
    }

    fn finish(&mut self) -> Vec<Line<'static>> {
        self.commit_word();
        if self.used + self.space_cols <= self.width {
            // Trailing spaces that still fit are kept verbatim.
            self.flush_spaces();
        }
        self.emit();
        std::mem::take(&mut self.done)
    }
}

/// Longest prefix of `text` that fits in `max` display columns (zero-width
/// chars at the boundary stay attached).
fn take_width(text: &str, max: usize) -> &str {
    let mut used = 0usize;
    let mut end = 0usize;
    for (index, ch) in text.char_indices() {
        let ch_width = ch.width().unwrap_or(0);
        if used + ch_width > max {
            break;
        }
        used += ch_width;
        end = index + ch.len_utf8();
    }
    &text[..end]
}

/// Format the working directory for the status bar: abbreviate `$HOME` to
/// `~`, and when wider than `max` columns drop leading components (prefixing
/// `…/`) so the leaf directory — the part you actually care about — stays
/// visible instead of being clipped off the end.
fn format_cwd(root: &std::path::Path, max: usize) -> String {
    let full = root.display().to_string();
    let display = match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && full.starts_with(&home) => {
            format!("~{}", &full[home.len()..])
        }
        _ => full,
    };
    if display.width() <= max {
        return display;
    }
    let sep = std::path::MAIN_SEPARATOR.to_string();
    let mut parts: Vec<&str> = display.split(&sep).filter(|p| !p.is_empty()).collect();
    while parts.len() > 1 {
        parts.remove(0);
        let candidate = format!("…{sep}{}", parts.join(&sep));
        if candidate.width() <= max {
            return candidate;
        }
    }
    // A single leaf still too wide: keep its tail under a leading `…`.
    let leaf = parts.last().copied().unwrap_or(&display);
    let budget = max.saturating_sub(1);
    let tail: String = {
        let mut used = 0;
        let mut chars: Vec<char> = Vec::new();
        for ch in leaf.chars().rev() {
            let w = ch.width().unwrap_or(0);
            if used + w > budget {
                break;
            }
            used += w;
            chars.push(ch);
        }
        chars.into_iter().rev().collect()
    };
    format!("…{tail}")
}

/// Truncate to `max` display columns (not chars), appending `…` when cut.
fn truncate_width(text: &str, max: usize) -> String {
    if text.width() <= max {
        return text.to_string();
    }
    let mut out = take_width(text, max.saturating_sub(1)).to_string();
    out.push('…');
    out
}

/// Clip a plain string to `max` columns, ending in `…` when cut. Counts chars,
/// not bytes, so it never splits a multi-byte character.
fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let keep = max.saturating_sub(1);
    let mut out: String = text.chars().take(keep).collect();
    out.push('…');
    out
}

/// Truncate a styled line to `max` display columns, appending a dim `…`
/// when cut so clipped content is visible as such (used by the diff
/// sidebar, where long lines would otherwise just stop mid-word).
fn truncate_line(mut line: Line<'static>, max: usize) -> Line<'static> {
    if line.width() <= max {
        return line;
    }
    let budget = max.saturating_sub(1);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for span in line.spans.drain(..) {
        let span_width = span.content.width();
        if used + span_width <= budget {
            used += span_width;
            spans.push(span);
            continue;
        }
        let kept = take_width(&span.content, budget - used);
        if !kept.is_empty() {
            spans.push(Span::styled(kept.to_string(), span.style));
        }
        break;
    }
    spans.push(Span::styled("…", dim()));
    line.spans = spans;
    line
}

// ---------------------------------------------------------------------------
// Syntax highlighting (syntect) — foreground colors only, never backgrounds,
// so the terminal's own background always shows through.
// ---------------------------------------------------------------------------

static SYNTECT_ASSETS: OnceLock<(SyntaxSet, Option<Theme>)> = OnceLock::new();

fn syntect_assets() -> &'static (SyntaxSet, Option<Theme>) {
    SYNTECT_ASSETS.get_or_init(|| {
        let syntaxes = SyntaxSet::load_defaults_newlines();
        let mut themes = ThemeSet::load_defaults();
        let theme = themes.themes.remove("base16-ocean.dark").or_else(|| {
            let key = themes.themes.keys().next().cloned()?;
            themes.themes.remove(&key)
        });
        (syntaxes, theme)
    })
}

/// Syntax-highlight a unified diff via syntect for terminal display.
pub fn highlight_diff(diff: &str) -> Text<'static> {
    let (syntaxes, theme) = syntect_assets();
    let syntax = syntaxes
        .find_syntax_by_name("Diff")
        .or_else(|| syntaxes.find_syntax_by_extension("diff"));

    let (Some(syntax), Some(theme)) = (syntax, theme.as_ref()) else {
        return fallback_diff(diff);
    };

    let mut highlighter = HighlightLines::new(syntax, theme);
    let mut lines: Vec<Line<'static>> = Vec::new();
    for line in LinesWithEndings::from(diff) {
        match highlighter.highlight_line(line, syntaxes) {
            Ok(ranges) => {
                let spans: Vec<Span<'static>> = ranges
                    .into_iter()
                    .map(|(style, content)| {
                        Span::styled(
                            content.trim_end_matches('\n').to_string(),
                            syntect_style(style),
                        )
                    })
                    .collect();
                lines.push(Line::from(spans));
            }
            Err(_) => lines.push(Line::raw(line.trim_end_matches('\n').to_string())),
        }
    }
    Text::from(lines)
}

/// Map a syntect style to ratatui, collapsing the theme's foreground to its
/// grayscale luminance (the UI is monochrome) and keeping font modifiers —
/// backgrounds would paint over the terminal transparency.
fn syntect_style(style: syntect::highlighting::Style) -> Style {
    let fg = style.foreground;
    let luma = (u32::from(fg.r) * 299 + u32::from(fg.g) * 587 + u32::from(fg.b) * 114) / 1000;
    let luma = luma as u8;
    let mut out = Style::default().fg(Color::Rgb(luma, luma, luma));
    if style.font_style.contains(FontStyle::BOLD) {
        out = out.add_modifier(Modifier::BOLD);
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        out = out.add_modifier(Modifier::ITALIC);
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        out = out.add_modifier(Modifier::UNDERLINED);
    }
    out
}

/// Plain prefix-based diff coloring used when syntect assets are missing.
fn fallback_diff(diff: &str) -> Text<'static> {
    let lines: Vec<Line<'static>> = diff
        .lines()
        .map(|line| {
            let style = if line.starts_with("+++") || line.starts_with("---") {
                Style::default().add_modifier(Modifier::BOLD)
            } else if line.starts_with('+') {
                Style::default().fg(Color::White)
            } else if line.starts_with('-') {
                Style::default().fg(TEXT_DIM)
            } else if line.starts_with("@@") {
                accent()
            } else if line.starts_with("diff ") || line.starts_with("index ") {
                Style::default().fg(TEXT_DIM).bold()
            } else {
                dim()
            };
            Line::from(Span::styled(line.to_string(), style))
        })
        .collect();
    Text::from(lines)
}

/// Highlight one fenced code block, memoized: completed blocks are
/// re-rendered every frame, so identical (lang, code) pairs hit the cache.
fn highlight_code_block(lang: &str, code: &str) -> Vec<Line<'static>> {
    static CACHE: OnceLock<Mutex<HashMap<u64, Vec<Line<'static>>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));

    let mut hasher = std::hash::DefaultHasher::new();
    lang.hash(&mut hasher);
    code.hash(&mut hasher);
    let key = hasher.finish();
    if let Ok(guard) = cache.lock()
        && let Some(lines) = guard.get(&key)
    {
        return lines.clone();
    }

    let (syntaxes, theme) = syntect_assets();
    let syntax = if lang.is_empty() {
        None
    } else {
        syntaxes.find_syntax_by_token(lang)
    };
    let lines: Vec<Line<'static>> = match (syntax, theme.as_ref()) {
        (Some(syntax), Some(theme)) => {
            let mut highlighter = HighlightLines::new(syntax, theme);
            LinesWithEndings::from(code)
                .map(|line| match highlighter.highlight_line(line, syntaxes) {
                    Ok(ranges) => Line::from(
                        ranges
                            .into_iter()
                            .map(|(style, content)| {
                                Span::styled(
                                    content.trim_end_matches('\n').to_string(),
                                    syntect_style(style),
                                )
                            })
                            .collect::<Vec<_>>(),
                    ),
                    Err(_) => Line::from(Span::styled(
                        line.trim_end_matches('\n').to_string(),
                        Style::default().fg(TEXT_DIM),
                    )),
                })
                .collect()
        }
        _ => code
            .lines()
            .map(|line| {
                Line::from(Span::styled(
                    line.to_string(),
                    Style::default().fg(TEXT_DIM),
                ))
            })
            .collect(),
    };

    if let Ok(mut guard) = cache.lock() {
        if guard.len() >= 128 {
            guard.clear();
        }
        guard.insert(key, lines.clone());
    }
    lines
}

// ---------------------------------------------------------------------------
// Markdown rendering (pulldown-cmark)
// ---------------------------------------------------------------------------

/// Render completed markdown to styled terminal text (fenced code blocks
/// syntax-highlighted, foreground colors only). Tables size themselves to
/// their content — callers that know the viewport should prefer
/// [`render_markdown_at`].
pub fn render_markdown(source: &str) -> Text<'static> {
    render_markdown_at(source, usize::MAX)
}

/// Like [`render_markdown`], but tables lay out into at most `width` display
/// columns: columns shrink proportionally and long cells wrap inside their
/// column so a narrow terminal keeps a coherent grid instead of soft-wrapping
/// mid-row.
pub fn render_markdown_at(source: &str, width: usize) -> Text<'static> {
    render_markdown_inner(source, true, width)
}

/// Render in-flight streaming markdown: identical, except code blocks stay
/// plain so per-frame rendering stays cheap.
fn render_markdown_streaming(source: &str, width: usize) -> Text<'static> {
    render_markdown_inner(source, false, width)
}

fn render_markdown_inner(source: &str, highlight: bool, width: usize) -> Text<'static> {
    let mut renderer = MarkdownRenderer {
        highlight,
        table_width: width,
        ..MarkdownRenderer::default()
    };
    let options = Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_TABLES
        // `$…$` / `$$…$$` emit InlineMath / DisplayMath so we can turn TeX
        // into Unicode instead of dumping raw backslash soup in the TUI.
        | Options::ENABLE_MATH;
    for event in Parser::new_ext(source, options) {
        renderer.event(event);
    }
    renderer.finish()
}

/// One table cell's styled inline spans.
type CellSpans = Vec<Span<'static>>;

struct MarkdownRenderer {
    lines: Vec<Line<'static>>,
    current: Vec<Span<'static>>,
    bold: usize,
    italic: usize,
    strike: usize,
    code_block: bool,
    /// Syntax-highlight completed code blocks via syntect.
    highlight: bool,
    /// Fenced language and buffered source of the open code block (only
    /// used when `highlight` is set).
    code_lang: String,
    code_buffer: String,
    heading: bool,
    /// One entry per open list; `Some(n)` carries the next ordered index.
    lists: Vec<Option<u64>>,
    quote_depth: usize,
    link: Option<String>,
    in_table: bool,
    /// Whether the open table closed a header row (always its first row).
    table_header: bool,
    table_aligns: Vec<MdAlignment>,
    table_rows: Vec<Vec<CellSpans>>,
    table_row: Vec<CellSpans>,
    /// Max display columns a finished table may occupy. `usize::MAX` means
    /// "size to content" (tests and callers without a viewport).
    table_width: usize,
}

impl Default for MarkdownRenderer {
    fn default() -> Self {
        Self {
            lines: Vec::new(),
            current: Vec::new(),
            bold: 0,
            italic: 0,
            strike: 0,
            code_block: false,
            highlight: false,
            code_lang: String::new(),
            code_buffer: String::new(),
            heading: false,
            lists: Vec::new(),
            quote_depth: 0,
            link: None,
            in_table: false,
            table_header: false,
            table_aligns: Vec::new(),
            table_rows: Vec::new(),
            table_row: Vec::new(),
            // Size to content unless the caller sets a viewport budget.
            table_width: usize::MAX,
        }
    }
}

impl MarkdownRenderer {
    fn style(&self) -> Style {
        let mut style = Style::default();
        if self.code_block {
            // In-flight (unhighlighted) block code: neutral, not loud.
            return style.fg(TEXT_DIM);
        }
        if self.heading {
            style = style.fg(ACCENT).add_modifier(Modifier::BOLD);
        }
        if self.bold > 0 {
            style = style.add_modifier(Modifier::BOLD);
        }
        if self.italic > 0 {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if self.strike > 0 {
            style = style.add_modifier(Modifier::CROSSED_OUT);
        }
        if self.quote_depth > 0 {
            style = style.fg(TEXT_DIM);
        }
        style
    }

    fn end_line(&mut self) {
        self.lines
            .push(Line::from(std::mem::take(&mut self.current)));
    }

    /// Flush the current spans only when non-empty (no spurious blank line).
    fn flush(&mut self) {
        if !self.current.is_empty() {
            self.end_line();
        }
    }

    fn blank_line(&mut self) {
        if !matches!(self.lines.last(), Some(line) if line.spans.is_empty()) {
            self.lines.push(Line::raw(""));
        }
    }

    fn line_prefix(&mut self) {
        if self.quote_depth > 0 {
            self.current
                .push(Span::styled("▌ ".repeat(self.quote_depth), dim()));
        }
        if self.code_block {
            self.current.push(Span::raw("  "));
        }
    }

    fn push_text(&mut self, text: &str) {
        if self.code_block {
            if self.highlight {
                // Buffered for one syntect pass when the block closes.
                self.code_buffer.push_str(text);
                return;
            }
            // Streaming: code blocks carry embedded newlines, plain style.
            let style = self.style();
            let mut first = true;
            for part in text.split('\n') {
                if !first {
                    self.end_line();
                    self.line_prefix();
                }
                first = false;
                if !part.is_empty() {
                    self.current.push(Span::styled(part.to_string(), style));
                }
            }
        } else {
            self.current
                .push(Span::styled(text.to_string(), self.style()));
        }
    }

    fn event(&mut self, event: MdEvent) {
        match event {
            MdEvent::Start(tag) => self.start(tag),
            MdEvent::End(tag) => self.end(tag),
            MdEvent::Text(text) => self.push_text(&text),
            MdEvent::Code(code) => {
                self.current
                    .push(Span::styled(code.to_string(), Style::default().fg(CODE)));
            }
            MdEvent::InlineMath(tex) => self.push_math(&tex, false),
            MdEvent::DisplayMath(tex) => self.push_math(&tex, true),
            // Table cells are single-line: fold breaks into a space.
            MdEvent::SoftBreak | MdEvent::HardBreak if self.in_table => self.push_text(" "),
            MdEvent::SoftBreak | MdEvent::HardBreak => {
                self.end_line();
                self.line_prefix();
            }
            MdEvent::Rule => {
                self.flush();
                self.lines
                    .push(Line::from(Span::styled("─".repeat(24), dim())));
                self.blank_line();
            }
            MdEvent::TaskListMarker(checked) => {
                let marker = if checked { "[x] " } else { "[ ] " };
                self.current.push(Span::styled(marker.to_string(), dim()));
            }
            MdEvent::Html(html) | MdEvent::InlineHtml(html) => self.push_text(&html),
            _ => {}
        }
    }

    /// Render TeX math as Unicode, italic so it reads as math rather than prose.
    /// Display math (`$$…$$`) gets its own indented line; inside a table cell
    /// both forms stay inline so the grid stays single-line per cell.
    fn push_math(&mut self, tex: &str, display: bool) {
        let rendered = latex_to_unicode(tex);
        let style = Style::default().fg(CODE).add_modifier(Modifier::ITALIC);
        if display && !self.in_table {
            self.flush();
            self.line_prefix();
            self.current.push(Span::raw("  "));
            self.current.push(Span::styled(rendered, style));
            self.end_line();
            self.blank_line();
        } else {
            self.current.push(Span::styled(rendered, style));
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => {
                self.flush();
                self.line_prefix();
            }
            Tag::Heading { .. } => {
                self.flush();
                self.blank_line();
                self.heading = true;
            }
            Tag::BlockQuote(_) => {
                self.flush();
                self.quote_depth += 1;
            }
            Tag::CodeBlock(kind) => {
                self.flush();
                self.code_lang.clear();
                if let CodeBlockKind::Fenced(lang) = kind
                    && !lang.is_empty()
                {
                    self.code_lang.push_str(&lang);
                    self.lines
                        .push(Line::from(Span::styled(format!("  ⌜{lang}⌟"), dim())));
                }
                self.code_block = true;
                self.code_buffer.clear();
                if !self.highlight {
                    self.line_prefix();
                }
            }
            Tag::List(start) => {
                self.flush();
                self.lists.push(start);
            }
            Tag::Item => {
                self.flush();
                let depth = self.lists.len().saturating_sub(1);
                let indent = "  ".repeat(depth);
                let bullet = match self.lists.last_mut() {
                    Some(Some(index)) => {
                        let label = format!("{indent}{index}. ");
                        *index += 1;
                        label
                    }
                    _ => format!("{indent}• "),
                };
                self.current.push(Span::styled(bullet, dim()));
            }
            Tag::Table(aligns) => {
                self.flush();
                self.in_table = true;
                self.table_aligns = aligns;
            }
            Tag::TableHead => self.bold += 1,
            Tag::Emphasis => self.italic += 1,
            Tag::Strong => self.bold += 1,
            Tag::Strikethrough => self.strike += 1,
            Tag::Link { dest_url, .. } => self.link = Some(dest_url.to_string()),
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph if self.in_table => {}
            TagEnd::Paragraph => {
                self.flush();
                if self.lists.is_empty() {
                    self.blank_line();
                }
            }
            TagEnd::Heading(_) => {
                self.heading = false;
                self.flush();
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.quote_depth = self.quote_depth.saturating_sub(1);
                self.blank_line();
            }
            TagEnd::CodeBlock => {
                if self.highlight {
                    let code = std::mem::take(&mut self.code_buffer);
                    let lang = std::mem::take(&mut self.code_lang);
                    for mut line in highlight_code_block(&lang, &code) {
                        let mut spans = Vec::with_capacity(line.spans.len() + 1);
                        spans.push(Span::raw("  "));
                        spans.append(&mut line.spans);
                        self.lines.push(Line::from(spans));
                    }
                } else {
                    self.flush();
                }
                self.code_block = false;
                self.blank_line();
            }
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
                if self.lists.is_empty() {
                    self.blank_line();
                }
            }
            TagEnd::Item => self.flush(),
            TagEnd::TableCell => {
                let cell = std::mem::take(&mut self.current);
                self.table_row.push(cell);
            }
            TagEnd::TableHead => {
                self.bold = self.bold.saturating_sub(1);
                self.table_header = true;
                self.table_rows.push(std::mem::take(&mut self.table_row));
            }
            TagEnd::TableRow => self.table_rows.push(std::mem::take(&mut self.table_row)),
            TagEnd::Table => self.end_table(),
            TagEnd::Emphasis => self.italic = self.italic.saturating_sub(1),
            TagEnd::Strong => self.bold = self.bold.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            TagEnd::Link => {
                if let Some(url) = self.link.take() {
                    self.current
                        .push(Span::styled(format!(" ({url})"), dim().underlined()));
                }
            }
            _ => {}
        }
    }

    /// Emit the buffered table as an aligned grid: cells padded to their
    /// column's display width, dim `│` rules between columns, a dim `─┼─`
    /// rule after the header. Rows may be ragged (mid-stream truncation);
    /// missing cells pad as empty.
    ///
    /// When [`Self::table_width`] is finite and smaller than the natural
    /// grid, columns shrink toward content (floor 1) and long cells wrap
    /// inside their column so the row stays a coherent multi-line block
    /// instead of soft-wrapping mid-`│`.
    fn end_table(&mut self) {
        self.in_table = false;
        let has_header = std::mem::take(&mut self.table_header);
        let aligns = std::mem::take(&mut self.table_aligns);
        let rows = std::mem::take(&mut self.table_rows);
        let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
        if cols == 0 {
            return;
        }
        let mut widths = vec![0usize; cols];
        for row in &rows {
            for (col, cell) in row.iter().enumerate() {
                widths[col] = widths[col].max(spans_width(cell));
            }
        }
        // Shrink columns to fit the available width. Separators cost
        // 3 columns each (` │ `); leave at least one column of content
        // per cell so the grid never collapses to pure rules.
        let sep = 3usize;
        let seps = cols.saturating_sub(1).saturating_mul(sep);
        if self.table_width < usize::MAX {
            let budget = self.table_width.saturating_sub(seps);
            let natural: usize = widths.iter().sum();
            if natural > budget {
                fit_column_widths(&mut widths, budget);
            }
        }
        for (index, mut row) in rows.into_iter().enumerate() {
            row.resize_with(cols, Vec::new);
            // Each cell may wrap into several lines; the row height is the
            // tallest cell. Empty cells contribute a single blank line.
            let wrapped: Vec<Vec<CellSpans>> = row
                .into_iter()
                .enumerate()
                .map(|(col, cell)| wrap_cell(cell, widths[col].max(1)))
                .collect();
            let height = wrapped.iter().map(Vec::len).max().unwrap_or(1).max(1);
            for line_idx in 0..height {
                let mut spans = Vec::new();
                for (col, cell_lines) in wrapped.iter().enumerate() {
                    if col > 0 {
                        spans.push(Span::styled(" │ ", dim()));
                    }
                    let cell = cell_lines.get(line_idx).cloned().unwrap_or_default();
                    let pad = widths[col].saturating_sub(spans_width(&cell));
                    // Align only the first wrapped line of a multi-line cell;
                    // continuation lines stay left so the text column reads
                    // as a hanging block rather than a re-centered mess.
                    let (left, right) = if line_idx == 0 {
                        match aligns.get(col) {
                            Some(MdAlignment::Right) => (pad, 0),
                            Some(MdAlignment::Center) => (pad / 2, pad - pad / 2),
                            _ => (0, pad),
                        }
                    } else {
                        (0, pad)
                    };
                    if left > 0 {
                        spans.push(Span::raw(" ".repeat(left)));
                    }
                    spans.extend(cell);
                    if right > 0 {
                        spans.push(Span::raw(" ".repeat(right)));
                    }
                }
                self.lines.push(Line::from(spans));
            }
            if index == 0 && has_header {
                let rule = widths
                    .iter()
                    .map(|width| "─".repeat(*width))
                    .collect::<Vec<_>>()
                    .join("─┼─");
                self.lines.push(Line::from(Span::styled(rule, dim())));
            }
        }
        self.blank_line();
    }

    fn finish(mut self) -> Text<'static> {
        self.flush();
        while matches!(self.lines.last(), Some(line) if line.spans.is_empty()) {
            self.lines.pop();
        }
        Text::from(self.lines)
    }
}

/// Convert a TeX math fragment to a readable Unicode string for the TUI.
///
/// Terminals can't run KaTeX. We strip font-selection wrappers (`\mathrm`,
/// `\mathbf`, …), map a few blackboard/script sets unicodeit misses when they
/// sit next to nested braces, then run `unicodeit` for the bulk of symbols,
/// super/subscripts, and operators. Anything still untranslatable is left
/// as-is so the formula stays copy-pastable.
fn latex_to_unicode(tex: &str) -> String {
    let trimmed = tex.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let mut s = trimmed.to_string();
    // Drop pure-style wrappers so their contents become ordinary text that
    // unicodeit can subscript (`z_{\mathrm{pos}}` → `z_{pos}` → `zₚₒₛ`).
    for cmd in [
        "mathrm",
        "mathbf",
        "boldsymbol",
        "mathit",
        "mathsf",
        "mathtt",
        "operatorname",
        "text",
        "textrm",
        "textit",
        "textbf",
        "textsf",
        "texttt",
        "bm",
        "mathbfit",
    ] {
        s = strip_latex_cmd(&s, cmd);
    }
    // Blackboard / fraktur / script single-letter sets unicodeit only maps as
    // whole commands when the argument is a bare letter — expand them first.
    s = expand_letter_set(&s, "mathbb", MATHBB);
    s = expand_letter_set(&s, "mathcal", MATHCAL);
    s = expand_letter_set(&s, "mathscr", MATHCAL);
    s = expand_letter_set(&s, "mathfrak", MATHFRAK);
    // Common aliases. Longest first so `\rightarrow` isn't half-eaten by `\to`.
    // Only replace when the match is a whole TeX control word (next char is not
    // an ASCII letter) — otherwise `\in` would nibble `\infty`/`\int`/…
    let mut aliases = [
        ("\\Leftrightarrow", "⇔"),
        ("\\leftrightarrow", "↔"),
        ("\\Rightarrow", "⇒"),
        ("\\Leftarrow", "⇐"),
        ("\\rightarrow", "→"),
        ("\\leftarrow", "←"),
        ("\\subseteq", "⊆"),
        ("\\supseteq", "⊇"),
        ("\\subset", "⊂"),
        ("\\supset", "⊃"),
        ("\\emptyset", "∅"),
        ("\\partial", "∂"),
        ("\\nabla", "∇"),
        ("\\forall", "∀"),
        ("\\exists", "∃"),
        ("\\notin", "∉"),
        ("\\approx", "≈"),
        ("\\simeq", "≃"),
        ("\\equiv", "≡"),
        ("\\cong", "≅"),
        ("\\parallel", "∥"),
        ("\\perp", "⊥"),
        ("\\otimes", "⊗"),
        ("\\oplus", "⊕"),
        ("\\ominus", "⊖"),
        ("\\oslash", "⊘"),
        ("\\cdots", "⋯"),
        ("\\vdots", "⋮"),
        ("\\ddots", "⋱"),
        ("\\ldots", "…"),
        ("\\times", "×"),
        ("\\cdot", "·"),
        ("\\bullet", "•"),
        ("\\circ", "∘"),
        ("\\ast", "∗"),
        ("\\wedge", "∧"),
        ("\\vee", "∨"),
        ("\\mid", "∣"),
        ("\\sim", "∼"),
        ("\\neq", "≠"),
        ("\\leq", "≤"),
        ("\\geq", "≥"),
        ("\\ll", "≪"),
        ("\\gg", "≫"),
        ("\\in", "∈"),
        ("\\ni", "∋"),
        ("\\cup", "∪"),
        ("\\cap", "∩"),
        ("\\neg", "¬"),
        ("\\top", "⊤"),
        ("\\bot", "⊥"),
        ("\\infty", "∞"),
        ("\\pm", "±"),
        ("\\mp", "∓"),
        ("\\ne", "≠"),
        ("\\le", "≤"),
        ("\\ge", "≥"),
        ("\\to", "→"),
    ];
    aliases.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
    for (from, to) in aliases {
        s = replace_tex_cmd(&s, from, to);
    }
    // `^\top` → superscript T (transpose) rather than a caret + ⊤.
    s = s.replace("^⊤", "ᵀ").replace("^{⊤}", "ᵀ");
    // \frac{a}{b} → (a)/(b)  (unicodeit leaves \frac alone)
    s = rewrite_frac(&s);
    // \sqrt{x} → √(x)
    s = rewrite_sqrt(&s);
    let out = unicodeit::replace(&s);
    // Collapse leftover empty braces and double spaces unicodeit can leave.
    // Flatten unconverted `_{word}` / `^{word}` to `_word` / `^word` so mixed
    // scripts (e.g. `zₚₒₛ` next to a non-subscriptable `neg`) stay readable.
    let out = flatten_unconverted_scripts(&out);
    let out = out.replace("{}", "").replace("  ", " ");
    out.trim().to_string()
}

/// Turn leftover `_{abc}` / `^{abc}` into `_abc` / `^abc` (braces only gone when
/// unicodeit couldn't map every character to a dedicated super/subscript).
fn flatten_unconverted_scripts(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        let ch = bytes[i] as char;
        if (ch == '_' || ch == '^')
            && i + 1 < bytes.len()
            && bytes[i + 1] == b'{'
            && let Some((body, after)) = take_balanced(&input[i + 2..])
        {
            out.push(ch);
            out.push_str(body);
            // `after` is a suffix of `input`; advance `i` by the matched span.
            i = input.len() - after.len();
            continue;
        }
        // Copy one UTF-8 char.
        let next = input[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
        out.push_str(&input[i..i + next]);
        i += next;
    }
    out
}

/// Blackboard-bold capitals (`\mathbb{R}` → ℝ).
const MATHBB: &[(char, char)] = &[
    ('A', '𝔸'),
    ('B', '𝔹'),
    ('C', 'ℂ'),
    ('D', '𝔻'),
    ('E', '𝔼'),
    ('F', '𝔽'),
    ('G', '𝔾'),
    ('H', 'ℍ'),
    ('I', '𝕀'),
    ('J', '𝕁'),
    ('K', '𝕂'),
    ('L', '𝕃'),
    ('M', '𝕄'),
    ('N', 'ℕ'),
    ('O', '𝕆'),
    ('P', 'ℙ'),
    ('Q', 'ℚ'),
    ('R', 'ℝ'),
    ('S', '𝕊'),
    ('T', '𝕋'),
    ('U', '𝕌'),
    ('V', '𝕍'),
    ('W', '𝕎'),
    ('X', '𝕏'),
    ('Y', '𝕐'),
    ('Z', 'ℤ'),
];

/// Calligraphic / script capitals (`\mathcal{L}` → ℒ).
const MATHCAL: &[(char, char)] = &[
    ('A', '𝒜'),
    ('B', 'ℬ'),
    ('C', '𝒞'),
    ('D', '𝒟'),
    ('E', 'ℰ'),
    ('F', 'ℱ'),
    ('G', '𝒢'),
    ('H', 'ℋ'),
    ('I', 'ℐ'),
    ('J', '𝒥'),
    ('K', '𝒦'),
    ('L', 'ℒ'),
    ('M', 'ℳ'),
    ('N', '𝒩'),
    ('O', '𝒪'),
    ('P', '𝒫'),
    ('Q', '𝒬'),
    ('R', 'ℛ'),
    ('S', '𝒮'),
    ('T', '𝒯'),
    ('U', '𝒰'),
    ('V', '𝒱'),
    ('W', '𝒲'),
    ('X', '𝒳'),
    ('Y', '𝒴'),
    ('Z', '𝒵'),
];

/// Fraktur capitals (`\mathfrak{g}` keeps lowercase as-is via the map).
const MATHFRAK: &[(char, char)] = &[
    ('A', '𝔄'),
    ('B', '𝔅'),
    ('C', 'ℭ'),
    ('D', '𝔇'),
    ('E', '𝔈'),
    ('F', '𝔉'),
    ('G', '𝔊'),
    ('H', 'ℌ'),
    ('I', 'ℑ'),
    ('J', '𝔍'),
    ('K', '𝔎'),
    ('L', '𝔏'),
    ('M', '𝔐'),
    ('N', '𝔑'),
    ('O', '𝔒'),
    ('P', '𝔓'),
    ('Q', '𝔔'),
    ('R', 'ℜ'),
    ('S', '𝔖'),
    ('T', '𝔗'),
    ('U', '𝔘'),
    ('V', '𝔙'),
    ('W', '𝔚'),
    ('X', '𝔛'),
    ('Y', '𝔜'),
    ('Z', 'ℨ'),
];

/// Replace every `\cmd{…}` with its brace body (style wrappers only).
fn strip_latex_cmd(input: &str, cmd: &str) -> String {
    let needle = format!("\\{cmd}{{");
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(at) = rest.find(&needle) {
        out.push_str(&rest[..at]);
        rest = &rest[at + needle.len()..];
        match take_balanced(rest) {
            Some((body, after)) => {
                out.push_str(body);
                rest = after;
            }
            None => {
                // Unbalanced — keep the command literal and stop scanning.
                out.push_str(&needle);
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Replace `\cmd{X}` for single-letter `X` via `map`; multi-letter bodies stay.
fn expand_letter_set(input: &str, cmd: &str, map: &[(char, char)]) -> String {
    let needle = format!("\\{cmd}{{");
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(at) = rest.find(&needle) {
        out.push_str(&rest[..at]);
        rest = &rest[at + needle.len()..];
        match take_balanced(rest) {
            Some((body, after)) => {
                let mut chars = body.chars();
                if let (Some(c), None) = (chars.next(), chars.next()) {
                    if let Some((_, uni)) = map.iter().find(|(k, _)| *k == c) {
                        out.push(*uni);
                    } else {
                        out.push(c);
                    }
                } else {
                    // Multi-char body: keep original command so nothing is lost.
                    out.push_str(&needle);
                    out.push_str(body);
                    out.push('}');
                }
                rest = after;
            }
            None => {
                out.push_str(&needle);
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Rewrite `\frac{num}{den}` → `(num)/(den)`.
fn rewrite_frac(input: &str) -> String {
    let needle = "\\frac{";
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(at) = rest.find(needle) {
        out.push_str(&rest[..at]);
        rest = &rest[at + needle.len()..];
        let Some((num, after_num)) = take_balanced(rest) else {
            out.push_str(needle);
            break;
        };
        rest = after_num;
        if !rest.starts_with('{') {
            out.push_str("\\frac{");
            out.push_str(num);
            out.push('}');
            continue;
        }
        rest = &rest[1..];
        let Some((den, after_den)) = take_balanced(rest) else {
            out.push_str("\\frac{");
            out.push_str(num);
            out.push_str("}{");
            break;
        };
        // Parenthesize only when the side has operators / multiple tokens.
        out.push_str(&paren_if_needed(num));
        out.push('/');
        out.push_str(&paren_if_needed(den));
        rest = after_den;
    }
    out.push_str(rest);
    out
}

/// Rewrite `\sqrt{x}` → `√(x)` and `\sqrt[n]{x}` → `ⁿ√(x)`.
fn rewrite_sqrt(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(at) = rest.find("\\sqrt") {
        out.push_str(&rest[..at]);
        rest = &rest[at + "\\sqrt".len()..];
        let mut index = String::new();
        if rest.starts_with('[')
            && let Some(end) = rest.find(']')
        {
            index.push_str(&rest[1..end]);
            rest = &rest[end + 1..];
        }
        if rest.starts_with('{') {
            rest = &rest[1..];
            if let Some((body, after)) = take_balanced(rest) {
                if !index.is_empty() {
                    // Map simple digit indices to superscripts when possible.
                    for ch in index.chars() {
                        out.push(match ch {
                            '0' => '⁰',
                            '1' => '¹',
                            '2' => '²',
                            '3' => '³',
                            '4' => '⁴',
                            '5' => '⁵',
                            '6' => '⁶',
                            '7' => '⁷',
                            '8' => '⁸',
                            '9' => '⁹',
                            _ => ch,
                        });
                    }
                }
                out.push('√');
                out.push_str(&paren_if_needed(body));
                rest = after;
                continue;
            }
        }
        // Fallback: leave the command text.
        out.push_str("\\sqrt");
        if !index.is_empty() {
            out.push('[');
            out.push_str(&index);
            out.push(']');
        }
    }
    out.push_str(rest);
    out
}

fn paren_if_needed(body: &str) -> String {
    let t = body.trim();
    if t.is_empty() {
        return "()".to_string();
    }
    // Already parenthesized, or a single atom (letter/digit/symbol run).
    if (t.starts_with('(') && t.ends_with(')'))
        || t.chars().count() == 1
        || t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return t.to_string();
    }
    format!("({t})")
}

/// Replace every occurrence of a TeX control sequence `cmd` (e.g. `\in`) with
/// `with`, but only at a control-word boundary — the character after the match
/// must not be an ASCII letter. Stops `\in` from eating `\infty`/`\int`.
fn replace_tex_cmd(input: &str, cmd: &str, with: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(at) = rest.find(cmd) {
        let after_cmd = &rest[at + cmd.len()..];
        let next_is_letter = after_cmd
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic());
        if next_is_letter {
            // Not a whole command — copy through the match and keep scanning.
            out.push_str(&rest[..at + cmd.len()]);
            rest = after_cmd;
            continue;
        }
        out.push_str(&rest[..at]);
        out.push_str(with);
        rest = after_cmd;
    }
    out.push_str(rest);
    out
}

/// Given input starting *after* an opening `{`, return `(body, rest_after_close)`.
fn take_balanced(input: &str) -> Option<(&str, &str)> {
    let mut depth = 1usize;
    for (i, ch) in input.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some((&input[..i], &input[i + 1..]));
                }
            }
            _ => {}
        }
    }
    None
}

fn spans_width(spans: &[Span]) -> usize {
    spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum()
}

/// Shrink `widths` in place so their sum is at most `budget`, never below 1
/// per column. Distributes the deficit by repeatedly taking one column from
/// the current widest — keeps relative importance, avoids starving a short
/// column while a long one still has room.
fn fit_column_widths(widths: &mut [usize], budget: usize) {
    if widths.is_empty() {
        return;
    }
    let floor = 1usize;
    let min_total = floor.saturating_mul(widths.len());
    let target = budget.max(min_total);
    // Cap anything already below floor first so the loop can't go negative.
    for w in widths.iter_mut() {
        *w = (*w).max(floor);
    }
    let mut total: usize = widths.iter().sum();
    while total > target {
        // Prefer trimming the current widest column that is still above floor.
        let Some((idx, _)) = widths
            .iter()
            .enumerate()
            .filter(|(_, w)| **w > floor)
            .max_by_key(|(_, w)| *w)
        else {
            break;
        };
        widths[idx] -= 1;
        total -= 1;
    }
}

/// Word-wrap a single table cell into lines of at most `width` display
/// columns, preserving span styles. Falls back to hard char splits only
/// when one word exceeds the budget (mirrors [`wrap_lines`]).
fn wrap_cell(cell: CellSpans, width: usize) -> Vec<CellSpans> {
    let width = width.max(1);
    if spans_width(&cell) <= width {
        return vec![cell];
    }

    // Flatten to (char, style) so a wrap can split mid-span cleanly.
    let mut chars: Vec<(char, Style)> = Vec::new();
    for span in &cell {
        let style = span.style;
        for ch in span.content.chars() {
            chars.push((ch, style));
        }
    }

    let mut lines: Vec<CellSpans> = Vec::new();
    let mut current: Vec<(char, Style)> = Vec::new();
    let mut used = 0usize;
    let mut word: Vec<(char, Style)> = Vec::new();
    let mut word_cols = 0usize;
    let mut spaces: Vec<(char, Style)> = Vec::new();
    let mut space_cols = 0usize;

    let emit = |current: &mut Vec<(char, Style)>, lines: &mut Vec<CellSpans>| {
        lines.push(chars_to_spans(std::mem::take(current)));
    };

    let hard_split = |current: &mut Vec<(char, Style)>,
                      used: &mut usize,
                      word: &mut Vec<(char, Style)>,
                      lines: &mut Vec<CellSpans>| {
        for (wch, wstyle) in word.drain(..) {
            let w = wch.width().unwrap_or(0);
            if w > 0 && *used + w > width && *used > 0 {
                emit(current, lines);
                *used = 0;
            }
            current.push((wch, wstyle));
            *used += w;
        }
    };

    let commit_word = |current: &mut Vec<(char, Style)>,
                       used: &mut usize,
                       word: &mut Vec<(char, Style)>,
                       word_cols: &mut usize,
                       spaces: &mut Vec<(char, Style)>,
                       space_cols: &mut usize,
                       lines: &mut Vec<CellSpans>| {
        if word.is_empty() {
            return;
        }
        if *used + *space_cols + *word_cols <= width {
            current.append(spaces);
            *used += *space_cols;
            *space_cols = 0;
            current.append(word);
            *used += *word_cols;
            *word_cols = 0;
            return;
        }
        if *used > 0 {
            spaces.clear();
            *space_cols = 0;
            emit(current, lines);
            *used = 0;
        } else {
            current.append(spaces);
            *used += *space_cols;
            *space_cols = 0;
        }
        if *used + *word_cols <= width {
            current.append(word);
            *used += *word_cols;
            *word_cols = 0;
        } else {
            hard_split(current, used, word, lines);
            *word_cols = 0;
        }
    };

    for (ch, style) in chars {
        if ch == ' ' {
            commit_word(
                &mut current,
                &mut used,
                &mut word,
                &mut word_cols,
                &mut spaces,
                &mut space_cols,
                &mut lines,
            );
            spaces.push((ch, style));
            space_cols += 1;
            continue;
        }
        let ch_w = ch.width().unwrap_or(0);
        if ch_w == 0 && word.is_empty() && !spaces.is_empty() {
            spaces.push((ch, style));
            continue;
        }
        word.push((ch, style));
        word_cols += ch_w;
    }
    commit_word(
        &mut current,
        &mut used,
        &mut word,
        &mut word_cols,
        &mut spaces,
        &mut space_cols,
        &mut lines,
    );
    if used + space_cols <= width {
        current.append(&mut spaces);
    }
    if !current.is_empty() || lines.is_empty() {
        emit(&mut current, &mut lines);
    }
    lines
}

/// Collapse a run of (char, style) pairs into spans, merging adjacent equal styles.
fn chars_to_spans(chars: Vec<(char, Style)>) -> CellSpans {
    let mut spans: CellSpans = Vec::new();
    for (ch, style) in chars {
        match spans.last_mut() {
            Some(span) if span.style == style => {
                let mut s = span.content.to_string();
                s.push(ch);
                *span = Span::styled(s, style);
            }
            _ => spans.push(Span::styled(ch.to_string(), style)),
        }
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Flatten a line's spans into one comparable string.
    fn flat(line: &Line) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn flats(lines: &[Line]) -> Vec<String> {
        lines.iter().map(flat).collect()
    }

    fn sel(anchor: (u16, u16), head: (u16, u16)) -> Selection {
        Selection {
            anchor,
            head,
            dragging: false,
        }
    }

    /// A 6×3 buffer holding three rows of text, for selection extraction tests.
    fn sample_buffer() -> Buffer {
        let mut buf = Buffer::empty(Rect::new(0, 0, 6, 3));
        buf.set_string(0, 0, "abcdef", Style::default());
        buf.set_string(0, 1, "ghi", Style::default()); // trailing blanks
        buf.set_string(0, 2, "jklmno", Style::default());
        buf
    }

    #[test]
    fn selection_on_one_row_includes_the_head_cell() {
        // Drag from column 1 to column 3 on row 0 → "bcd" (head inclusive).
        let rows = selection_rows(&sel((1, 0), (3, 0)), 6, 3);
        assert_eq!(rows, vec![(0, 1, 4)]);
        assert_eq!(
            selection_text(&sample_buffer(), &sel((1, 0), (3, 0))),
            "bcd"
        );
    }

    #[test]
    fn selection_orders_endpoints_regardless_of_drag_direction() {
        // Dragging up-and-left yields the same span as down-and-right.
        let forward = selection_text(&sample_buffer(), &sel((1, 0), (2, 2)));
        let backward = selection_text(&sample_buffer(), &sel((2, 2), (1, 0)));
        assert_eq!(forward, backward);
        assert_eq!(forward, "bcdef\nghi\njkl");
    }

    #[test]
    fn selection_trims_trailing_blanks_per_row() {
        // Middle row "ghi" padded to width 6; the blanks must not be copied.
        let text = selection_text(&sample_buffer(), &sel((0, 1), (5, 1)));
        assert_eq!(text, "ghi");
    }

    #[test]
    fn click_without_drag_is_empty() {
        // anchor == head: the app's Up handler clears it (no copy) via
        // is_empty(), so a no-drag click never reaches the copy path.
        assert!(sel((2, 1), (2, 1)).is_empty());
        assert!(!sel((2, 1), (3, 1)).is_empty());
    }

    #[test]
    fn selection_clamps_to_buffer_bounds() {
        // Head past the right/bottom edge stays within the grid.
        let rows = selection_rows(&sel((0, 0), (99, 99)), 6, 3);
        assert_eq!(rows, vec![(0, 0, 6), (1, 0, 6), (2, 0, 6)]);
    }

    #[test]
    fn indeterminate_bar_fills_width_and_animates() {
        let a = flat(&indeterminate_bar(20, 0));
        let b = flat(&indeterminate_bar(20, 7));
        assert_eq!(a.chars().count(), 20, "bar spans the full width");
        assert!(a.contains('█') && a.contains('░'), "has lit and dim cells");
        assert_ne!(a, b, "the lit window moves with the tick");
    }

    #[test]
    fn cwd_keeps_short_path_intact() {
        let p = std::path::Path::new("/srv/app");
        assert_eq!(format_cwd(p, 32), "/srv/app");
    }

    #[test]
    fn cwd_drops_leading_components_keeping_leaf() {
        let p = std::path::Path::new("/home/gradient/projects/ai/wizard");
        // Narrow budget forces dropping leading parts but keeps the leaf.
        let out = format_cwd(p, 14);
        assert!(out.starts_with('…'), "expected ellipsis prefix, got {out}");
        assert!(out.ends_with("wizard"), "expected leaf kept, got {out}");
        assert!(out.width() <= 14, "expected within budget, got {out}");
    }

    #[test]
    fn cwd_abbreviates_home() {
        // SAFETY: single-threaded test process.
        unsafe { std::env::set_var("HOME", "/home/gradient") };
        let p = std::path::Path::new("/home/gradient/projects/ai");
        assert_eq!(format_cwd(p, 32), "~/projects/ai");
    }

    #[test]
    fn wrap_breaks_at_word_boundaries() {
        let lines = wrap_lines(Text::from(Line::raw("the quick brown fox")), 10);
        assert_eq!(flats(&lines), ["the quick", "brown fox"]);
    }

    #[test]
    fn wrap_moves_whole_word_instead_of_splitting() {
        // The recorded defect: "one occurrence" split as "on / e occurrence".
        let lines = wrap_lines(Text::from(Line::raw("one occurrence")), 12);
        assert_eq!(flats(&lines), ["one", "occurrence"]);
    }

    #[test]
    fn wrap_hard_splits_word_longer_than_width() {
        let lines = wrap_lines(Text::from(Line::raw("abcdefghijkl")), 5);
        assert_eq!(flats(&lines), ["abcde", "fghij", "kl"]);
    }

    #[test]
    fn wrap_continuations_keep_hanging_indent() {
        let line = Line::from(vec![
            Span::styled("· ", accent()),
            Span::raw("alpha beta gamma"),
        ]);
        let lines = wrap_lines(Text::from(line), 9);
        assert_eq!(flats(&lines), ["· alpha", "  beta", "  gamma"]);
        // The marker keeps its accent style; continuations stay raw.
        assert_eq!(lines[0].spans[0].style, accent());
    }

    #[test]
    fn wrap_keeps_styles_across_span_boundary_in_one_word() {
        let red = Style::default().fg(Color::Red);
        let blue = Style::default().fg(Color::Blue);
        // "main.py" is one word spanning two styled spans: it must move to
        // the next line whole, with both styles intact.
        let line = Line::from(vec![
            Span::styled("run ma", red),
            Span::styled("in.py", blue),
        ]);
        let lines = wrap_lines(Text::from(line), 7);
        assert_eq!(flats(&lines), ["run", "main.py"]);
        assert_eq!(lines[1].spans[0].content.as_ref(), "ma");
        assert_eq!(lines[1].spans[0].style, red);
        assert_eq!(lines[1].spans[1].content.as_ref(), "in.py");
        assert_eq!(lines[1].spans[1].style, blue);
    }

    #[test]
    fn wrap_wide_chars_never_exceed_width() {
        let lines = wrap_lines(Text::from(Line::raw("日本語のテスト")), 5);
        assert_eq!(flats(&lines), ["日本", "語の", "テス", "ト"]);
        for line in &lines {
            assert!(line.width() <= 5);
        }
    }

    #[test]
    fn wrap_keeps_combining_marks_with_base_char() {
        let lines = wrap_lines(Text::from(Line::raw("e\u{301}".repeat(5))), 3);
        assert_eq!(flats(&lines), ["e\u{301}".repeat(3), "e\u{301}".repeat(2)]);
    }

    #[test]
    fn wrap_leaves_short_lines_untouched() {
        let line = Line::from(vec![Span::styled("❯ ", dim()), Span::raw("hi")]);
        let lines = wrap_lines(Text::from(line.clone()), 10);
        assert_eq!(lines, vec![line]);
    }

    #[test]
    fn hanging_indent_detects_gutter_marks() {
        assert_eq!(hanging_indent(&Line::raw("❯ hello")), 2);
        assert_eq!(hanging_indent(&Line::raw("· hello")), 2);
        assert_eq!(hanging_indent(&Line::raw("✓ tool")), 2);
        assert_eq!(hanging_indent(&Line::raw("  • item")), 4);
        assert_eq!(hanging_indent(&Line::raw("  plain")), 2);
        assert_eq!(hanging_indent(&Line::raw("plain text")), 0);
        // A dim rule is not a mark (wider than two columns of glyphs).
        assert_eq!(hanging_indent(&Line::raw("────────")), 0);
    }

    #[test]
    fn truncate_line_cuts_with_dim_ellipsis() {
        let red = Style::default().fg(Color::Red);
        let line = Line::from(vec![Span::raw("abc"), Span::styled("defgh", red)]);
        let out = truncate_line(line, 5);
        assert_eq!(flat(&out), "abcd…");
        assert_eq!(out.spans[1].style, red);
        assert_eq!(out.spans.last().unwrap().content.as_ref(), "…");
        assert_eq!(out.spans.last().unwrap().style, dim());
        assert!(out.width() <= 5);
    }

    #[test]
    fn truncate_line_leaves_fitting_lines_alone() {
        let line = Line::raw("short");
        assert_eq!(truncate_line(line.clone(), 10), line);
    }

    const TABLE: &str = "| Field | Value |\n|---|---:|\n\
                         | **Capital** | N'Djamena |\n| Population | ~19-20 million |\n";

    fn span_style(line: &Line, content: &str) -> Style {
        line.spans
            .iter()
            .find(|span| span.content.as_ref() == content)
            .unwrap_or_else(|| panic!("no span {content:?} in {:?}", flat(line)))
            .style
    }

    #[test]
    fn markdown_table_renders_as_aligned_grid() {
        let text = render_markdown(TABLE);
        assert_eq!(
            flats(&text.lines),
            vec![
                "Field      │          Value",
                "───────────┼───────────────",
                "Capital    │      N'Djamena",
                "Population │ ~19-20 million",
            ]
        );
    }

    #[test]
    fn markdown_table_right_aligns_column_by_padding_left() {
        let text = render_markdown("| a | num |\n|---|--:|\n| b | 7 |\n");
        assert_eq!(flats(&text.lines), vec!["a │ num", "──┼────", "b │   7"]);
    }

    #[test]
    fn markdown_table_preserves_inline_styling() {
        let text = render_markdown(TABLE);
        let header = span_style(&text.lines[0], "Field");
        assert!(header.add_modifier.contains(Modifier::BOLD));
        let strong = span_style(&text.lines[2], "Capital");
        assert!(strong.add_modifier.contains(Modifier::BOLD));
        let plain = span_style(&text.lines[2], "N'Djamena");
        assert!(!plain.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn markdown_table_pads_ragged_rows_as_empty_cells() {
        let text = render_markdown("| a | b | c |\n|---|---|---|\n| long-cell |\n");
        let widths: Vec<usize> = flats(&text.lines)
            .iter()
            .map(|line| UnicodeWidthStr::width(line.as_str()))
            .collect();
        assert_eq!(widths, vec![17, 17, 17]);
    }

    #[test]
    fn markdown_table_fits_narrow_width_without_soft_wrap_mid_grid() {
        // Natural width of this table is well over 40 columns. Laid out into 40,
        // every row (and the header rule) must fit, and every data row still
        // carries a `│` separator — soft-wrapping the whole line would drop
        // columns off the right edge or split mid-cell without a rule.
        let md = "\
| Stage | What people actually do | What it buys |
|---|---|---|
| Pretraining (from scratch with latents) | Almost nobody at scale | Deep internalization |
| Fine-tune a pretrained LM | Standard path (Coconut, ICoT) | Reuse English fluency |
";
        let text = render_markdown_at(md, 40);
        let flats = flats(&text.lines);
        assert!(!flats.is_empty());
        for line in &flats {
            assert!(
                UnicodeWidthStr::width(line.as_str()) <= 40,
                "row exceeds budget: {line:?} (width {})",
                UnicodeWidthStr::width(line.as_str())
            );
        }
        // Header rule still present and within budget.
        assert!(
            flats.iter().any(|line| line.contains('┼')),
            "expected a header rule, got {flats:?}"
        );
        // At least one body line still has a column separator — the grid
        // survived the shrink, it wasn't flattened into free text.
        let body: Vec<_> = flats.iter().filter(|line| line.contains('│')).collect();
        assert!(
            body.len() >= 2,
            "expected header + body with separators, got {flats:?}"
        );
    }

    #[test]
    fn markdown_table_wraps_long_cells_inside_columns() {
        // A two-column table forced into a tight budget: the long left cell
        // must wrap onto a second line under its own column, not push past │.
        let md = "| left | right |\n|---|---|\n| wordy phrase here | ok |\n";
        let text = render_markdown_at(md, 20);
        let flats = flats(&text.lines);
        // Natural single-line would be ~"wordy phrase here │ ok" (~22+); under
        // 20 the first body row wraps.
        let body: Vec<_> = flats
            .iter()
            .filter(|line| line.contains('│') && !line.contains("left"))
            .cloned()
            .collect();
        assert!(
            body.len() >= 2,
            "expected multi-line wrapped body, got {flats:?}"
        );
        for line in &body {
            assert!(
                UnicodeWidthStr::width(line.as_str()) <= 20,
                "wrapped body line too wide: {line:?}"
            );
        }
    }

    #[test]
    fn fit_column_widths_never_goes_below_one() {
        let mut widths = vec![10, 10, 10];
        fit_column_widths(&mut widths, 2);
        assert_eq!(widths, vec![1, 1, 1]);
        assert_eq!(widths.iter().sum::<usize>(), 3);
    }

    #[test]
    fn fit_column_widths_trims_the_widest_first() {
        let mut widths = vec![20, 5, 5];
        fit_column_widths(&mut widths, 20);
        assert_eq!(widths.iter().sum::<usize>(), 20);
        assert!(widths[0] >= widths[1] && widths[0] >= widths[2]);
        assert!(widths.iter().all(|&w| w >= 1));
    }

    #[test]
    fn latex_to_unicode_maps_greek_and_scripts() {
        assert_eq!(latex_to_unicode(r"\alpha + \beta = \gamma"), "α + β = γ");
        assert_eq!(latex_to_unicode(r"x^2 + y_1"), "x² + y₁");
        assert_eq!(latex_to_unicode(r"\mathbb{E}[X]"), "𝔼[X]");
        assert_eq!(latex_to_unicode(r"\mathbb{R}"), "ℝ");
        // \frac becomes a slash form — readable in one terminal row.
        assert_eq!(latex_to_unicode(r"\frac{1}{2}"), "1/2");
        assert_eq!(latex_to_unicode(r"\sqrt{x}"), "√x");
        // Whole-command matching: `\in` must not nibble `\infty`.
        assert_eq!(latex_to_unicode(r"x \in \mathbb{R}"), "x ∈ ℝ");
        assert_eq!(latex_to_unicode(r"\infty"), "∞");
        assert_eq!(latex_to_unicode(r"A^\top"), "Aᵀ");
    }

    #[test]
    fn latex_to_unicode_strips_font_wrappers() {
        // The screenshot case: nested \mathrm/\mathbf/\boldsymbol + blackboard E.
        let tex = r"\mathrm{Cov}(\mathbf{z}_{\mathrm{pos}}, \mathbf{z}_{\mathrm{neg}}) = \mathbb{E}[(\mathbf{z}_{\mathrm{pos}} - \boldsymbol{\mu}_{\mathrm{pos}})(\mathbf{z}_{\mathrm{neg}} - \boldsymbol{\mu}_{\mathrm{neg}})^\top]";
        let out = latex_to_unicode(tex);
        // No raw TeX commands or `$` left.
        assert!(!out.contains('\\'), "still has backslash: {out}");
        assert!(!out.contains("mathrm"), "{out}");
        assert!(!out.contains("mathbf"), "{out}");
        assert!(out.contains('𝔼'), "expected 𝔼 in {out}");
        assert!(
            out.contains('μ') || out.contains("mu"),
            "expected mu in {out}"
        );
        // Transpose: superscript T preferred over caret+⊤.
        assert!(
            out.contains('ᵀ') || out.contains('⊤') || out.contains("^T"),
            "expected transpose in {out}"
        );
        // pos fully maps to subscripts; neg has no subscript-g so falls back flat.
        assert!(
            out.contains("ₚₒₛ") || out.contains("_pos"),
            "expected pos subscript form in {out}"
        );
        assert!(
            out.contains("Cov"),
            "expected Cov operator name kept, got {out}"
        );
    }

    #[test]
    fn markdown_inline_math_renders_as_unicode() {
        let text = render_markdown(r"See $\alpha + \beta$ for details.");
        let joined = flats(&text.lines).join("\n");
        assert!(
            joined.contains("α + β"),
            "expected unicode math, got {joined:?}"
        );
        assert!(
            !joined.contains('$') && !joined.contains('\\'),
            "raw delimiters leaked: {joined:?}"
        );
    }

    #[test]
    fn markdown_display_math_is_indented_on_own_line() {
        let text = render_markdown("before\n\n$$\\sum_{i=1}^{n} x_i$$\n\nafter\n");
        let flats = flats(&text.lines);
        // Display math is its own indented line, not jammed into prose.
        assert!(
            flats
                .iter()
                .any(|line| line.contains('∑') && line.starts_with("  ")),
            "expected indented display math, got {flats:?}"
        );
        assert!(
            flats.iter().any(|line| line.contains("before")),
            "{flats:?}"
        );
        assert!(flats.iter().any(|line| line.contains("after")), "{flats:?}");
    }

    fn cs(text: &str) -> Vec<char> {
        text.chars().collect()
    }

    #[test]
    fn composer_wrap_keeps_short_lines_whole() {
        assert_eq!(wrap_rows(&cs("hello"), 10), vec![(0, 5)]);
        assert_eq!(wrap_rows(&cs(""), 10), vec![(0, 0)]);
    }

    #[test]
    fn composer_wrap_splits_long_lines_at_the_budget() {
        // "abcdef" at 3 columns: two full rows; at 4: 4 + 2.
        assert_eq!(wrap_rows(&cs("abcdef"), 3), vec![(0, 3), (3, 6)]);
        assert_eq!(wrap_rows(&cs("abcdef"), 4), vec![(0, 4), (4, 6)]);
    }

    #[test]
    fn composer_wrap_respects_hard_breaks_and_trailing_newline() {
        // The '\n' belongs to no row; a trailing one yields an empty last row.
        assert_eq!(wrap_rows(&cs("ab\ncd"), 10), vec![(0, 2), (3, 5)]);
        assert_eq!(wrap_rows(&cs("ab\n"), 10), vec![(0, 2), (3, 3)]);
        assert_eq!(wrap_rows(&cs("a\n\nb"), 10), vec![(0, 1), (2, 2), (3, 4)]);
    }

    #[test]
    fn composer_wrap_never_splits_a_wide_char() {
        // '你' is 2 columns; at budget 3 it doesn't fit after "ab" and moves
        // whole to the next row.
        assert_eq!(wrap_rows(&cs("ab你c"), 3), vec![(0, 2), (2, 4)]);
    }

    #[test]
    fn composer_cursor_maps_through_soft_wraps() {
        let rows = wrap_rows(&cs("abcdef"), 3); // (0,3) (3,6)
        assert_eq!(cursor_visual(&rows, 2), (0, 2));
        // Exactly on the wrap boundary: start of the next visual row.
        assert_eq!(cursor_visual(&rows, 3), (1, 0));
        // End of text: end of the last row.
        assert_eq!(cursor_visual(&rows, 6), (1, 3));
    }

    #[test]
    fn composer_cursor_stays_on_its_row_at_hard_breaks() {
        let rows = wrap_rows(&cs("ab\ncd"), 10); // (0,2) (3,5)
        // On the '\n' itself: end of the row before it.
        assert_eq!(cursor_visual(&rows, 2), (0, 2));
        assert_eq!(cursor_visual(&rows, 3), (1, 0));
        assert_eq!(cursor_visual(&rows, 5), (1, 2));
    }

    #[test]
    fn composer_cursor_on_empty_input_is_origin() {
        let rows = wrap_rows(&cs(""), 10);
        assert_eq!(cursor_visual(&rows, 0), (0, 0));
    }

    #[test]
    fn long_input_soft_wraps_instead_of_scrolling() {
        let mut app = App::new(crate::config::Config::default());
        app.input = "x".repeat(100);
        app.cursor = 100;

        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();

        // 80 columns leave 76 for text, so 100 chars fill one row and wrap 24
        // onto a continuation row. The composer sits above the status line:
        // rule (19), two text rows (20–21), rule (22).
        let buffer = terminal.backend().buffer().clone();
        let row =
            |y: u16| -> String { (0..80).map(|x| buffer[(x, y)].symbol()).collect::<String>() };
        assert_eq!(row(20).trim_end(), format!(" ❯ {}", "x".repeat(76)));
        assert_eq!(row(21).trim_end(), format!("   {}", "x".repeat(24)));
        // The caret follows onto the wrapped row instead of the old
        // horizontal scroll keeping everything on one line.
        let cursor = terminal.get_cursor_position().unwrap();
        assert_eq!((cursor.x, cursor.y), (3 + 24, 21));
    }

    /// Render `app` at 80x24 and return the screen as one string per row.
    fn render(app: &App) -> Vec<String> {
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..24)
            .map(|y| {
                (0..80)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn app_with_run() -> App {
        let mut app = App::new(crate::config::Config::default());
        app.handle_agent_event(crate::agent::AgentEvent::SubagentRunStarted {
            run: 1,
            bg: Some(1),
            name: "researcher".to_string(),
            task: "map the auth flow".to_string(),
        });
        app
    }

    #[test]
    fn the_rail_paints_a_dot_per_subagent_under_the_composer() {
        let mut app = app_with_run();
        app.handle_agent_event(crate::agent::AgentEvent::SubagentRunToolStarted {
            run: 1,
            name: "read_file".to_string(),
            args: serde_json::json!({}),
        });

        let rows = render(&app);
        // The rail sits between the composer and the status bar (row 23).
        let rail = rows
            .iter()
            .find(|row| row.contains("researcher"))
            .expect("the rail shows the run");
        assert!(rail.contains("read_file"), "shows what it is doing: {rail}");
        // Unread work is badged, so you can tell it moved while you looked away.
        assert!(rail.contains("+1"), "shows the unread badge: {rail}");
    }

    #[test]
    fn no_subagents_means_no_rail_and_no_lost_rows() {
        let bare = App::new(crate::config::Config::default());
        let with_run = app_with_run();
        // The rail costs nothing until there is something to show, and then
        // takes exactly the one row it needs.
        assert_eq!(rail_height(&bare), 0);
        assert_eq!(rail_height(&with_run), 1);
    }

    #[test]
    fn todo_overlay_sits_above_the_composer_without_stealing_chat_width() {
        use crate::tools::todo::{TodoItem, TodoStatus};

        let mut app = App::new(crate::config::Config::default());
        app.show_todos = true;
        app.todos = vec![
            TodoItem {
                content: "done already".to_string(),
                status: TodoStatus::Completed,
            },
            TodoItem {
                content: "working on this".to_string(),
                status: TodoStatus::InProgress,
            },
            TodoItem {
                content: "later".to_string(),
                status: TodoStatus::Pending,
            },
        ];
        app.transcript
            .push(TranscriptEntry::User("full-width chat line".to_string()));

        let rows = render(&app);
        let joined = rows.join("\n");
        assert!(
            joined.contains("todos 1/3"),
            "overlay title shows progress: {joined}"
        );
        assert!(
            joined.contains("working on this"),
            "current item is visible: {joined}"
        );
        assert!(
            rows.iter().any(|r| r.contains("full-width chat line")),
            "transcript still shows under the overlay: {joined}"
        );
        // Chat keeps full width: the diff sidebar uses a LEFT border on the
        // right 40%, so a pure todo-sidebar would leave an empty right column.
        // With the overlay, the title/items span the full terminal width.
        let title = rows
            .iter()
            .find(|r| r.contains("todos 1/3"))
            .expect("title row");
        // Title is left-anchored (not in a right-hand 40% pane starting ~col 48).
        let title_col = title.find("todos 1/3").expect("title text");
        assert!(
            title_col < 20,
            "todo overlay is left-anchored above the input, not a right sidebar: col={title_col} row={title:?}"
        );
    }

    #[test]
    fn attaching_replaces_the_chat_with_the_subagents_own_transcript() {
        let mut app = app_with_run();
        app.transcript
            .push(TranscriptEntry::User("main conversation".to_string()));
        app.handle_agent_event(crate::agent::AgentEvent::SubagentRunText {
            run: 1,
            text: "the auth flow starts in login.rs".to_string(),
        });
        app.attach_pane(0);

        let rows = render(&app);
        let screen = rows.join("\n");
        // The pane took over: its header names the run, its message is on
        // screen, and the main conversation is not.
        assert!(screen.contains("researcher"), "{screen}");
        assert!(screen.contains("running"), "{screen}");
        assert!(
            screen.contains("the auth flow starts in login.rs"),
            "{screen}"
        );
        assert!(!screen.contains("main conversation"), "{screen}");
        // And there is a way back.
        assert!(screen.contains("esc back"), "{screen}");
    }
}
