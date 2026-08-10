//! Modal (vim-style) editing for the input composer.
//!
//! The composer began as a single logical line and this module was written for
//! that. Alt+Enter now inserts a hard newline, so a draft can have several, and
//! the split is deliberate rather than accidental:
//!
//! - **Line-aware**: `0`, `^`, `$` and `D`/`C`, which have one obvious meaning
//!   per line. They used to measure the whole buffer, so `0` on line two jumped
//!   to the start of line one and `D` deleted every line below the caret.
//! - **Whole-draft**: `dd`/`S`, which take the entire composer. A composer is
//!   one thing you are about to send, so "the line" for a linewise operator is
//!   defensibly all of it — and unlike the motions above, taking it is
//!   recoverable with `u`.
//! - **Not bound to lines at all**: `j`/`k`, which recall input history, since
//!   history is what a composer's up and down have always meant here. `j`
//!   outside a history browse drops into the subagent rail, like plain ↓.
//!
//! Otherwise this is vim: `hjkl`/`w`/`b`/`e` motions, the `d`/`c`/`y`
//! operators with motions and counts, `x`/`r`/`p` and the `i`/`a`/`I`/`A`
//! insert transitions.
//!
//! [`VimState`] holds the mode and the small amount of pending state a modal
//! editor needs (an operator awaiting its motion, a count prefix, the yank
//! register, an undo stack). The key dispatch lives on
//! [`App`](crate::app::App) so it can reuse the existing line-editing
//! primitives; the pure motion math lives here, where it is unit-tested.

/// Which half of modal editing the composer is in. When vim mode is disabled
/// the composer behaves as if always in [`VimMode::Insert`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VimMode {
    /// Keys are motions and operators; printable characters do not insert.
    Normal,
    /// Ordinary text entry (the default, and the only behavior when vim mode
    /// is off).
    #[default]
    Insert,
}

/// A pending vim operator (`d`/`c`/`y`) — the half of a command that runs once
/// its motion arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VimOp {
    Delete,
    Change,
    Yank,
}

/// A multi-key command waiting for its next key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pending {
    /// An operator (`d`/`c`/`y`) awaiting a motion, or a repeat of the same
    /// key for the linewise form (`dd`/`cc`/`yy`).
    Operator(VimOp),
    /// `r` typed: the next printable key replaces the character under the
    /// cursor.
    Replace,
}

/// All modal-editing state for the composer.
#[derive(Debug, Clone, Default)]
pub struct VimState {
    /// Whether modal editing is active at all (mirrors `[ui] vim` in config;
    /// toggled live by `/vim`). When false the composer is always insert-like.
    pub enabled: bool,
    pub mode: VimMode,
    /// A multi-key command in progress (operator awaiting a motion, or `r`).
    pub pending: Option<Pending>,
    /// Numeric count prefix being typed (`3w`, `d2w`), if any.
    pub count: Option<usize>,
    /// The count that preceded a pending operator, held while its motion's own
    /// count is typed.
    ///
    /// Vim multiplies the two — `2d3w` deletes six words — and with one field
    /// the digits simply concatenated: `2` then `3` made 23, so `2d3w` cleared
    /// a 23-word line. Two fields, multiplied at motion time.
    pub operator_count: Option<usize>,
    /// The yank/delete register, pasted by `p`/`P`.
    pub register: String,
    /// Undo snapshots `(input, cursor)` captured before each Normal-mode edit;
    /// `u` pops the most recent.
    pub undo: Vec<(String, usize)>,
}

/// Cap on the undo stack so a long session cannot grow it without bound.
const UNDO_LIMIT: usize = 200;

impl VimState {
    /// True when keys should be read as motions/operators (vim on and in
    /// Normal mode).
    pub fn is_normal(&self) -> bool {
        self.enabled && self.mode == VimMode::Normal
    }

    /// The status-bar mode tag, or `None` when modal editing is off.
    pub fn label(&self) -> Option<&'static str> {
        if !self.enabled {
            return None;
        }
        Some(match self.mode {
            VimMode::Normal => "NORMAL",
            VimMode::Insert => "INSERT",
        })
    }

    /// Clear the in-progress multi-key command (operator/count/replace).
    pub fn clear_pending(&mut self) {
        self.pending = None;
        self.count = None;
        self.operator_count = None;
    }

    /// Push an undo snapshot, trimming the oldest entry past [`UNDO_LIMIT`].
    pub fn push_undo(&mut self, input: &str, cursor: usize) {
        self.undo.push((input.to_string(), cursor));
        if self.undo.len() > UNDO_LIMIT {
            self.undo.remove(0);
        }
    }
}

/// Character class for word motions. Vim treats a run of "word" characters
/// (alphanumeric + `_`), a run of punctuation, and whitespace as distinct, so
/// `w` stops at a word↔punctuation boundary even without intervening space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Blank,
    Word,
    Punct,
}

fn classify(c: char) -> Class {
    if c.is_whitespace() {
        Class::Blank
    } else if c.is_alphanumeric() || c == '_' {
        Class::Word
    } else {
        Class::Punct
    }
}

/// `w`: start of the next word. Skips the rest of the current word/punct run,
/// then any whitespace, landing on the next non-blank character (or the end).
pub fn word_forward(chars: &[char], cursor: usize) -> usize {
    let len = chars.len();
    let mut i = cursor;
    if i >= len {
        return len;
    }
    let start = classify(chars[i]);
    if start != Class::Blank {
        while i < len && classify(chars[i]) == start {
            i += 1;
        }
    }
    while i < len && classify(chars[i]) == Class::Blank {
        i += 1;
    }
    i
}

/// `b`: start of the current word, or of the previous word when already at a
/// boundary.
pub fn word_back(chars: &[char], cursor: usize) -> usize {
    if cursor == 0 {
        return 0;
    }
    let mut i = cursor - 1;
    while i > 0 && classify(chars[i]) == Class::Blank {
        i -= 1;
    }
    if classify(chars[i]) == Class::Blank {
        return 0;
    }
    let class = classify(chars[i]);
    while i > 0 && classify(chars[i - 1]) == class {
        i -= 1;
    }
    i
}

/// `e`: end of the next word (the last character of that word).
pub fn word_end(chars: &[char], cursor: usize) -> usize {
    let len = chars.len();
    if len == 0 {
        return 0;
    }
    let mut i = cursor;
    // `e` always advances at least one cell before looking for a word end.
    if i < len {
        i += 1;
    }
    while i < len && classify(chars[i]) == Class::Blank {
        i += 1;
    }
    if i >= len {
        return len - 1;
    }
    let class = classify(chars[i]);
    while i + 1 < len && classify(chars[i + 1]) == class {
        i += 1;
    }
    i
}

/// `^`: index of the first non-blank character (or 0 for an all-blank/empty
/// line).
pub fn first_non_blank(chars: &[char]) -> usize {
    chars.iter().position(|c| !c.is_whitespace()).unwrap_or(0)
}

/// The bounds of the line `cursor` sits on: `start..end`, where `end` is the
/// newline (or the buffer end).
///
/// The composer used to be a single logical line, and this module's header
/// said so. Alt+Enter made that untrue, and the line motions kept measuring the
/// whole buffer: `0` on line two jumped to the start of line *one*, `$` on line
/// one jumped to the end of the last line, and `D` deleted every line below the
/// caret. Those three have an unambiguous per-line meaning, so they use this.
///
/// `j`/`k` still browse history rather than moving between lines, and `dd`/`S`
/// still take the whole draft; both are called out in the module header.
pub fn line_bounds(chars: &[char], cursor: usize) -> (usize, usize) {
    let at = cursor.min(chars.len());
    let start = chars[..at]
        .iter()
        .rposition(|c| *c == '\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let end = chars[at..]
        .iter()
        .position(|c| *c == '\n')
        .map(|i| at + i)
        .unwrap_or(chars.len());
    (start, end)
}

/// Where a motion key lands the cursor, and the character range an operator
/// applied with that motion would cover. `start..end` is always normalized
/// (`start <= end`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Motion {
    /// Cursor destination for a bare motion.
    pub target: usize,
    /// Inclusive-start, exclusive-end character range for an operator.
    pub start: usize,
    pub end: usize,
}

/// Resolve a motion key into a [`Motion`] over `chars` from `cursor`, applying
/// the count `n`. Returns `None` for keys that are not motions.
pub fn resolve_motion(key: char, n: usize, chars: &[char], cursor: usize) -> Option<Motion> {
    let len = chars.len();
    let n = n.max(1);
    let range = |a: usize, b: usize| (a.min(b), a.max(b));
    let (target, (start, end)) = match key {
        'h' => {
            let t = cursor.saturating_sub(n);
            (t, range(t, cursor))
        }
        'l' | ' ' => {
            let t = (cursor + n).min(len);
            (t, range(cursor, t))
        }
        '0' => {
            let (start, _) = line_bounds(chars, cursor);
            (start, range(start, cursor))
        }
        '^' => {
            let (start, end) = line_bounds(chars, cursor);
            let t = start + first_non_blank(&chars[start..end]);
            (t, range(t, cursor))
        }
        '$' => {
            let (_, end) = line_bounds(chars, cursor);
            (end, range(cursor, end))
        }
        'w' => {
            let mut t = cursor;
            for _ in 0..n {
                t = word_forward(chars, t);
            }
            (t, range(cursor, t))
        }
        'b' => {
            let mut t = cursor;
            for _ in 0..n {
                t = word_back(chars, t);
            }
            (t, range(t, cursor))
        }
        'e' => {
            let mut t = cursor;
            for _ in 0..n {
                t = word_end(chars, t);
            }
            // `e` is inclusive: an operator eats through the word-end cell.
            (t, range(cursor, (t + 1).min(len)))
        }
        _ => return None,
    };
    Some(Motion { target, start, end })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cv(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    #[test]
    fn word_forward_across_classes() {
        let c = cv("foo bar.baz");
        assert_eq!(word_forward(&c, 0), 4); // foo -> bar
        assert_eq!(word_forward(&c, 4), 7); // bar -> .
        assert_eq!(word_forward(&c, 7), 8); // . -> baz
        assert_eq!(word_forward(&c, 8), c.len()); // baz -> end
    }

    #[test]
    fn word_back_across_classes() {
        let c = cv("foo bar.baz");
        assert_eq!(word_back(&c, c.len()), 8); // from end -> baz
        assert_eq!(word_back(&c, 8), 7); // baz -> .
        assert_eq!(word_back(&c, 7), 4); // . -> bar
        assert_eq!(word_back(&c, 4), 0); // bar -> foo
        assert_eq!(word_back(&c, 0), 0);
    }

    #[test]
    fn word_end_lands_on_last_char() {
        let c = cv("foo bar");
        assert_eq!(word_end(&c, 0), 2); // end of foo
        assert_eq!(word_end(&c, 2), 6); // end of bar
        assert_eq!(word_end(&c, 6), 6); // already at last
    }

    #[test]
    fn first_non_blank_skips_leading_space() {
        assert_eq!(first_non_blank(&cv("   hi")), 3);
        assert_eq!(first_non_blank(&cv("hi")), 0);
        assert_eq!(first_non_blank(&cv("   ")), 0);
        assert_eq!(first_non_blank(&cv("")), 0);
    }

    #[test]
    fn dollar_and_zero_motions() {
        let c = cv("hello");
        let m = resolve_motion('$', 1, &c, 0).unwrap();
        assert_eq!((m.start, m.end), (0, 5));
        let m = resolve_motion('0', 1, &c, 3).unwrap();
        assert_eq!((m.start, m.end), (0, 3));
    }

    #[test]
    fn counted_word_motion() {
        let c = cv("a b c d");
        let m = resolve_motion('w', 2, &c, 0).unwrap();
        assert_eq!(m.target, 4); // a -> c
    }

    #[test]
    fn e_motion_range_is_inclusive() {
        let c = cv("foo bar");
        // `de` from 0 should cover "foo" (chars 0..3).
        let m = resolve_motion('e', 1, &c, 0).unwrap();
        assert_eq!((m.start, m.end), (0, 3));
    }

    #[test]
    fn non_motion_key_is_none() {
        let c = cv("foo");
        assert!(resolve_motion('z', 1, &c, 0).is_none());
    }

    #[test]
    fn h_and_l_clamp_at_the_line_edges() {
        let c = cv("abc");
        let m = resolve_motion('l', 99, &c, 1).unwrap();
        assert_eq!(m.target, 3, "l stops at the end of the line");
        assert_eq!((m.start, m.end), (1, 3));
        let m = resolve_motion('h', 99, &c, 2).unwrap();
        assert_eq!(m.target, 0, "h stops at the start of the line");
        assert_eq!((m.start, m.end), (0, 2));
    }

    #[test]
    fn word_forward_from_a_blank_lands_on_the_next_word() {
        let c = cv("a   bc");
        assert_eq!(word_forward(&c, 1), 4);
        assert_eq!(word_forward(&c, c.len()), c.len());
    }

    #[test]
    fn undo_stack_stays_bounded_dropping_the_oldest() {
        let mut vim = VimState::default();
        for i in 0..UNDO_LIMIT + 5 {
            vim.push_undo(&format!("snap {i}"), i);
        }
        assert_eq!(vim.undo.len(), UNDO_LIMIT);
        assert_eq!(vim.undo[0].0, "snap 5", "the oldest snapshots fell off");
        assert_eq!(vim.undo.last().unwrap().1, UNDO_LIMIT + 4);
    }
}
