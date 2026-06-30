//! Ratatui rendering: pure functions from [`App`] state to widgets.
//! Layout: chat transcript (with optional git diff sidebar) above the input
//! line and a quiet status line. Floating layers: the command-suggestion
//! popup and the model/mode/rewind/subagent picker.
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

use pulldown_cmark::{CodeBlockKind, Event as MdEvent, Options, Parser, Tag, TagEnd};
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

use crate::app::{App, InputMode, Selection, TranscriptEntry};
use crate::config::Mode;
use crate::session_registry::SessionState;
use crate::vim::VimMode;

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Tallest the multi-line composer grows before it scrolls internally.
const MAX_INPUT_ROWS: u16 = 10;

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
    // The composer grows with its content (one row per hard line break) up to
    // MAX_INPUT_ROWS, then scrolls internally. +2 for the rules above/below.
    let input_rows = (app.input.split('\n').count() as u16).clamp(1, MAX_INPUT_ROWS);
    let [main_area, input_area, status_area] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(input_rows + 2),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    if app.show_diff || app.show_todos {
        let [chat_area, side_area] =
            Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)])
                .areas(main_area);
        draw_transcript(frame, app, chat_area);
        match (app.show_todos, app.show_diff) {
            (true, true) => {
                // Both panels share the sidebar: todos on top (sized to the
                // list), the diff below.
                let todo_height = (app.todos.len() as u16 + 1).clamp(2, side_area.height / 2);
                let [todo_area, diff_area] =
                    Layout::vertical([Constraint::Length(todo_height), Constraint::Min(1)])
                        .areas(side_area);
                draw_todo_sidebar(frame, app, todo_area);
                draw_diff_sidebar(frame, app, diff_area);
            }
            (true, false) => draw_todo_sidebar(frame, app, side_area),
            _ => draw_diff_sidebar(frame, app, side_area),
        }
    } else {
        draw_transcript(frame, app, main_area);
    }

    draw_input(frame, app, input_area);
    draw_status_bar(frame, app, status_area);

    // Floating layers, back to front.
    if app.picker.is_none()
        && app.plan_review.is_none()
        && app.interview.is_none()
        && !app.show_dashboard
        && !app.show_subagents
    {
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
    // The dashboard and subagent monitor are modal and full-screen, so they
    // paint last (on top). Only one is ever open at a time.
    if app.show_dashboard {
        draw_dashboard(frame, app);
    }
    if app.show_subagents {
        draw_subagents(frame, app);
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
    // Stay on the welcome screen until the conversation actually begins (see
    // `App::has_conversation`: early system notices alone don't dismiss it).
    if !app.has_conversation() && app.streaming.is_empty() && !app.status.busy {
        // A slash-command menu (e.g. `/provider`) or other modal floats over a
        // small centered area; the welcome card would show through around it.
        // Drop the card while any overlay is open so there's no text overlay.
        let overlay_open = app.picker.is_some()
            || app.plan_review.is_some()
            || app.interview.is_some()
            || app.show_dashboard
            || app.show_subagents;
        if !overlay_open {
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

    let lines = wrap_lines(transcript_text(app), inner_width);
    let total = lines.len();
    let max_scroll = total.saturating_sub(inner_height);
    let scroll = (app.scroll as usize).min(max_scroll);
    let start = max_scroll - scroll;
    let end = (start + inner_height).min(total);
    let visible: Vec<Line<'static>> = lines[start..end].to_vec();

    frame.render_widget(Paragraph::new(Text::from(visible)), inner);

    // Scrolled away from the tail: a quiet hint in the top-right corner.
    if scroll > 0 {
        let label = format!("↓ {scroll} more ");
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

/// Build the full (unwrapped) transcript text from app state.
fn transcript_text(app: &App) -> Text<'static> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut prev_tool = false;
    let mut prev_notice = false;
    let mut first = true;

    for entry in &app.transcript {
        let is_tool = matches!(entry, TranscriptEntry::ToolCard { .. });
        let is_notice = matches!(entry, TranscriptEntry::Notice(_));
        // Comfortable spacing between turns; runs of tool cards or notices
        // stay tight so they read as one group.
        let tight = (is_tool && prev_tool) || (is_notice && prev_notice);
        if !first && !tight {
            lines.push(Line::raw(""));
        }
        first = false;
        prev_tool = is_tool;
        prev_notice = is_notice;

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
                    render_markdown(message),
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
                    app.tick,
                );
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
    }

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
        let mut text = render_markdown_streaming(&app.streaming);
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

    Text::from(lines)
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
/// when relevant (errors, or Ctrl-T).
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

/// Todo side panel (`/todos`, auto-shown on the first todo update): the
/// agent's working list with status glyphs — ✓ completed (dim,
/// struck-through), ▸ in progress (accent), ☐ pending.
fn draw_todo_sidebar(frame: &mut Frame, app: &App, area: Rect) {
    let (done, total) = crate::tools::todo::progress(&app.todos);
    let block = Block::new()
        .borders(Borders::LEFT)
        .border_style(dim())
        .title(Line::from(vec![
            Span::styled(" ≡ ", accent()),
            Span::styled(
                format!("todos {done}/{total}"),
                Style::default().fg(TEXT_DIM),
            ),
        ]));
    let inner_width = block.inner(area).width as usize;
    let lines: Vec<Line<'static>> = if app.todos.is_empty() {
        vec![Line::from(Span::styled("(empty)", dim().italic()))]
    } else {
        app.todos
            .iter()
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
    let token_total = app.status.prompt_tokens + app.status.completion_tokens;
    if token_total > 0 {
        spans.push(Span::styled(" · ", dim()));
        spans.push(Span::styled(
            crate::usage::format_tokens(token_total),
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
        spans.push(Span::styled(
            format!(
                "step {}/{} · {elapsed}s",
                app.status.step, app.status.max_steps
            ),
            dim(),
        ));
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
        "PgUp/PgDn scroll"
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

/// Input: a clean accent prompt bracketed by dim rules above and below — no
/// box. Handles cursor-aware horizontal scrolling and inline ghost-text
/// completion.
fn draw_input(frame: &mut Frame, app: &App, area: Rect) {
    if area.height < 3 || area.width < 6 {
        return;
    }
    let rule = Line::from(Span::styled("─".repeat(area.width as usize), dim()));

    // One column of left padding keeps the prompt aligned with the
    // transcript margin.
    let pad = 1usize;
    let prompt_width = 2usize;
    let budget = (area.width as usize)
        .saturating_sub(pad + prompt_width + 1)
        .max(1);

    // In the inline provider-setup prompt the API-key field is masked: each
    // typed character renders as a width-1 bullet (so the cursor math is
    // unaffected) and the real key never reaches the screen.
    let chars: Vec<char> = if app.prompt_is_masked() {
        vec!['•'; app.input.chars().count()]
    } else {
        app.input.chars().collect()
    };
    let cursor = app.cursor.min(chars.len());
    let normal = app.vim.is_normal();

    // Split the buffer into logical rows on hard line breaks; each row is the
    // half-open char range `[rs, re)` (the '\n' itself is not part of a row).
    let mut rows: Vec<(usize, usize)> = Vec::new();
    let mut start_char = 0usize;
    for (i, &c) in chars.iter().enumerate() {
        if c == '\n' {
            rows.push((start_char, i));
            start_char = i + 1;
        }
    }
    rows.push((start_char, chars.len()));

    // Locate the cursor (row, column-in-chars). `cursor <= re` puts an
    // end-of-row cursor (just before the break) on that row, not the next.
    let (mut crow, mut ccol) = (0usize, 0usize);
    for (ri, &(rs, re)) in rows.iter().enumerate() {
        if cursor <= re {
            crow = ri;
            ccol = cursor - rs;
            break;
        }
    }

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

        // Only the cursor's row scrolls horizontally to keep the caret visible;
        // other rows render from their start and truncate at the budget.
        let mut hstart = 0usize;
        if is_cursor_row {
            let mut cols: usize = widths[..ccol.min(widths.len())].iter().sum();
            while hstart < ccol && cols > budget - 1 {
                cols -= widths[hstart];
                hstart += 1;
            }
        }
        let mut hend = hstart;
        let mut used = 0usize;
        while hend < row.len() && used + widths[hend] <= budget {
            used += widths[hend];
            hend += 1;
        }

        // First row carries the prompt glyph; continuation rows indent to match.
        let leading = if ri == 0 {
            Span::styled("❯ ", accent().bold())
        } else {
            Span::raw("  ")
        };
        let mut spans = vec![Span::raw(" "), leading];

        if normal && is_cursor_row {
            // Vim Normal mode paints its own block cursor (reversed cell) so the
            // mode is legible without a hardware caret.
            let rel = ccol.saturating_sub(hstart).min(hend - hstart);
            spans.push(Span::raw(
                row[hstart..hstart + rel].iter().collect::<String>(),
            ));
            if hstart + rel < hend {
                spans.push(Span::styled(row[hstart + rel].to_string(), block));
                spans.push(Span::raw(
                    row[hstart + rel + 1..hend].iter().collect::<String>(),
                ));
            } else {
                spans.push(Span::styled(" ", block));
            }
        } else {
            spans.push(Span::raw(row[hstart..hend].iter().collect::<String>()));

            // Ghost text (command completion) only makes sense on a single-row
            // line with the cursor at the very end, where → can accept it.
            if is_cursor_row
                && !normal
                && rows.len() == 1
                && hstart == 0
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
                    let room = budget.saturating_sub(used);
                    if !ghost.is_empty() && room > 0 {
                        let ghost: String = ghost.chars().take(room).collect();
                        spans.push(Span::styled(ghost, dim().italic()));
                    }
                }
            }

            if is_cursor_row && !normal {
                let cols: usize = widths[hstart..ccol.min(widths.len())].iter().sum();
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

/// In-session subagent monitor (`/subagents`): the subagents that have run or
/// are running this session, with live status. Modal — Esc/Enter/q close it —
/// and reconstructed from the transcript each frame: a `spawn_subagent` card is
/// one run, and the `<name> ▸ <tool>` cards that follow it are its steps.
fn draw_subagents(frame: &mut Frame, app: &App) {
    let area = frame.area();
    frame.render_widget(Clear, area);

    // Walk the transcript once, grouping nested `▸` steps under their run.
    struct Run {
        name: String,
        task: String,
        /// 0 = running, 1 = completed, 2 = failed / hit budget.
        state: u8,
        steps: usize,
        current: Option<String>,
    }
    let mut runs: Vec<Run> = Vec::new();
    for entry in &app.transcript {
        let TranscriptEntry::ToolCard {
            name,
            args,
            output,
            is_error,
            ..
        } = entry
        else {
            continue;
        };
        if name == "spawn_subagent" {
            let who = args
                .get("subagent")
                .and_then(|v| v.as_str())
                .unwrap_or("subagent");
            let task = args.get("task").and_then(|v| v.as_str()).unwrap_or("");
            let state = match output {
                None => 0,
                Some(_) if *is_error => 2,
                Some(_) => 1,
            };
            runs.push(Run {
                name: who.to_string(),
                task: task.to_string(),
                state,
                steps: 0,
                current: None,
            });
        } else if let Some(tool) = name.split(" ▸ ").nth(1) {
            // A nested subagent step belongs to the most recent run.
            if let Some(run) = runs.last_mut() {
                run.steps += 1;
                run.current = if output.is_none() {
                    Some(tool.to_string())
                } else {
                    None
                };
            }
        }
    }

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(dim())
        .title(Line::from(vec![
            Span::styled(" ✦ ", accent()),
            Span::styled(
                "subagents · this session",
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ]))
        .title_bottom(Line::from(Span::styled(" Esc close · refreshes live ", dim())).centered());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width < 8 || inner.height < 4 {
        return;
    }
    let width = inner.width as usize;
    let spinner = SPINNER[(app.tick as usize) % SPINNER.len()];

    let mut lines: Vec<Line<'static>> = Vec::new();
    if runs.is_empty() {
        lines.push(dash_bullet(
            "no subagents have run this session",
            dim().italic(),
        ));
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "the agent delegates via spawn_subagent · /agents to browse the roster",
            dim().italic(),
        )));
    } else {
        // Running first (there is at most one at a time), then finished newest
        // first so the latest result is on top.
        let mut order: Vec<usize> = (0..runs.len()).filter(|&i| runs[i].state == 0).collect();
        order.extend((0..runs.len()).filter(|&i| runs[i].state != 0).rev());
        for i in order {
            let run = &runs[i];
            let (glyph, style) = match run.state {
                0 => (spinner.to_string(), accent()),
                1 => ("✓".to_string(), Style::default().fg(TEXT_DIM)),
                _ => ("✗".to_string(), Style::default().fg(Color::White).bold()),
            };
            let suffix = match run.state {
                0 => format!(" · step {}", run.steps.max(1)),
                2 => format!(" · stopped at {} steps", run.steps),
                _ => format!(" · {} steps", run.steps),
            };
            lines.push(truncate_line(
                Line::from(vec![
                    Span::styled(format!("{glyph} "), style),
                    Span::styled(
                        run.name.clone(),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(suffix, dim()),
                ]),
                width,
            ));
            if !run.task.is_empty() {
                lines.push(truncate_line(
                    Line::from(Span::styled(
                        format!("    {}", run.task),
                        Style::default().fg(TEXT_DIM),
                    )),
                    width,
                ));
            }
            if let Some(current) = &run.current {
                lines.push(truncate_line(
                    Line::from(vec![
                        Span::styled("    ▸ ", accent()),
                        Span::styled(current.clone(), dim()),
                    ]),
                    width,
                ));
            }
        }
    }

    let max = inner.height as usize;
    if lines.len() > max {
        lines.truncate(max);
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
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
        let lines = wrap_lines(render_markdown(&review.plan), body_area.width as usize);
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
/// syntax-highlighted, foreground colors only).
pub fn render_markdown(source: &str) -> Text<'static> {
    render_markdown_inner(source, true)
}

/// Render in-flight streaming markdown: identical, except code blocks stay
/// plain so per-frame rendering stays cheap.
fn render_markdown_streaming(source: &str) -> Text<'static> {
    render_markdown_inner(source, false)
}

fn render_markdown_inner(source: &str, highlight: bool) -> Text<'static> {
    let mut renderer = MarkdownRenderer {
        highlight,
        ..MarkdownRenderer::default()
    };
    let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    for event in Parser::new_ext(source, options) {
        renderer.event(event);
    }
    renderer.finish()
}

#[derive(Default)]
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
            Tag::Emphasis => self.italic += 1,
            Tag::Strong => self.bold += 1,
            Tag::Strikethrough => self.strike += 1,
            Tag::Link { dest_url, .. } => self.link = Some(dest_url.to_string()),
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
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

    fn finish(mut self) -> Text<'static> {
        self.flush();
        while matches!(self.lines.last(), Some(line) if line.spans.is_empty()) {
            self.lines.pop();
        }
        Text::from(self.lines)
    }
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
}
