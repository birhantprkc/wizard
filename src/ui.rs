//! Ratatui rendering: pure functions from [`App`] state to widgets.
//! Layout: chat transcript (with optional git diff sidebar) above a status
//! bar and the input line. Floating layers: the command-suggestion popup,
//! the model/mode picker, and the approval modal.

use std::sync::OnceLock;

use pulldown_cmark::{CodeBlockKind, Event as MdEvent, Options, Parser, Tag, TagEnd};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Margin, Position, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;
use unicode_width::UnicodeWidthChar;

use crate::app::{App, InputMode, PickerKind, TranscriptEntry};
use crate::config::Mode;

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Accent color used for chrome (borders, prompt, highlights).
const ACCENT: Color = Color::Magenta;
/// Background of the status bar and selected rows.
const SURFACE: Color = Color::Rgb(24, 24, 32);
const SELECTION: Color = Color::Rgb(45, 45, 65);

/// Render one frame. The only entry point the main loop calls; everything
/// else in this module is a helper.
pub fn draw(frame: &mut Frame, app: &App) {
    let [main_area, status_area, input_area] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(3),
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

    draw_status_bar(frame, app, status_area);
    draw_input(frame, app, input_area);

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
/// collapsible tool cards. Shows the welcome screen while empty.
fn draw_transcript(frame: &mut Frame, app: &App, area: Rect) {
    if app.transcript.is_empty() && app.streaming.is_empty() && !app.status.busy {
        draw_welcome(frame, app, area);
        return;
    }

    let mut block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(Line::from(vec![
            Span::styled(" ✦ ", Style::default().fg(ACCENT)),
            Span::styled("wizard ", Style::default().fg(Color::White).bold()),
        ]))
        .border_style(Style::default().fg(Color::DarkGray));

    let inner_width = area.width.saturating_sub(2).max(1) as usize;
    let inner_height = area.height.saturating_sub(2) as usize;

    let lines = wrap_lines(transcript_text(app), inner_width);
    let total = lines.len();
    let max_scroll = total.saturating_sub(inner_height);
    let scroll = (app.scroll as usize).min(max_scroll);
    let start = max_scroll - scroll;
    let end = (start + inner_height).min(total);
    let visible: Vec<Line<'static>> = lines[start..end].to_vec();

    if scroll > 0 {
        block = block.title_top(
            Line::from(Span::styled(
                format!(" ↓ {scroll} more "),
                Style::default().fg(Color::Yellow),
            ))
            .right_aligned(),
        );
    }
    frame.render_widget(Paragraph::new(Text::from(visible)).block(block), area);

    // Scrollbar along the right border once content overflows.
    if total > inner_height {
        let mut state = ScrollbarState::new(max_scroll + 1).position(start);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(Some("│"))
            .thumb_symbol("┃")
            .track_style(Style::default().fg(Color::DarkGray))
            .thumb_style(Style::default().fg(ACCENT));
        frame.render_stateful_widget(
            scrollbar,
            area.inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut state,
        );
    }
}

/// Welcome screen shown before the first message.
fn draw_welcome(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let dim = Style::default().fg(Color::DarkGray);
    let key = Style::default().fg(Color::Cyan);
    let lines: Vec<Line<'static>> = vec![
        Line::from(Span::styled("✦  ·  ✶  ·  ✦", Style::default().fg(ACCENT))),
        Line::raw(""),
        Line::from(Span::styled(
            "w  i  z  a  r  d",
            Style::default().fg(Color::White).bold(),
        )),
        Line::from(Span::styled(
            "your sovereign coding wizard — self-extending, fully local",
            dim.italic(),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled("model ", dim),
            Span::styled(app.status.model.clone(), Style::default().fg(Color::White)),
            Span::styled("   ·   mode ", dim),
            mode_span(app.status.mode),
        ]),
        Line::raw(""),
        Line::raw(""),
        Line::from(vec![
            Span::styled("type a message", Style::default().fg(Color::White)),
            Span::styled(" and press Enter to begin", dim),
        ]),
        Line::raw(""),
        Line::from(vec![
            Span::styled("/", key),
            Span::styled("  commands — Tab completes, ↑/↓ select", dim),
        ]),
        Line::from(vec![
            Span::styled("/model", key),
            Span::styled("  pick a model (or Ctrl-P)", dim),
        ]),
        Line::from(vec![
            Span::styled("/help", key),
            Span::styled("  all commands & keys", dim),
        ]),
    ];

    let height = lines.len() as u16;
    let top = inner.height.saturating_sub(height) / 2;
    let centered = Rect {
        x: inner.x,
        y: inner.y + top,
        width: inner.width,
        height: height.min(inner.height),
    };
    frame.render_widget(
        Paragraph::new(Text::from(lines)).alignment(Alignment::Center),
        centered,
    );
}

/// Colored span for a mode name (cyan genie, red sovereign).
fn mode_span(mode: Mode) -> Span<'static> {
    match mode {
        Mode::Genie => Span::styled("genie", Style::default().fg(Color::Cyan).bold()),
        Mode::Sovereign => Span::styled("sovereign", Style::default().fg(Color::Red).bold()),
    }
}

/// Build the full (unwrapped) transcript text from app state.
fn transcript_text(app: &App) -> Text<'static> {
    let mut lines: Vec<Line<'static>> = Vec::new();

    for entry in &app.transcript {
        match entry {
            TranscriptEntry::User(message) => {
                lines.push(Line::from(Span::styled(
                    "you ❯",
                    Style::default().fg(Color::Cyan).bold(),
                )));
                for line in message.lines() {
                    lines.push(Line::from(Span::raw(format!("  {line}"))));
                }
                lines.push(Line::raw(""));
            }
            TranscriptEntry::Assistant(message) => {
                lines.push(Line::from(Span::styled(
                    "wizard ✦",
                    Style::default().fg(Color::Magenta).bold(),
                )));
                lines.extend(render_markdown(message).lines);
                lines.push(Line::raw(""));
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
                );
            }
            TranscriptEntry::Notice(message) => {
                for line in message.lines() {
                    lines.push(Line::from(Span::styled(
                        format!("• {line}"),
                        Style::default().fg(Color::DarkGray).italic(),
                    )));
                }
            }
        }
    }

    if !app.streaming.is_empty() {
        lines.push(Line::from(Span::styled(
            "wizard ✦",
            Style::default().fg(Color::Magenta).bold(),
        )));
        lines.extend(render_markdown(&app.streaming).lines);
    } else if app.status.busy {
        let spinner = SPINNER[(app.tick as usize) % SPINNER.len()];
        lines.push(Line::from(Span::styled(
            format!("{spinner} thinking…"),
            Style::default().fg(Color::Yellow),
        )));
    }

    Text::from(lines)
}

/// Render one tool invocation card into `lines`.
fn tool_card_lines(
    lines: &mut Vec<Line<'static>>,
    name: &str,
    args: &serde_json::Value,
    output: Option<&str>,
    is_error: bool,
    collapsed: bool,
) {
    const MAX_ARG_LINES: usize = 8;
    const MAX_OUTPUT_LINES: usize = 200;

    let (icon, style) = match (output, is_error) {
        (None, _) => ('⚙', Style::default().fg(Color::Yellow)),
        (Some(_), false) => ('✔', Style::default().fg(Color::Green)),
        (Some(_), true) => ('✘', Style::default().fg(Color::Red)),
    };
    let marker = if collapsed { '▸' } else { '▾' };

    let summary = truncate_chars(&serde_json::to_string(args).unwrap_or_default(), 60);
    lines.push(Line::from(vec![
        Span::styled(format!("{marker} {icon} "), style),
        Span::styled(name.to_string(), style.bold()),
        Span::styled(format!("  {summary}"), Style::default().fg(Color::DarkGray)),
    ]));

    if !collapsed {
        let dim = Style::default().fg(Color::DarkGray);
        if !args.is_null()
            && let Ok(pretty) = serde_json::to_string_pretty(args)
        {
            let arg_lines: Vec<&str> = pretty.lines().collect();
            for line in arg_lines.iter().take(MAX_ARG_LINES) {
                lines.push(Line::from(Span::styled(format!("  │ {line}"), dim)));
            }
            if arg_lines.len() > MAX_ARG_LINES {
                lines.push(Line::from(Span::styled(
                    format!("  │ … (+{} lines)", arg_lines.len() - MAX_ARG_LINES),
                    dim,
                )));
            }
        }
        match output {
            None => {
                lines.push(Line::from(Span::styled(
                    "  │ running…",
                    Style::default().fg(Color::Yellow).italic(),
                )));
            }
            Some(text) => {
                let body_style = if is_error {
                    Style::default().fg(Color::Red)
                } else {
                    Style::default().fg(Color::Gray)
                };
                let out_lines: Vec<&str> = text.lines().collect();
                for line in out_lines.iter().take(MAX_OUTPUT_LINES) {
                    lines.push(Line::from(Span::styled(format!("  {line}"), body_style)));
                }
                if out_lines.len() > MAX_OUTPUT_LINES {
                    lines.push(Line::from(Span::styled(
                        format!("  … (+{} lines)", out_lines.len() - MAX_OUTPUT_LINES),
                        dim,
                    )));
                }
            }
        }
    }
    lines.push(Line::raw(""));
}

/// Git diff sidebar (`/diff`), syntax-highlighted.
fn draw_diff_sidebar(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(Line::from(vec![
            Span::styled(" ± ", Style::default().fg(Color::Yellow)),
            Span::styled("git diff ", Style::default().fg(Color::White).bold()),
        ]))
        .border_style(Style::default().fg(Color::DarkGray));
    let paragraph = Paragraph::new(highlight_diff(&app.diff_text)).block(block);
    frame.render_widget(paragraph, area);
}

/// Status bar: identity, mode badge, model, busy state with elapsed time,
/// and contextual key hints right-aligned.
fn draw_status_bar(frame: &mut Frame, app: &App, area: Rect) {
    let badge = match app.status.mode {
        Mode::Genie => Span::styled(
            " GENIE ",
            Style::default().fg(Color::Black).bg(Color::Cyan).bold(),
        ),
        Mode::Sovereign => Span::styled(
            " SOVEREIGN ",
            Style::default().fg(Color::White).bg(Color::Red).bold(),
        ),
    };
    let state = if app.status.busy {
        let spinner = SPINNER[(app.tick as usize) % SPINNER.len()];
        let elapsed = app
            .turn_started
            .map(|started| started.elapsed().as_secs())
            .unwrap_or(0);
        Span::styled(
            format!(
                "{spinner} step {}/{} · {}s",
                app.status.step, app.status.max_steps, elapsed
            ),
            Style::default().fg(Color::Yellow),
        )
    } else {
        Span::styled("● idle", Style::default().fg(Color::Green))
    };

    let separator = Span::styled(" │ ", Style::default().fg(Color::DarkGray));
    let line = Line::from(vec![
        Span::styled(" ✦ wizard ", Style::default().fg(ACCENT).bold()),
        badge,
        separator.clone(),
        Span::styled(app.status.model.clone(), Style::default().fg(Color::White)),
        separator,
        state,
    ]);
    let left_width = line.width() as u16;
    let bar_style = Style::default().bg(SURFACE);
    frame.render_widget(Paragraph::new(line).style(bar_style), area);

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
    let width = hints.chars().count() as u16 + 1;
    if area.width > left_width + width {
        let hint_area = Rect {
            x: area.right().saturating_sub(width),
            y: area.y,
            width,
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(Span::styled(hints, Style::default().fg(Color::DarkGray)))
                .style(bar_style),
            hint_area,
        );
    }
}

/// Input line with prompt symbol, cursor-aware horizontal scrolling, and
/// inline ghost-text completion of the highlighted command suggestion.
fn draw_input(frame: &mut Frame, app: &App, area: Rect) {
    let (title, border_color) = match app.input_mode {
        InputMode::Chat => (" message ", Color::DarkGray),
        InputMode::Command => (" command ", Color::Yellow),
        InputMode::Approval => (" approval pending — answer y/n ", ACCENT),
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(title)
        .border_style(Style::default().fg(border_color));

    let prompt = Span::styled("❯ ", Style::default().fg(ACCENT).bold());
    let prompt_width = 2usize;
    let inner_width = area.width.saturating_sub(2) as usize;
    let budget = inner_width.saturating_sub(prompt_width + 1).max(1);

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
    let cursor_x = area.x + 1 + prompt_width as u16 + cursor_cols as u16;

    let mut spans = vec![prompt, Span::raw(visible)];

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
                spans.push(Span::styled(
                    ghost,
                    Style::default().fg(Color::DarkGray).italic(),
                ));
            }
        }
    }

    frame.render_widget(Paragraph::new(Line::from(spans)).block(block), area);

    if app.input_mode != InputMode::Approval && app.picker.is_none() {
        frame.set_cursor_position(Position::new(cursor_x, area.y + 1));
    }
}

/// Command-suggestion popup floating above the input box (its bottom edge
/// sits above the status bar, full input width so it covers the transcript
/// border cleanly).
fn draw_suggestions(frame: &mut Frame, app: &App, input_area: Rect) {
    if app.suggestions.is_empty() {
        return;
    }

    let rows = app.suggestions.len() as u16;
    // The row directly above the input is the status bar; stack above it.
    let bottom = input_area.y.saturating_sub(1);
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

    // Window the rows so the ▸ selection stays visible on short terminals
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
            let (marker, name_style, row_style) = if selected {
                (
                    "▸ ",
                    Style::default().fg(Color::Cyan).bold().bg(SELECTION),
                    Style::default().bg(SELECTION),
                )
            } else {
                ("  ", Style::default().fg(Color::Cyan), Style::default())
            };
            let usage = format!("/{} {}", spec.name, spec.args);
            Line::from(vec![
                Span::styled(marker.to_string(), row_style.fg(Color::Cyan)),
                Span::styled(format!("{usage:<usage_width$}"), name_style),
                Span::styled(
                    format!("  {}", truncate_chars(spec.description, description_room)),
                    row_style.fg(Color::Gray),
                ),
            ])
            .style(row_style)
        })
        .collect();

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(" commands ")
        .border_style(Style::default().fg(Color::Yellow));
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
            let row_style = if selected {
                Style::default().bg(SELECTION)
            } else {
                Style::default()
            };
            let marker = if selected { "❯ " } else { "  " };
            let value_style = if item.current {
                row_style.fg(Color::Cyan).bold()
            } else {
                row_style.fg(Color::White)
            };
            // Ellipsize long model tags so the ● current marker stays visible.
            let suffix = if item.current {
                " ● current".chars().count()
            } else {
                0
            };
            let value_room = inner_width.saturating_sub(2 + suffix + 1);
            let mut spans = vec![
                Span::styled(marker.to_string(), row_style.fg(ACCENT)),
                Span::styled(truncate_chars(&item.value, value_room), value_style),
            ];
            if item.current {
                spans.push(Span::styled(" ● current", row_style.fg(Color::Green)));
            }
            if !item.detail.is_empty() {
                spans.push(Span::styled(
                    format!("  {}", item.detail),
                    row_style.fg(Color::Gray),
                ));
            }
            Line::from(spans).style(row_style)
        })
        .collect();

    let kind_icon = match picker.kind {
        PickerKind::Model => "⚛",
        PickerKind::Mode => "✦",
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(format!(" {kind_icon}{}", picker.title))
        .title_bottom(
            Line::from(Span::styled(
                " ↑↓ move · Enter select · Esc cancel ",
                Style::default().fg(Color::DarkGray),
            ))
            .centered(),
        )
        .border_style(Style::default().fg(ACCENT));
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

    let dim = Style::default().fg(Color::DarkGray);
    let mut lines: Vec<Line<'static>> = vec![
        Line::from(vec![
            Span::styled("tool: ", dim),
            Span::styled(
                pending.call.function.name.clone(),
                Style::default().fg(Color::Yellow).bold(),
            ),
        ]),
        Line::raw(""),
    ];

    for line in arg_lines.iter().take(shown) {
        lines.push(Line::from(Span::styled(
            line.to_string(),
            Style::default().fg(Color::Gray),
        )));
    }
    if truncated {
        lines.push(Line::from(Span::styled("…", dim)));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled("[y] approve", Style::default().fg(Color::Green).bold()),
        Span::raw("    "),
        Span::styled("[n] deny", Style::default().fg(Color::Red).bold()),
    ]));

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(" approve tool call? ")
        .border_style(Style::default().fg(ACCENT));
    frame.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
}

/// Wrap styled lines at `width` display columns (wide CJK/emoji glyphs
/// count as two) so the transcript can be pinned exactly to its bottom.
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
                if used + ch_width > width && used > 0 {
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

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let mut truncated: String = text.chars().take(max).collect();
        truncated.push('…');
        truncated
    }
}

// ---------------------------------------------------------------------------
// Diff highlighting (syntect)
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
                Style::default().fg(Color::White).bold()
            } else if line.starts_with('+') {
                Style::default().fg(Color::Green)
            } else if line.starts_with('-') {
                Style::default().fg(Color::Red)
            } else if line.starts_with("@@") {
                Style::default().fg(Color::Cyan)
            } else if line.starts_with("diff ") || line.starts_with("index ") {
                Style::default().fg(Color::Yellow).bold()
            } else {
                Style::default()
            };
            Line::from(Span::styled(line.to_string(), style))
        })
        .collect();
    Text::from(lines)
}

// ---------------------------------------------------------------------------
// Markdown rendering (pulldown-cmark)
// ---------------------------------------------------------------------------

/// Render markdown to styled terminal text (chat messages).
pub fn render_markdown(source: &str) -> Text<'static> {
    let mut renderer = MarkdownRenderer::default();
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
            return style.fg(Color::Green);
        }
        if self.heading {
            style = style.fg(Color::Cyan).add_modifier(Modifier::BOLD);
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
            style = style.fg(Color::Gray);
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
            self.current.push(Span::styled(
                "▌ ".repeat(self.quote_depth),
                Style::default().fg(Color::DarkGray),
            ));
        }
        if self.code_block {
            self.current.push(Span::raw("  "));
        }
    }

    fn push_text(&mut self, text: &str) {
        if self.code_block {
            // Code blocks carry embedded newlines.
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
                self.current.push(Span::styled(
                    code.to_string(),
                    Style::default().fg(Color::Yellow),
                ));
            }
            MdEvent::SoftBreak | MdEvent::HardBreak => {
                self.end_line();
                self.line_prefix();
            }
            MdEvent::Rule => {
                self.flush();
                self.lines.push(Line::from(Span::styled(
                    "─".repeat(24),
                    Style::default().fg(Color::DarkGray),
                )));
                self.blank_line();
            }
            MdEvent::TaskListMarker(checked) => {
                let marker = if checked { "[x] " } else { "[ ] " };
                self.current.push(Span::styled(
                    marker.to_string(),
                    Style::default().fg(Color::Cyan),
                ));
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
                if let CodeBlockKind::Fenced(lang) = kind
                    && !lang.is_empty()
                {
                    self.lines.push(Line::from(Span::styled(
                        format!("  ⌜{lang}⌟"),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
                self.code_block = true;
                self.line_prefix();
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
                self.current
                    .push(Span::styled(bullet, Style::default().fg(Color::Cyan)));
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
                self.flush();
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
                    self.current.push(Span::styled(
                        format!(" ({url})"),
                        Style::default().fg(Color::Blue).underlined(),
                    ));
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
