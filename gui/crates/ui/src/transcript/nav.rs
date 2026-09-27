//! Power user keyboard navigation over the transcript: a cursor that stops on
//! each message, tool card and code block, with scroll, expand and copy.

use super::*;

/// Code block rows copy their code instead of the whole message.
fn row_code(row: &Row) -> Option<&str> {
    let (RowKind::Markdown { tree, block_ix } | RowKind::LiveMarkdown { tree, block_ix }) =
        &row.kind
    else {
        return None;
    };
    match &tree.blocks.get(*block_ix)?.block {
        Block::CodeBlock { code, .. } => Some(code.as_str()),
        _ => None,
    }
}

/// Rows the cursor stops on: the first row of every message, every tool
/// card, and every code block. Pure.
pub(super) fn nav_stops(rows: &[Row]) -> Vec<usize> {
    rows.iter()
        .enumerate()
        .filter(|(_, row)| {
            row.turn_start
                || matches!(row.kind, RowKind::ToolGroup { .. })
                || row_code(row).is_some()
        })
        .map(|(ix, _)| ix)
        .collect()
}

/// The stop `delta` steps from the cursor. A cursor between stops counts as
/// sitting just after the stop before it. Pure.
pub(super) fn step_stop(stops: &[usize], cursor: Option<usize>, delta: isize) -> Option<usize> {
    if stops.is_empty() {
        return None;
    }
    let at = match cursor {
        Some(cursor) => match stops.binary_search(&cursor) {
            Ok(at) => at as isize,
            // Between stops: stepping down lands on the next one, up on the previous.
            Err(insert) if delta > 0 => insert as isize - 1,
            Err(insert) => insert as isize,
        },
        // No cursor yet: j starts at the top, k at the bottom.
        None if delta > 0 => -1,
        None => stops.len() as isize,
    };
    let next = (at + delta).clamp(0, stops.len() as isize - 1);
    Some(stops[next as usize])
}

/// Text `y` copies for the row at `ix`: a code block's code, otherwise the
/// owning message's copy text (the user prompt for user rows). Pure.
pub(super) fn yank_text(rows: &[Row], ix: usize) -> Option<(SharedString, SharedString)> {
    let row = rows.get(ix)?;
    if let Some(code) = row_code(row) {
        return Some((row.entry_id.clone(), code.to_string().into()));
    }
    if let RowKind::User { text, .. } = &row.kind {
        return Some((row.entry_id.clone(), text.clone()));
    }
    rows.iter()
        .filter(|r| r.entry_id == row.entry_id)
        .find_map(|r| r.copy_text.clone())
        .map(|text| (row.entry_id.clone(), text))
}

impl Transcript {
    fn nav_cursor_ix(&self) -> Option<usize> {
        let id = self.nav_cursor.as_ref()?;
        self.rows.iter().position(|row| &row.id == id)
    }

    fn set_nav_cursor(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(ix) else {
            return;
        };
        self.nav_cursor = Some(row.id.clone());
        let stops = nav_stops(&self.rows);
        if stops.last() == Some(&ix) && ix + 1 == self.rows.len() {
            // The last row lives under the composer's clearance; the pin
            // shows it whole.
            self.jump_to_bottom(cx);
        } else {
            self.nav_reveal(ix);
        }
        cx.notify();
    }

    /// Scroll just enough to show row `ix` clear of the titlebar above and
    /// the composer below (the list itself runs under both).
    fn nav_reveal(&mut self, ix: usize) {
        self.begin_scroll_navigation();
        let viewport = self.list.viewport_bounds();
        let height = f32::from(viewport.size.height);
        if height <= 0.0 {
            self.list.scroll_to_reveal_item(ix);
            return;
        }
        if self.is_glued() {
            // Same bottom-anchor materialization the rail's glide does.
            self.list.scroll_by(px(-(height + 0.5)));
        }
        let top_margin = Theme::TITLEBAR_HEIGHT + Theme::TRANSCRIPT_FADE_BAND;
        let bottom_margin = self.bottom_clearance + Theme::TRANSCRIPT_FADE_BAND;
        match self.list.bounds_for_item(ix) {
            Some(bounds) => {
                let top = f32::from(bounds.top() - viewport.top());
                let bottom = f32::from(bounds.bottom() - viewport.top());
                if top < top_margin {
                    self.list.scroll_by(px(top - top_margin));
                } else if bottom > height - bottom_margin {
                    // Never push the row's top under the titlebar to show its end.
                    let down = (bottom - (height - bottom_margin)).min(top - top_margin);
                    self.list.scroll_by(px(down.max(0.0)));
                }
            }
            None => {
                self.list.scroll_to(ListOffset {
                    item_ix: ix,
                    offset_in_item: px(0.0),
                });
                self.list.scroll_by(px(-top_margin));
            }
        }
    }

    /// The transcript gains or loses the navigation cursor. Gaining it with no
    /// cursor (or a stale one) starts at the first stop in view.
    pub(crate) fn set_nav_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if self.nav_active == active {
            return;
        }
        self.nav_active = active;
        if active && self.nav_cursor_ix().is_none() {
            let stops = nav_stops(&self.rows);
            let top = self.list.logical_scroll_top().item_ix;
            self.nav_cursor = stops
                .iter()
                .copied()
                .find(|&ix| ix >= top)
                .or(stops.last().copied())
                .and_then(|ix| self.rows.get(ix))
                .map(|row| row.id.clone());
        }
        cx.notify();
    }

    pub(crate) fn nav_step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let stops = nav_stops(&self.rows);
        if let Some(ix) = step_stop(&stops, self.nav_cursor_ix(), delta) {
            self.set_nav_cursor(ix, cx);
        }
    }

    pub(crate) fn nav_edge(&mut self, bottom: bool, cx: &mut Context<Self>) {
        let stops = nav_stops(&self.rows);
        let target = if bottom { stops.last() } else { stops.first() };
        if let Some(&ix) = target {
            self.nav_cursor = Some(self.rows[ix].id.clone());
        }
        if bottom {
            self.jump_to_bottom(cx);
        } else if !self.rows.is_empty() {
            self.scroll_to_row(0, cx);
        }
        cx.notify();
    }

    /// Ctrl-d / Ctrl-u: scroll half a viewport and carry the cursor to the
    /// first stop now in view.
    pub(crate) fn nav_half_page(&mut self, down: bool, cx: &mut Context<Self>) {
        let viewport = f32::from(self.list.viewport_bounds().size.height);
        if viewport <= 0.0 {
            return;
        }
        self.begin_scroll_navigation();
        if self.is_glued() {
            // Same bottom-anchor materialization the rail's glide does.
            self.list.scroll_by(px(-(viewport + 0.5)));
        }
        let half = viewport / 2.0;
        self.list.scroll_by(px(if down { half } else { -half }));
        if down && self.distance_from_bottom() <= 1.0 {
            self.jump_to_bottom(cx);
        }
        let top = self.list.logical_scroll_top().item_ix;
        let stops = nav_stops(&self.rows);
        if let Some(ix) = stops.iter().copied().find(|&ix| ix >= top) {
            self.nav_cursor = Some(self.rows[ix].id.clone());
        }
        cx.notify();
    }

    /// `o` / Enter: expand or collapse the tool card under the cursor.
    pub(crate) fn nav_toggle(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(ix) = self.nav_cursor_ix() else {
            return false;
        };
        let row = &self.rows[ix];
        let RowKind::ToolGroup { auto_open, .. } = row.kind else {
            return false;
        };
        let id = row.id.clone();
        self.toggle_fold(id.clone(), 0.0, auto_open, cx);
        // Opening a card grows the list below the cursor; keep the card itself
        // in view.
        if let Some(ix) = self.rows.iter().position(|row| row.id == id) {
            self.nav_reveal(ix);
        }
        cx.notify();
        true
    }

    /// `y`: copy the code block or message under the cursor. The copied
    /// message shows the same "Copied" check a click on its copy button does.
    pub(crate) fn nav_yank(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(ix) = self.nav_cursor_ix() else {
            return false;
        };
        let Some((entry_id, text)) = yank_text(&self.rows, ix) else {
            return false;
        };
        self.copy_message(entry_id, text, cx);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, entry: &str, turn_start: bool, kind: RowKind) -> Row {
        Row {
            id: id.to_string().into(),
            version: 0,
            turn_start,
            kind,
            entry_id: entry.to_string().into(),
            timestamp: None,
            copy_text: None,
            compact_fold: None,
        }
    }

    fn markdown(source: &str) -> RowKind {
        RowKind::Markdown {
            tree: Arc::new(parse_full(source)),
            block_ix: 0,
        }
    }

    fn user(text: &str) -> RowKind {
        RowKind::User {
            text: text.to_string().into(),
            mentions: Arc::default(),
            attachments: Arc::default(),
            badges: Arc::default(),
            pending: false,
        }
    }

    fn tools() -> RowKind {
        RowKind::ToolGroup {
            summary: "Ran 2 commands".into(),
            tools: Arc::default(),
            auto_open: false,
            worked_secs: None,
            compact_shell: false,
        }
    }

    fn sample() -> Vec<Row> {
        let mut reply_end = row("a1#2", "a1", false, markdown("done"));
        reply_end.copy_text = Some("whole reply".into());
        vec![
            row("u1", "u1", true, user("fix the tests")),
            row("a1#0", "a1", true, markdown("Looking now.")),
            row("a1#t", "a1", false, tools()),
            row("a1#1", "a1", false, markdown("```rust\nfn main() {}\n```")),
            reply_end,
        ]
    }

    #[test]
    fn stops_are_messages_tool_cards_and_code_blocks() {
        assert_eq!(nav_stops(&sample()), vec![0, 1, 2, 3]);
    }

    #[test]
    fn stepping_clamps_and_starts_from_the_right_end() {
        let stops = vec![0, 1, 2, 3];
        assert_eq!(step_stop(&stops, None, 1), Some(0));
        assert_eq!(step_stop(&stops, None, -1), Some(3));
        assert_eq!(step_stop(&stops, Some(1), 1), Some(2));
        assert_eq!(step_stop(&stops, Some(1), -1), Some(0));
        assert_eq!(step_stop(&stops, Some(3), 1), Some(3));
        assert_eq!(step_stop(&stops, Some(0), -1), Some(0));
        // Between stops (row 4 is not a stop).
        assert_eq!(step_stop(&[0, 2, 5], Some(4), 1), Some(5));
        assert_eq!(step_stop(&[0, 2, 5], Some(4), -1), Some(2));
        assert_eq!(step_stop(&[], Some(0), 1), None);
    }

    #[test]
    fn yank_copies_code_blocks_prompts_and_whole_replies() {
        let rows = sample();
        assert_eq!(
            yank_text(&rows, 3).map(|(_, t)| t.to_string()),
            Some("fn main() {}".into())
        );
        assert_eq!(
            yank_text(&rows, 0).map(|(_, t)| t.to_string()),
            Some("fix the tests".into())
        );
        assert_eq!(
            yank_text(&rows, 1).map(|(e, t)| (e.to_string(), t.to_string())),
            Some(("a1".into(), "whole reply".into()))
        );
        assert_eq!(yank_text(&rows, 9), None);
    }
}
