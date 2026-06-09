//! Ratatui rendering: pure functions from [`App`] state to widgets.
//! Layout: chat transcript (with optional git diff sidebar) above the input
//! line and a quiet status line. Floating layers: the command-suggestion
//! popup, the model/mode picker, and the approval modal.
//!
//! Design rules (do not regress):
//! - **Transparent**: never paint a background color; everything renders on
//!   `Color::Reset` so the user's terminal background shows through.
//!   Selection reads through an accent marker + bold, not opaque slabs.
//! - **One accent** ([`ACCENT`]) for chrome plus dim grays; green/red only
//!   as success/error semantics, cyan only for inline code.
//! - **No heavy boxes**: borderless sections separated by padding and dim
//!   rules; rounded dim borders only on floating layers.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Mutex, OnceLock};

use pulldown_cmark::{CodeBlockKind, Event as MdEvent, Options, Parser, Tag, TagEnd};
use ratatui::Frame;
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

use crate::app::{App, InputMode, TranscriptEntry};
use crate::config::Mode;

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// The single accent color used for chrome (prompt, gutters, names,
/// attention borders).
const ACCENT: Color = Color::Magenta;
/// Dim chrome: rules, gutter marks, hints, secondary borders.
const DIM: Color = Color::DarkGray;
/// Secondary text (tool output, user echo, details).
const TEXT_DIM: Color = Color::Gray;
/// Inline code (block code gets syntect foreground colors, or [`TEXT_DIM`]
/// when plain).
const CODE: Color = Color::Cyan;

fn dim() -> Style {
    Style::default().fg(DIM)
}

fn accent() -> Style {
    Style::default().fg(ACCENT)
}

/// Render one frame. The only entry point the main loop calls; everything
/// else in this module is a helper.
pub fn draw(frame: &mut Frame, app: &App) {
    let [main_area, input_area, status_area] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(2),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    if app.show_diff {
        let [chat_area, diff_area] =
            Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)])
                .areas(main_area);
        draw_transcript(frame, app, chat_area);
        draw_diff_sidebar(frame, app, diff_area);
    } else {
        draw_transcript(frame, app, main_area);
    }

    draw_input(frame, app, input_area);
    draw_status_bar(frame, app, status_area);

    // Floating layers, back to front.
    if app.picker.is_none() && app.pending_approval.is_none() {
        draw_suggestions(frame, app, input_area);
    }
    if app.picker.is_some() {
        draw_picker(frame, app);
    }
    if app.pending_approval.is_some() {
        draw_approval_modal(frame, app);
    }
}

/// Chat transcript: user/assistant messages with streaming markdown and
/// collapsible tool cards. Borderless; a one-column side margin keeps the
/// text off the terminal edge. Shows the welcome screen while empty.
fn draw_transcript(frame: &mut Frame, app: &App, area: Rect) {
    if app.transcript.is_empty() && app.streaming.is_empty() && !app.status.busy {
        draw_welcome(frame, app, area);
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
    let lines: Vec<Line<'static>> = vec![
        Line::from(Span::styled("✦", accent())),
        Line::raw(""),
        Line::from(Span::styled(
            "w i z a r d",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "your sovereign coding wizard — self-extending, fully local",
            dim().italic(),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled(app.status.model.clone(), Style::default().fg(TEXT_DIM)),
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
            Span::styled("  pick a model (or Ctrl-P)", dim()),
        ]),
        Line::from(vec![
            Span::styled("/help", accent()),
            Span::styled("  all commands & keys", dim()),
        ]),
    ];

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
        Mode::Sovereign => Span::styled("sovereign", Style::default().fg(Color::Red).bold()),
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
                    Style::default().fg(Color::Red)
                } else {
                    dim().italic()
                };
                for line in message.lines() {
                    lines.push(Line::from(Span::styled(format!("  {line}"), style)));
                }
            }
        }
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
            Span::styled("thinking…", dim().italic()),
        ]));
    }

    Text::from(lines)
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
        (Some(_), false) => Span::styled("✓", Style::default().fg(Color::Green)),
        (Some(_), true) => Span::styled("✗", Style::default().fg(Color::Red)),
    };

    let summary = if args.is_null() {
        String::new()
    } else {
        truncate_width(&serde_json::to_string(args).unwrap_or_default(), 64)
    };
    let mut card = vec![
        glyph,
        Span::raw(" "),
        Span::styled(name.to_string(), accent()),
    ];
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

/// Git diff sidebar (`/diff`): separated from the chat by a single dim
/// rule, syntax-highlighted (foreground colors only).
fn draw_diff_sidebar(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::new()
        .borders(Borders::LEFT)
        .border_style(dim())
        .title(Line::from(vec![
            Span::styled(" ± ", accent()),
            Span::styled("git diff", Style::default().fg(TEXT_DIM)),
        ]));
    let paragraph = Paragraph::new(highlight_diff(&app.diff_text)).block(block);
    frame.render_widget(paragraph, area);
}

/// Bottom status line: model, mode, and turn state on the left; contextual
/// key hints on the right. One quiet line, no background fill.
fn draw_status_bar(frame: &mut Frame, app: &App, area: Rect) {
    let spinner = SPINNER[(app.tick as usize) % SPINNER.len()];
    let mut spans = vec![
        Span::styled(" ✦ ", accent()),
        Span::styled(app.status.model.clone(), Style::default().fg(TEXT_DIM)),
        Span::styled(" · ", dim()),
        mode_span(app.status.mode),
    ];
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
    let line = Line::from(spans);
    let left_width = line.width() as u16;
    frame.render_widget(Paragraph::new(line), area);

    // Contextual key hints, right-aligned in a sub-rect so the left side is
    // never overdrawn.
    let hints = if app.pending_approval.is_some() {
        "y approve · n deny"
    } else if app.picker.is_some() {
        "↑↓ move · Enter select · Esc cancel"
    } else if !app.suggestions.is_empty() {
        "↑↓ select · Tab complete · Enter run"
    } else if app.status.busy {
        "PgUp/PgDn scroll · ^C quit"
    } else {
        "/ commands · ↑ history · ^P model · ^C quit"
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

/// Input: a dim rule above a clean accent prompt — no box. Handles
/// cursor-aware horizontal scrolling and inline ghost-text completion.
fn draw_input(frame: &mut Frame, app: &App, area: Rect) {
    if area.height < 2 || area.width < 6 {
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

    if app.input_mode == InputMode::Approval {
        let line = Line::from(vec![
            Span::raw(" "),
            Span::styled("❯ ", dim().bold()),
            Span::styled("awaiting approval — y approve · n deny", dim().italic()),
        ]);
        frame.render_widget(Paragraph::new(Text::from(vec![rule, line])), area);
        return;
    }

    let chars: Vec<char> = app.input.chars().collect();
    let widths: Vec<usize> = chars.iter().map(|c| c.width().unwrap_or(0)).collect();
    let cursor = app.cursor.min(chars.len());
    // Keep the cursor visible: scroll the window (in display columns, so
    // wide CJK/emoji glyphs count properly) until the cursor column fits,
    // truncating the tail if needed.
    let mut start = 0usize;
    let mut cursor_cols: usize = widths[..cursor].iter().sum();
    while start < cursor && cursor_cols > budget - 1 {
        cursor_cols -= widths[start];
        start += 1;
    }
    let mut end = start;
    let mut used_cols = 0usize;
    while end < chars.len() && used_cols + widths[end] <= budget {
        used_cols += widths[end];
        end += 1;
    }
    let visible: String = chars[start..end].iter().collect();
    let cursor_x = area.x + (pad + prompt_width) as u16 + cursor_cols as u16;

    let mut spans = vec![
        Span::raw(" "),
        Span::styled("❯ ", accent().bold()),
        Span::raw(visible),
    ];

    // Ghost text: the untyped remainder of the highlighted suggestion plus
    // its argument hint, dimmed (only when the whole input is visible and
    // the cursor sits at the end, where → can actually accept it).
    if start == 0
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
                ghost.push_str(spec.args);
            }
            let room = budget.saturating_sub(used_cols);
            if !ghost.is_empty() && room > 0 {
                let ghost: String = ghost.chars().take(room).collect();
                spans.push(Span::styled(ghost, dim().italic()));
            }
        }
    }

    frame.render_widget(
        Paragraph::new(Text::from(vec![rule, Line::from(spans)])),
        area,
    );

    if app.picker.is_none() {
        frame.set_cursor_position(Position::new(cursor_x, area.y + 1));
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
                    format!("  {}", truncate_width(spec.description, description_room)),
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

/// Centered modal for the model / mode picker.
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
            let mut spans = vec![
                Span::styled(marker, accent()),
                Span::styled(truncate_width(&item.value, value_room), value_style),
            ];
            if item.current {
                spans.push(Span::styled(" ●", Style::default().fg(Color::Green)));
            }
            if !item.detail.is_empty() {
                spans.push(Span::styled(format!("  {}", item.detail), dim()));
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
        .title_bottom(
            Line::from(Span::styled(" ↑↓ move · Enter select · Esc cancel ", dim())).centered(),
        );
    frame.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
}

/// Centered modal asking the user to approve a gated tool call.
fn draw_approval_modal(frame: &mut Frame, app: &App) {
    let Some(pending) = &app.pending_approval else {
        return;
    };
    let frame_area = frame.area();

    let args = serde_json::to_string_pretty(&pending.call.function.arguments)
        .unwrap_or_else(|_| "{}".to_string());
    let arg_lines: Vec<&str> = args.lines().collect();

    // Size the modal to its content: tool line + blank + argument block
    // (+ overflow ellipsis) + blank + buttons, plus the borders; capped so
    // it always fits the frame.
    let max_args = frame_area.height.saturating_sub(7).max(3) as usize;
    let shown = arg_lines.len().min(max_args);
    let truncated = arg_lines.len() > shown;
    let height = (shown + truncated as usize + 6) as u16;
    let width = (frame_area.width as u32 * 70 / 100).max(1) as u16;
    let area = Rect {
        x: frame_area.x + (frame_area.width.saturating_sub(width)) / 2,
        y: frame_area.y + (frame_area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
    .intersection(frame_area);
    frame.render_widget(Clear, area);

    let mut lines: Vec<Line<'static>> = vec![
        Line::from(vec![
            Span::styled("tool ", dim()),
            Span::styled(pending.call.function.name.clone(), accent().bold()),
        ]),
        Line::raw(""),
    ];

    for line in arg_lines.iter().take(shown) {
        lines.push(Line::from(Span::styled(
            line.to_string(),
            Style::default().fg(TEXT_DIM),
        )));
    }
    if truncated {
        lines.push(Line::from(Span::styled("…", dim())));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled("y", Style::default().fg(Color::Green).bold()),
        Span::styled(" approve", dim()),
        Span::raw("    "),
        Span::styled("n", Style::default().fg(Color::Red).bold()),
        Span::styled(" deny", dim()),
    ]));

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(accent())
        .title(Line::from(vec![
            Span::styled(" ✦", accent()),
            Span::styled(" approve tool call? ", Style::default().fg(TEXT_DIM)),
        ]));
    frame.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
}

/// Wrap styled lines at `width` display columns (wide CJK/emoji glyphs
/// count as two) so the transcript can be pinned exactly to its bottom.
/// A wide char that no longer fits wraps to the next line first;
/// zero-width chars (combining marks) always stay with their base char.
fn wrap_lines(text: Text<'static>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut out: Vec<Line<'static>> = Vec::new();
    for line in text.lines {
        let mut current: Vec<Span<'static>> = Vec::new();
        let mut used = 0usize;
        for span in line.spans {
            let style = span.style;
            let mut buffer = String::new();
            for ch in span.content.chars() {
                let ch_width = ch.width().unwrap_or(0);
                if ch_width > 0 && used + ch_width > width && used > 0 {
                    if !buffer.is_empty() {
                        current.push(Span::styled(std::mem::take(&mut buffer), style));
                    }
                    out.push(Line::from(std::mem::take(&mut current)));
                    used = 0;
                }
                buffer.push(ch);
                used += ch_width;
            }
            if !buffer.is_empty() {
                current.push(Span::styled(buffer, style));
            }
        }
        out.push(Line::from(current));
    }
    out
}

/// Truncate to `max` display columns (not chars), appending `…` when cut.
fn truncate_width(text: &str, max: usize) -> String {
    if text.width() <= max {
        return text.to_string();
    }
    let budget = max.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let ch_width = ch.width().unwrap_or(0);
        if used + ch_width > budget {
            break;
        }
        out.push(ch);
        used += ch_width;
    }
    out.push('…');
    out
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

/// Map a syntect style to ratatui, keeping only the foreground color and
/// font modifiers — backgrounds would paint over the terminal transparency.
fn syntect_style(style: syntect::highlighting::Style) -> Style {
    let fg = style.foreground;
    let mut out = Style::default().fg(Color::Rgb(fg.r, fg.g, fg.b));
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
                Style::default().fg(Color::Green)
            } else if line.starts_with('-') {
                Style::default().fg(Color::Red)
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
