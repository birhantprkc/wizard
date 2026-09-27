//! Modal (vim) editing for the message composer.
//!
//! [`Vim`] is a pure state machine over the composer's text and caret. The
//! caller feeds it one [`Key`] at a time together with the buffer and a byte
//! cursor, and writes the result back into the input. Typing in Insert mode
//! stays with the caller (IME, completion menus and paste keep working); this
//! only takes over in Normal and Visual modes. There is no gpui in here, so
//! every behavior is unit-tested without a window.

use std::ops::Range;

/// Undo snapshots kept per composer.
const UNDO_LIMIT: usize = 100;
/// Upper bound on any count, so `99999999x` cannot stall the UI thread.
const COUNT_LIMIT: usize = 10_000;
/// Upper bound on text produced by one paste or insert repeat.
const REPEAT_BYTES: usize = 256 * 1024;
/// Spaces per `>` / `<` step, matching the composer's list indent.
const SHIFT: &str = "  ";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Insert,
    Normal,
    Visual,
    VisualLine,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Insert => "INSERT",
            Mode::Normal => "NORMAL",
            Mode::Visual => "VISUAL",
            Mode::VisualLine => "V-LINE",
        }
    }
}

/// One keystroke, shaped like gpui's: `key` is the key name ("a", "escape",
/// "enter", "left", "space", ...) and `ch` the character it types, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    pub key: String,
    pub ch: Option<char>,
    pub ctrl: bool,
    pub alt: bool,
    pub cmd: bool,
}

impl Key {
    pub fn char(c: char) -> Self {
        let key = if c == ' ' {
            "space".to_string()
        } else {
            c.to_lowercase().collect()
        };
        Self {
            key,
            ch: Some(c),
            ctrl: false,
            alt: false,
            cmd: false,
        }
    }

    pub fn named(name: &str) -> Self {
        Self {
            key: name.to_string(),
            ch: (name == "space").then_some(' '),
            ctrl: false,
            alt: false,
            cmd: false,
        }
    }

    pub fn ctrl(name: &str) -> Self {
        Self {
            key: name.to_string(),
            ch: None,
            ctrl: true,
            alt: false,
            cmd: false,
        }
    }
}

/// Parse a test-friendly key sequence: plain characters plus `<esc>`, `<cr>`,
/// `<bs>`, `<del>`, `<tab>`, `<space>`, `<left>`/`<right>`/`<up>`/`<down>`,
/// `<lt>` for a literal `<`, and `<c-x>` for ctrl chords. An unknown `<...>`
/// is read as a literal `<`.
pub fn keys(seq: &str) -> Vec<Key> {
    let mut out = Vec::new();
    let mut rest = seq;
    while let Some(c) = rest.chars().next() {
        if c == '<'
            && let Some(end) = rest.find('>')
            && let Some(key) = named_key(&rest[1..end])
        {
            out.push(key);
            rest = &rest[end + 1..];
            continue;
        }
        out.push(Key::char(c));
        rest = &rest[c.len_utf8()..];
    }
    out
}

fn named_key(name: &str) -> Option<Key> {
    let lower = name.to_ascii_lowercase();
    Some(match lower.as_str() {
        "esc" | "escape" => Key::named("escape"),
        "cr" | "enter" | "return" => Key::named("enter"),
        "bs" | "backspace" => Key::named("backspace"),
        "del" | "delete" => Key::named("delete"),
        "tab" => Key::named("tab"),
        "space" => Key::named("space"),
        "left" | "right" | "up" | "down" => Key::named(&lower),
        "lt" => Key::char('<'),
        _ => {
            let rest = lower.strip_prefix("c-")?;
            if rest.chars().count() != 1 {
                return None;
            }
            Key::ctrl(rest)
        }
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Consumed; the caller writes back text, cursor and selection.
    Handled,
    /// Not ours: the input handles it as usual.
    PassThrough,
    /// Enter in Normal mode with nothing pending: send the message.
    Submit,
    /// Esc in Normal mode with nothing pending: the caller may leave the composer.
    Escape,
}

/// A key reduced to what the state machine cares about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tok {
    Esc,
    Enter,
    Bs,
    Del,
    Left,
    Right,
    Up,
    Down,
    Ch(char),
    CtrlR,
    /// Never ours in any mode (cmd chords, ctrl chords, alt chords).
    Pass,
    /// Named keys we have no meaning for; swallowed outside Insert.
    Unknown,
}

fn tok(key: &Key) -> Tok {
    if key.cmd {
        return Tok::Pass;
    }
    if key.ctrl {
        return match key.key.as_str() {
            "r" => Tok::CtrlR,
            "[" => Tok::Esc,
            _ => Tok::Pass,
        };
    }
    match key.key.as_str() {
        "escape" => return Tok::Esc,
        "enter" => return Tok::Enter,
        "backspace" => return Tok::Bs,
        "delete" => return Tok::Del,
        "left" => return Tok::Left,
        "right" => return Tok::Right,
        "up" => return Tok::Up,
        "down" => return Tok::Down,
        "space" => return Tok::Ch(' '),
        "tab" => return Tok::Unknown,
        _ => {}
    }
    if key.alt {
        return Tok::Pass;
    }
    if let Some(c) = key.ch {
        return Tok::Ch(c);
    }
    let mut chars = key.key.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => Tok::Ch(c),
        _ => Tok::Unknown,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Delete,
    Change,
    Yank,
    Indent,
    Outdent,
}

impl Op {
    fn from_char(c: char) -> Option<Self> {
        Some(match c {
            'd' => Op::Delete,
            'c' => Op::Change,
            'y' => Op::Yank,
            '>' => Op::Indent,
            '<' => Op::Outdent,
            _ => return None,
        })
    }

    fn char(self) -> char {
        match self {
            Op::Delete => 'd',
            Op::Change => 'c',
            Op::Yank => 'y',
            Op::Indent => '>',
            Op::Outdent => '<',
        }
    }
}

/// A command waiting for one more key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Pend {
    #[default]
    None,
    /// `g` typed (for `gg`).
    G,
    /// `f`/`t`/`F`/`T` typed; the next char is the target.
    Find(char),
    /// `r` typed; the next char replaces.
    Replace,
    /// `i` or `a` typed after an operator or in Visual; `true` is inner.
    Obj(bool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Exclusive,
    Inclusive,
    Linewise,
}

#[derive(Debug, Clone, Copy)]
struct Target {
    pos: usize,
    kind: Kind,
}

enum Motion {
    /// Not a motion key.
    No,
    /// A prefix; wait for the next key.
    Wait,
    /// A motion that cannot move (search miss, top of buffer).
    Fail,
    Go(Target),
}

/// A repeatable change: its keys without counts, the count it ran with, and
/// the text typed if it entered Insert mode.
#[derive(Debug, Clone, Default)]
struct Change {
    keys: Vec<Key>,
    count: Option<usize>,
    insert: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Vim {
    mode: Mode,
    count: Option<usize>,
    op_count: Option<usize>,
    op: Option<Op>,
    pend: Pend,
    register: String,
    register_linewise: bool,
    undo: Vec<(String, usize)>,
    redo: Vec<(String, usize)>,
    last_find: Option<(char, char)>,
    want_col: Option<usize>,
    /// Char index of the fixed end of a Visual selection.
    anchor: usize,
    last_change: Option<Change>,
    cmd_keys: Vec<Key>,
    used_count: Option<usize>,
    pending_change: Option<Change>,
    /// Set by `u`, `<c-r>` and `.` so they are not recorded as the last change.
    no_record: bool,
    /// Set by `u` and `<c-r>` so they do not push an undo step.
    no_undo: bool,
    count_key: bool,
    replaying: bool,
    /// Snapshot before the command that entered Insert, pushed on Esc.
    insert_undo: Option<(String, usize)>,
    /// Text when the insert session began, diffed on Esc for `.`.
    insert_snap: Option<String>,
    insert_repeat: usize,
}

impl Vim {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn register(&self) -> &str {
        &self.register
    }

    pub fn set_mode(&mut self, mode: Mode, text: &str, cursor: &mut usize) {
        self.clear_pending();
        let b = Buf::new(text);
        match mode {
            Mode::Insert => {
                if self.mode != Mode::Insert {
                    self.insert_snap = None;
                    self.insert_undo = None;
                    self.pending_change = None;
                    self.insert_repeat = 0;
                }
                *cursor = b.byte(b.idx(*cursor));
            }
            Mode::Normal | Mode::Visual | Mode::VisualLine => {
                let ci = clamp_normal(&b, b.idx(*cursor));
                *cursor = b.byte(ci);
                self.anchor = ci;
            }
        }
        self.mode = mode;
    }

    /// Pending command text for the mode indicator, empty when idle.
    pub fn pending(&self) -> String {
        let mut out = String::new();
        if let Some(count) = self.count {
            out.push_str(&count.to_string());
        }
        if let Some(op) = self.op {
            out.push(op.char());
        }
        if let Some(count) = self.op_count {
            out.push_str(&count.to_string());
        }
        match self.pend {
            Pend::None => {}
            Pend::G => out.push('g'),
            Pend::Find(k) => out.push(k),
            Pend::Replace => out.push('r'),
            Pend::Obj(inner) => out.push(if inner { 'i' } else { 'a' }),
        }
        out
    }

    /// Visual selection as a byte range: the char under the cursor is
    /// included, and whole lines (with their newline) in Visual-line mode.
    pub fn selection(&self, text: &str, cursor: usize) -> Option<Range<usize>> {
        let b = Buf::new(text);
        let ci = b.idx(cursor);
        let anchor = self.anchor.min(b.len());
        let (lo, hi) = (ci.min(anchor), ci.max(anchor));
        match self.mode {
            Mode::Visual => Some(b.byte(lo)..b.byte((hi + 1).min(b.len()))),
            Mode::VisualLine => {
                let end = b.line_end(hi);
                let end = if end < b.len() { end + 1 } else { end };
                Some(b.byte(b.line_start(lo))..b.byte(end))
            }
            _ => None,
        }
    }

    /// Clamp a cursor after the text changed outside the state machine.
    pub fn clamp(&self, text: &str, cursor: usize) -> usize {
        let b = Buf::new(text);
        let ci = b.idx(cursor);
        match self.mode {
            Mode::Insert => b.byte(ci),
            _ => b.byte(clamp_normal(&b, ci)),
        }
    }

    pub fn handle(&mut self, key: &Key, text: &mut String, cursor: &mut usize) -> Outcome {
        let t = tok(key);
        if t == Tok::Pass {
            return Outcome::PassThrough;
        }
        if self.mode == Mode::Insert {
            return self.insert_key(t, text, cursor);
        }
        let before = (text.clone(), *cursor);
        let recording = self.mode == Mode::Normal && !self.replaying;
        if recording && self.idle() {
            self.cmd_keys.clear();
            self.used_count = None;
        }
        self.no_record = false;
        self.no_undo = false;
        self.count_key = false;
        let prev_mode = self.mode;
        let b = Buf::new(text);
        let mut ci = clamp_normal(&b, b.idx(*cursor));
        let out = if self.mode == Mode::Normal {
            self.normal(t, text, &mut ci)
        } else {
            self.visual(t, text, &mut ci)
        };
        if recording && !self.count_key {
            self.cmd_keys.push(key.clone());
        }
        let b = Buf::new(text);
        if self.mode == Mode::Insert {
            *cursor = b.byte(ci.min(b.len()));
            if prev_mode != Mode::Insert {
                if !self.replaying {
                    self.insert_undo = Some(before);
                }
                self.insert_snap = Some(text.clone());
                if recording {
                    self.pending_change = Some(Change {
                        keys: std::mem::take(&mut self.cmd_keys),
                        count: self.used_count,
                        insert: None,
                    });
                }
            }
        } else {
            *cursor = b.byte(clamp_normal(&b, ci));
            if matches!(self.mode, Mode::Visual | Mode::VisualLine) {
                self.anchor = self.anchor.min(b.len());
            }
            let changed = *text != before.0;
            if changed && !self.replaying && !self.no_undo {
                self.push_undo(before);
            }
            if recording && self.idle() {
                if changed && !self.no_record {
                    self.last_change = Some(Change {
                        keys: std::mem::take(&mut self.cmd_keys),
                        count: self.used_count,
                        insert: None,
                    });
                }
                self.cmd_keys.clear();
            }
        }
        out
    }

    // ---- insert mode ------------------------------------------------------

    fn insert_key(&mut self, t: Tok, text: &mut String, cursor: &mut usize) -> Outcome {
        if self.insert_snap.is_none() {
            // A session the caller started (new composer, set_mode): the text
            // before the first key is the baseline for undo and `.`.
            self.insert_snap = Some(text.clone());
            if !self.replaying && self.insert_undo.is_none() {
                self.insert_undo = Some((text.clone(), *cursor));
            }
        }
        if t == Tok::Esc {
            self.finish_insert(text, cursor);
            return Outcome::Handled;
        }
        if !self.replaying {
            return Outcome::PassThrough;
        }
        // Replaying `.`: type the recorded text ourselves.
        let b = Buf::new(text);
        let ci = b.idx(*cursor);
        match t {
            Tok::Ch(c) => {
                text.insert(b.byte(ci), c);
                *cursor = b.byte(ci) + c.len_utf8();
            }
            Tok::Enter => {
                text.insert(b.byte(ci), '\n');
                *cursor = b.byte(ci) + 1;
            }
            Tok::Bs if ci > 0 => {
                text.replace_range(b.byte(ci - 1)..b.byte(ci), "");
                *cursor = b.byte(ci - 1);
            }
            _ => {}
        }
        Outcome::Handled
    }

    fn finish_insert(&mut self, text: &mut String, cursor: &mut usize) {
        let snap = self.insert_snap.take().unwrap_or_default();
        let inserted = inserted_text(&snap, text);
        let b = Buf::new(text);
        let mut byte = b.byte(b.idx(*cursor));
        if self.insert_repeat > 1 && !inserted.is_empty() {
            let times = (self.insert_repeat - 1)
                .min(REPEAT_BYTES / inserted.len())
                .max(1);
            let extra = inserted.repeat(times);
            text.insert_str(byte, &extra);
            byte += extra.len();
        }
        self.insert_repeat = 0;
        if let Some(mut change) = self.pending_change.take()
            && !self.replaying
        {
            change.insert = Some(inserted);
            self.last_change = Some(change);
        }
        if let Some(undo) = self.insert_undo.take()
            && !self.replaying
            && undo.0 != *text
        {
            self.push_undo(undo);
        }
        self.mode = Mode::Normal;
        self.want_col = None;
        let b = Buf::new(text);
        let mut ci = b.idx(byte);
        if ci > b.line_start(ci) {
            ci -= 1;
        }
        *cursor = b.byte(clamp_normal(&b, ci));
    }

    // ---- normal mode ------------------------------------------------------

    fn idle(&self) -> bool {
        self.count.is_none()
            && self.op_count.is_none()
            && self.op.is_none()
            && self.pend == Pend::None
    }

    fn clear_pending(&mut self) {
        self.count = None;
        self.op_count = None;
        self.op = None;
        self.pend = Pend::None;
    }

    fn peek_count(&self) -> Option<usize> {
        match (self.count, self.op_count) {
            (None, None) => None,
            (a, b) => Some(
                a.unwrap_or(1)
                    .saturating_mul(b.unwrap_or(1))
                    .clamp(1, COUNT_LIMIT),
            ),
        }
    }

    fn take_count(&mut self) -> Option<usize> {
        let count = self.peek_count();
        self.count = None;
        self.op_count = None;
        self.used_count = count;
        count
    }

    /// Digits typed as a count. Returns true if the key was one.
    fn count_digit(&mut self, t: Tok) -> bool {
        let Tok::Ch(c @ '0'..='9') = t else {
            return false;
        };
        if self.pend != Pend::None {
            return false;
        }
        let slot = if self.op.is_some() {
            &mut self.op_count
        } else {
            &mut self.count
        };
        if c == '0' && slot.is_none() {
            return false;
        }
        let digit = c as usize - '0' as usize;
        *slot = Some(
            slot.unwrap_or(0)
                .saturating_mul(10)
                .saturating_add(digit)
                .min(99_999),
        );
        self.count_key = true;
        true
    }

    fn normal(&mut self, t: Tok, text: &mut String, ci: &mut usize) -> Outcome {
        if self.count_digit(t) {
            return Outcome::Handled;
        }
        match t {
            Tok::Esc => {
                if self.idle() {
                    return Outcome::Escape;
                }
                self.clear_pending();
                return Outcome::Handled;
            }
            Tok::Enter if self.idle() => return Outcome::Submit,
            _ => {}
        }
        if self.pend == Pend::Replace {
            self.pend = Pend::None;
            let n = self.take_count().unwrap_or(1);
            if let Tok::Ch(c) = t {
                self.replace_chars(text, ci, c, n);
            }
            self.clear_pending();
            return Outcome::Handled;
        }
        if let Pend::Obj(inner) = self.pend {
            self.pend = Pend::None;
            let op = self.op.take();
            let n = self.take_count().unwrap_or(1);
            if let (Some(op), Tok::Ch(c)) = (op, t) {
                let b = Buf::new(text);
                if let Some((s, e)) = text_object(&b, *ci, inner, c, n) {
                    self.apply_range(op, text, ci, s, e);
                }
            }
            self.clear_pending();
            return Outcome::Handled;
        }
        if let Some(op) = self.op {
            if self.pend == Pend::None {
                if t == Tok::Ch(op.char()) {
                    let n = self.take_count().unwrap_or(1);
                    self.op = None;
                    let b = Buf::new(text);
                    let first = b.line_of(*ci);
                    let last = (first + n - 1).min(b.last_line());
                    self.lines_op(op, text, ci, first, last);
                    return Outcome::Handled;
                }
                if let Tok::Ch(c @ ('i' | 'a')) = t {
                    self.pend = Pend::Obj(c == 'i');
                    return Outcome::Handled;
                }
            }
            let b = Buf::new(text);
            let count = self.peek_count();
            match self.motion(t, &b, *ci, count, Some(op)) {
                Motion::Wait => return Outcome::Handled,
                Motion::Go(target) => {
                    self.take_count();
                    self.op = None;
                    self.apply_motion(op, text, ci, target);
                }
                Motion::Fail | Motion::No => {}
            }
            self.clear_pending();
            return Outcome::Handled;
        }
        let b = Buf::new(text);
        let count = self.peek_count();
        match self.motion(t, &b, *ci, count, None) {
            Motion::Wait => return Outcome::Handled,
            Motion::Go(target) => {
                self.take_count();
                *ci = target.pos;
                return Outcome::Handled;
            }
            Motion::Fail => {
                self.clear_pending();
                return Outcome::Handled;
            }
            Motion::No => {}
        }
        if let Tok::Ch(c) = t {
            if let Some(op) = Op::from_char(c) {
                self.op = Some(op);
                return Outcome::Handled;
            }
            if c == 'r' {
                self.pend = Pend::Replace;
                return Outcome::Handled;
            }
        }
        let count = self.take_count();
        let n = count.unwrap_or(1);
        self.want_col = None;
        let b = Buf::new(text);
        let cur = *ci;
        let ls = b.line_start(cur);
        let le = b.line_end(cur);
        match t {
            Tok::Ch('i') => self.enter_insert(ci, cur, n),
            Tok::Ch('a') => self.enter_insert(ci, if cur < le { cur + 1 } else { cur }, n),
            Tok::Ch('I') => self.enter_insert(ci, b.first_non_blank(ls), n),
            Tok::Ch('A') => self.enter_insert(ci, le, n),
            Tok::Ch('o') => {
                splice(text, le, le, "\n");
                self.enter_insert(ci, le + 1, 1);
            }
            Tok::Ch('O') => {
                splice(text, ls, ls, "\n");
                self.enter_insert(ci, ls, 1);
            }
            Tok::Ch('x') | Tok::Del => {
                let e = (cur + n).min(le);
                if e > cur {
                    self.char_op(Op::Delete, text, ci, cur, e);
                }
            }
            Tok::Ch('X') => {
                let s = cur.saturating_sub(n).max(ls);
                if s < cur {
                    self.char_op(Op::Delete, text, ci, s, cur);
                }
            }
            Tok::Ch('D') | Tok::Ch('C') => {
                let line = (b.line_of(cur) + n - 1).min(b.last_line());
                let end = b.line_end(b.start_of_line(line));
                let op = if t == Tok::Ch('D') {
                    Op::Delete
                } else {
                    Op::Change
                };
                self.char_op(op, text, ci, cur, end);
            }
            Tok::Ch('s') => {
                let e = (cur + n).min(le);
                self.char_op(Op::Change, text, ci, cur, e);
            }
            Tok::Ch('S') => {
                let first = b.line_of(cur);
                let last = (first + n - 1).min(b.last_line());
                self.lines_op(Op::Change, text, ci, first, last);
            }
            Tok::Ch('Y') => {
                let first = b.line_of(cur);
                let last = (first + n - 1).min(b.last_line());
                self.lines_op(Op::Yank, text, ci, first, last);
            }
            Tok::Ch('J') => self.join(text, ci, n.max(2) - 1),
            Tok::Ch('p') => self.paste(text, ci, true, n),
            Tok::Ch('P') => self.paste(text, ci, false, n),
            Tok::Ch('~') => {
                let e = (cur + n).min(le);
                let flipped: String = b.slice(cur, e).chars().map(toggle_case).collect();
                splice(text, cur, e, &flipped);
                *ci = e;
            }
            Tok::Ch('u') => {
                self.no_record = true;
                self.no_undo = true;
                self.undo_redo(text, ci, n, true);
            }
            Tok::CtrlR => {
                self.no_record = true;
                self.no_undo = true;
                self.undo_redo(text, ci, n, false);
            }
            Tok::Ch('.') => self.repeat(text, ci, count),
            Tok::Ch('v') => {
                self.mode = Mode::Visual;
                self.anchor = cur;
            }
            Tok::Ch('V') => {
                self.mode = Mode::VisualLine;
                self.anchor = cur;
            }
            _ => {}
        }
        Outcome::Handled
    }

    fn enter_insert(&mut self, ci: &mut usize, at: usize, repeat: usize) {
        self.mode = Mode::Insert;
        self.insert_repeat = repeat;
        *ci = at;
    }

    // ---- visual mode ------------------------------------------------------

    fn visual(&mut self, t: Tok, text: &mut String, ci: &mut usize) -> Outcome {
        if self.count_digit(t) {
            return Outcome::Handled;
        }
        if t == Tok::Esc {
            if self.idle() {
                self.mode = Mode::Normal;
            }
            self.clear_pending();
            return Outcome::Handled;
        }
        let b = Buf::new(text);
        if let Pend::Obj(inner) = self.pend {
            self.pend = Pend::None;
            let n = self.take_count().unwrap_or(1);
            if let Tok::Ch(c) = t
                && let Some((s, e)) = text_object(&b, *ci, inner, c, n)
                && e > s
            {
                self.anchor = s;
                *ci = e - 1;
            }
            return Outcome::Handled;
        }
        let count = self.peek_count();
        match self.motion(t, &b, *ci, count, None) {
            Motion::Wait => return Outcome::Handled,
            Motion::Go(target) => {
                self.take_count();
                *ci = target.pos;
                return Outcome::Handled;
            }
            Motion::Fail => {
                self.clear_pending();
                return Outcome::Handled;
            }
            Motion::No => {}
        }
        let n = self.take_count().unwrap_or(1);
        let linewise = self.mode == Mode::VisualLine;
        let anchor = self.anchor.min(b.len());
        let (lo, hi) = (anchor.min(*ci), anchor.max(*ci));
        let (s, e) = (lo, (hi + 1).min(b.len()));
        let (first, last) = (b.line_of(lo), b.line_of(hi));
        let Tok::Ch(c) = t else {
            return Outcome::Handled;
        };
        match c {
            'v' | 'V' => {
                let target = if c == 'v' {
                    Mode::Visual
                } else {
                    Mode::VisualLine
                };
                self.mode = if self.mode == target {
                    Mode::Normal
                } else {
                    target
                };
            }
            'o' => {
                self.anchor = *ci;
                *ci = anchor;
            }
            'i' | 'a' => self.pend = Pend::Obj(c == 'i'),
            'd' | 'x' | 'X' | 'D' | 'y' | 'Y' => {
                let op = if matches!(c, 'y' | 'Y') {
                    Op::Yank
                } else {
                    Op::Delete
                };
                self.mode = Mode::Normal;
                if linewise || matches!(c, 'X' | 'D' | 'Y') {
                    self.lines_op(op, text, ci, first, last);
                    if op == Op::Yank {
                        *ci = b.start_of_line(first).max(lo.min(*ci));
                    }
                } else {
                    self.char_op(op, text, ci, s, e);
                }
            }
            'c' | 's' | 'C' | 'S' | 'R' => {
                self.mode = Mode::Normal;
                if linewise || matches!(c, 'C' | 'S' | 'R') {
                    self.lines_op(Op::Change, text, ci, first, last);
                } else {
                    self.char_op(Op::Change, text, ci, s, e);
                }
            }
            '>' | '<' => {
                self.mode = Mode::Normal;
                shift_lines(text, first, last, c == '>', n);
                let b = Buf::new(text);
                *ci = b.first_non_blank(b.start_of_line(first));
            }
            '~' | 'u' | 'U' => {
                self.mode = Mode::Normal;
                let (s, e) = if linewise {
                    (b.start_of_line(first), b.line_end(b.start_of_line(last)))
                } else {
                    (s, e)
                };
                let mapped: String = b
                    .slice(s, e)
                    .chars()
                    .map(|ch| match c {
                        '~' => toggle_case(ch),
                        'u' => ch.to_lowercase().next().unwrap_or(ch),
                        _ => ch.to_uppercase().next().unwrap_or(ch),
                    })
                    .collect();
                splice(text, s, e, &mapped);
                *ci = s;
            }
            'J' => {
                self.mode = Mode::Normal;
                *ci = b.start_of_line(first);
                self.join(text, ci, (last - first).max(1));
            }
            _ => {}
        }
        Outcome::Handled
    }

    // ---- motions ----------------------------------------------------------

    fn motion(
        &mut self,
        t: Tok,
        b: &Buf,
        cur: usize,
        count: Option<usize>,
        op: Option<Op>,
    ) -> Motion {
        let n = count.unwrap_or(1);
        match self.pend {
            Pend::G => {
                self.pend = Pend::None;
                return match t {
                    Tok::Ch('g') => {
                        self.want_col = None;
                        let line = count.map_or(0, |c| c - 1).min(b.last_line());
                        go(b.first_non_blank(b.start_of_line(line)), Kind::Linewise)
                    }
                    _ => Motion::Fail,
                };
            }
            Pend::Find(k) => {
                self.pend = Pend::None;
                return match t {
                    Tok::Ch(c) => {
                        self.want_col = None;
                        self.last_find = Some((k, c));
                        find(b, cur, k, c, n, false)
                    }
                    _ => Motion::Fail,
                };
            }
            _ => {}
        }
        let ls = b.line_start(cur);
        let le = b.line_end(cur);
        let motion = match t {
            Tok::Ch('h') | Tok::Left => go(cur.saturating_sub(n).max(ls), Kind::Exclusive),
            Tok::Bs => go(cur.saturating_sub(n), Kind::Exclusive),
            Tok::Ch('l') | Tok::Right => {
                let mut pos = (cur + n).min(le);
                if op.is_none() && pos == le && le > ls {
                    pos = le - 1;
                }
                go(pos, Kind::Exclusive)
            }
            Tok::Ch(' ') => go((cur + n).min(b.len()), Kind::Exclusive),
            Tok::Ch('j') | Tok::Down => return self.vertical(b, cur, n as isize, op.is_some()),
            Tok::Ch('k') | Tok::Up => return self.vertical(b, cur, -(n as isize), op.is_some()),
            Tok::Ch(c @ ('w' | 'W')) => word_motion(b, cur, n, c == 'W', op),
            Tok::Ch(c @ ('b' | 'B')) => {
                let mut pos = cur;
                for _ in 0..n {
                    pos = word_back(b, pos, c == 'B');
                }
                go(pos, Kind::Exclusive)
            }
            Tok::Ch(c @ ('e' | 'E')) => {
                let mut pos = cur;
                for _ in 0..n {
                    pos = word_end(b, pos, c == 'E');
                }
                go(pos, Kind::Inclusive)
            }
            Tok::Ch('0') => go(ls, Kind::Exclusive),
            Tok::Ch('^') => go(b.first_non_blank(ls), Kind::Exclusive),
            Tok::Ch('$') => {
                let line = (b.line_of(cur) + n - 1).min(b.last_line());
                let end = b.line_end(b.start_of_line(line));
                self.want_col = Some(usize::MAX);
                return go(end, Kind::Exclusive);
            }
            Tok::Ch('G') => {
                let line = count.map_or(b.last_line(), |c| c - 1).min(b.last_line());
                go(b.first_non_blank(b.start_of_line(line)), Kind::Linewise)
            }
            Tok::Ch('g') => {
                self.pend = Pend::G;
                return Motion::Wait;
            }
            Tok::Ch(c @ ('f' | 't' | 'F' | 'T')) => {
                self.pend = Pend::Find(c);
                return Motion::Wait;
            }
            Tok::Ch(c @ (';' | ',')) => match self.last_find {
                Some((k, ch)) => {
                    let k = if c == ',' { reverse_find(k) } else { k };
                    find(b, cur, k, ch, n, true)
                }
                None => Motion::Fail,
            },
            _ => Motion::No,
        };
        if matches!(motion, Motion::Go(_)) {
            self.want_col = None;
        }
        motion
    }

    fn vertical(&mut self, b: &Buf, cur: usize, delta: isize, for_op: bool) -> Motion {
        let line = b.line_of(cur);
        let target = (line as isize + delta).clamp(0, b.last_line() as isize) as usize;
        if target == line && for_op {
            return Motion::Fail;
        }
        let want = *self.want_col.get_or_insert(cur - b.line_start(cur));
        let ls = b.start_of_line(target);
        let le = b.line_end(ls);
        let pos = if le > ls {
            ls + want.min(le - ls - 1)
        } else {
            ls
        };
        go(pos, Kind::Linewise)
    }

    // ---- operators --------------------------------------------------------

    fn apply_motion(&mut self, op: Op, text: &mut String, ci: &mut usize, target: Target) {
        let b = Buf::new(text);
        let cur = *ci;
        if target.kind == Kind::Linewise {
            let (a, z) = (b.line_of(cur), b.line_of(target.pos));
            self.lines_op(op, text, ci, a.min(z), a.max(z));
            return;
        }
        let s = cur.min(target.pos);
        let mut e = cur.max(target.pos);
        if target.kind == Kind::Inclusive {
            e = (e + 1).min(b.len());
        }
        self.apply_range(op, text, ci, s, e);
    }

    fn apply_range(&mut self, op: Op, text: &mut String, ci: &mut usize, s: usize, e: usize) {
        if matches!(op, Op::Indent | Op::Outdent) {
            let b = Buf::new(text);
            let first = b.line_of(s);
            let last = b.line_of(e.saturating_sub(1).max(s));
            self.lines_op(op, text, ci, first, last);
        } else {
            self.char_op(op, text, ci, s, e);
        }
    }

    fn char_op(&mut self, op: Op, text: &mut String, ci: &mut usize, s: usize, e: usize) {
        let b = Buf::new(text);
        let s = s.min(b.len());
        let e = e.clamp(s, b.len());
        match op {
            Op::Delete | Op::Change => {
                if e > s {
                    self.set_register(b.slice(s, e), false);
                    splice(text, s, e, "");
                }
                *ci = s;
                if op == Op::Change {
                    self.enter_insert(ci, s, 1);
                }
            }
            Op::Yank => {
                self.set_register(b.slice(s, e), false);
                *ci = s;
            }
            Op::Indent | Op::Outdent => {
                self.apply_range(op, text, ci, s, e);
            }
        }
    }

    fn lines_op(&mut self, op: Op, text: &mut String, ci: &mut usize, first: usize, last: usize) {
        let b = Buf::new(text);
        let ls = b.start_of_line(first);
        let le = b.line_end(b.start_of_line(last));
        match op {
            Op::Yank => {
                self.set_register(format!("{}\n", b.slice(ls, le)), true);
                if b.line_of(*ci) > first {
                    let col = *ci - b.line_start(*ci);
                    let fle = b.line_end(ls);
                    *ci = if fle > ls {
                        ls + col.min(fle - ls - 1)
                    } else {
                        ls
                    };
                }
            }
            Op::Delete => {
                let n = b.len();
                let (s, e, reg) = if le < n {
                    (ls, le + 1, b.slice(ls, le + 1))
                } else if ls > 0 {
                    (ls - 1, n, format!("{}\n", b.slice(ls, n)))
                } else {
                    (0, n, format!("{}\n", b.slice(0, n)))
                };
                self.set_register(reg, true);
                splice(text, s, e, "");
                let b = Buf::new(text);
                let start = b.line_start(s.min(b.len()));
                *ci = b.first_non_blank(start);
            }
            Op::Change => {
                self.set_register(format!("{}\n", b.slice(ls, le)), true);
                // Keep the first line's indent, as with 'autoindent'.
                let keep = b.first_non_blank(ls).min(le);
                splice(text, keep, le, "");
                self.enter_insert(ci, keep, 1);
            }
            Op::Indent | Op::Outdent => {
                shift_lines(text, first, last, op == Op::Indent, 1);
                let b = Buf::new(text);
                *ci = b.first_non_blank(b.start_of_line(first));
            }
        }
    }

    fn set_register(&mut self, text: String, linewise: bool) {
        self.register = text;
        self.register_linewise = linewise;
    }

    fn replace_chars(&mut self, text: &mut String, ci: &mut usize, c: char, n: usize) {
        let b = Buf::new(text);
        let le = b.line_end(*ci);
        if *ci + n > le {
            return;
        }
        let with: String = std::iter::repeat_n(c, n).collect();
        splice(text, *ci, *ci + n, &with);
        *ci += n - 1;
    }

    fn join(&mut self, text: &mut String, ci: &mut usize, times: usize) {
        for _ in 0..times.min(COUNT_LIMIT) {
            let b = Buf::new(text);
            let ls = b.line_start(*ci);
            let le = b.line_end(*ci);
            if le >= b.len() {
                break;
            }
            let mut e = le + 1;
            while e < b.len() && is_blank(b.c[e]) {
                e += 1;
            }
            let next_empty = e >= b.len() || b.c[e] == '\n';
            let ends_blank = le > ls && is_blank(b.c[le - 1]);
            let sep = if le == ls || ends_blank || next_empty || b.c[e] == ')' {
                ""
            } else {
                " "
            };
            splice(text, le, e, sep);
            *ci = le;
        }
    }

    fn paste(&mut self, text: &mut String, ci: &mut usize, after: bool, n: usize) {
        if self.register.is_empty() {
            return;
        }
        let n = n.min(REPEAT_BYTES / self.register.len()).max(1);
        let reg = self.register.repeat(n);
        let b = Buf::new(text);
        let cur = *ci;
        if self.register_linewise {
            if after {
                let le = b.line_end(cur);
                if le < b.len() {
                    splice(text, le + 1, le + 1, &reg);
                } else {
                    let body = reg.strip_suffix('\n').unwrap_or(&reg);
                    splice(text, le, le, &format!("\n{body}"));
                }
                let b = Buf::new(text);
                *ci = b.first_non_blank((le + 1).min(b.len()));
            } else {
                let ls = b.line_start(cur);
                splice(text, ls, ls, &reg);
                let b = Buf::new(text);
                *ci = b.first_non_blank(ls);
            }
        } else {
            let at = if after && cur < b.line_end(cur) {
                cur + 1
            } else {
                cur
            };
            splice(text, at, at, &reg);
            *ci = at + reg.chars().count() - 1;
        }
    }

    fn push_undo(&mut self, snapshot: (String, usize)) {
        self.undo.push(snapshot);
        if self.undo.len() > UNDO_LIMIT {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    fn undo_redo(&mut self, text: &mut String, ci: &mut usize, n: usize, undo: bool) {
        for _ in 0..n.min(UNDO_LIMIT) {
            let popped = if undo {
                self.undo.pop()
            } else {
                self.redo.pop()
            };
            let Some((old, _)) = popped else {
                break;
            };
            let b = Buf::new(text);
            let current = (std::mem::replace(text, old), b.byte(*ci));
            // Like vim, land at the start of what changed.
            let changed_at = common_prefix(&current.0, text);
            if undo {
                self.redo.push(current);
            } else {
                self.undo.push(current);
            }
            let b = Buf::new(text);
            *ci = b.idx(changed_at);
        }
    }

    fn repeat(&mut self, text: &mut String, ci: &mut usize, count: Option<usize>) {
        let Some(change) = self.last_change.clone() else {
            return;
        };
        let mut seq = Vec::new();
        if let Some(n) = count.or(change.count) {
            seq.extend(n.to_string().chars().map(Key::char));
        }
        seq.extend(change.keys.iter().cloned());
        if let Some(inserted) = &change.insert {
            for c in inserted.chars() {
                seq.push(if c == '\n' {
                    Key::named("enter")
                } else {
                    Key::char(c)
                });
            }
            seq.push(Key::named("escape"));
        }
        let was = self.replaying;
        self.replaying = true;
        let b = Buf::new(text);
        let mut cursor = b.byte(*ci);
        for key in &seq {
            self.handle(key, text, &mut cursor);
        }
        if self.mode == Mode::Insert {
            self.handle(&Key::named("escape"), text, &mut cursor);
        }
        self.clear_pending();
        self.replaying = was;
        self.no_record = true;
        self.no_undo = false;
        self.count_key = false;
        let b = Buf::new(text);
        *ci = b.idx(cursor);
    }
}

fn go(pos: usize, kind: Kind) -> Motion {
    Motion::Go(Target { pos, kind })
}

fn reverse_find(k: char) -> char {
    match k {
        'f' => 'F',
        'F' => 'f',
        't' => 'T',
        _ => 't',
    }
}

fn find(b: &Buf, cur: usize, k: char, c: char, n: usize, repeat: bool) -> Motion {
    let ls = b.line_start(cur);
    let le = b.line_end(cur);
    let mut left = n;
    match k {
        'f' | 't' => {
            let mut i = if k == 't' && repeat { cur + 1 } else { cur };
            loop {
                i += 1;
                if i >= le {
                    return Motion::Fail;
                }
                if b.c[i] == c {
                    left -= 1;
                    if left == 0 {
                        break;
                    }
                }
            }
            go(if k == 't' { i - 1 } else { i }, Kind::Inclusive)
        }
        _ => {
            let mut i = if k == 'T' && repeat {
                cur.saturating_sub(1)
            } else {
                cur
            };
            loop {
                if i <= ls {
                    return Motion::Fail;
                }
                i -= 1;
                if b.c[i] == c {
                    left -= 1;
                    if left == 0 {
                        break;
                    }
                }
            }
            go(if k == 'T' { i + 1 } else { i }, Kind::Exclusive)
        }
    }
}

// ---- buffer ---------------------------------------------------------------

/// The text as chars with their byte offsets; motions work in char indices.
struct Buf {
    c: Vec<char>,
    /// Byte offset of each char, plus the text length at the end.
    o: Vec<usize>,
}

impl Buf {
    fn new(text: &str) -> Self {
        let mut c = Vec::with_capacity(text.len());
        let mut o = Vec::with_capacity(text.len() + 1);
        for (i, ch) in text.char_indices() {
            c.push(ch);
            o.push(i);
        }
        o.push(text.len());
        Self { c, o }
    }

    fn len(&self) -> usize {
        self.c.len()
    }

    /// Char index for a byte offset (rounded down to a char start).
    fn idx(&self, byte: usize) -> usize {
        match self.o.binary_search(&byte) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        }
    }

    fn byte(&self, i: usize) -> usize {
        self.o[i.min(self.len())]
    }

    fn slice(&self, s: usize, e: usize) -> String {
        let e = e.min(self.len());
        self.c[s.min(e)..e].iter().collect()
    }

    fn line_start(&self, i: usize) -> usize {
        let mut j = i.min(self.len());
        while j > 0 && self.c[j - 1] != '\n' {
            j -= 1;
        }
        j
    }

    fn line_end(&self, i: usize) -> usize {
        let mut j = i.min(self.len());
        while j < self.len() && self.c[j] != '\n' {
            j += 1;
        }
        j
    }

    fn line_of(&self, i: usize) -> usize {
        self.c[..i.min(self.len())]
            .iter()
            .filter(|&&c| c == '\n')
            .count()
    }

    fn last_line(&self) -> usize {
        self.line_of(self.len())
    }

    fn start_of_line(&self, line: usize) -> usize {
        if line == 0 {
            return 0;
        }
        let mut seen = 0;
        for (i, &c) in self.c.iter().enumerate() {
            if c == '\n' {
                seen += 1;
                if seen == line {
                    return i + 1;
                }
            }
        }
        self.line_start(self.len())
    }

    fn first_non_blank(&self, ls: usize) -> usize {
        let mut j = ls.min(self.len());
        while j < self.len() && is_blank(self.c[j]) {
            j += 1;
        }
        j
    }
}

/// Normal-mode cursor: on a char, never on a line's newline unless the line is empty.
fn clamp_normal(b: &Buf, i: usize) -> usize {
    let i = i.min(b.len());
    let ls = b.line_start(i);
    let le = b.line_end(i);
    if le > ls { i.min(le - 1) } else { ls }
}

fn splice(text: &mut String, s: usize, e: usize, with: &str) {
    let b = Buf::new(text);
    let (s, e) = (b.byte(s), b.byte(e));
    text.replace_range(s..e.max(s), with);
}

fn is_blank(c: char) -> bool {
    c == ' ' || c == '\t'
}

/// 0 blank, 1 punctuation, 2 word, 3 newline.
fn class(c: char, big: bool) -> u8 {
    if c == '\n' {
        3
    } else if c.is_whitespace() {
        0
    } else if big || c.is_alphanumeric() || c == '_' {
        2
    } else {
        1
    }
}

fn toggle_case(c: char) -> char {
    if c.is_lowercase() {
        c.to_uppercase().next().unwrap_or(c)
    } else if c.is_uppercase() {
        c.to_lowercase().next().unwrap_or(c)
    } else {
        c
    }
}

/// Start of the next word; an empty line counts as a word.
fn word_fwd(b: &Buf, mut i: usize, big: bool) -> usize {
    let n = b.len();
    if i >= n {
        return n;
    }
    let start = i;
    let k = class(b.c[i], big);
    if k == 1 || k == 2 {
        while i < n && class(b.c[i], big) == k {
            i += 1;
        }
    }
    loop {
        if i >= n {
            return n;
        }
        let c = b.c[i];
        if c == '\n' {
            if i != start && (i == 0 || b.c[i - 1] == '\n') {
                return i;
            }
            i += 1;
        } else if c.is_whitespace() {
            i += 1;
        } else {
            return i;
        }
    }
}

fn word_back(b: &Buf, i: usize, big: bool) -> usize {
    if i == 0 {
        return 0;
    }
    let mut i = i.min(b.len()) - 1;
    loop {
        let c = b.c[i];
        if c == '\n' {
            if i == 0 || b.c[i - 1] == '\n' {
                return i;
            }
            i -= 1;
        } else if c.is_whitespace() {
            if i == 0 {
                return 0;
            }
            i -= 1;
        } else {
            break;
        }
    }
    let k = class(b.c[i], big);
    while i > 0 && class(b.c[i - 1], big) == k {
        i -= 1;
    }
    i
}

fn word_end(b: &Buf, i: usize, big: bool) -> usize {
    let n = b.len();
    if n == 0 {
        return 0;
    }
    let mut i = i + 1;
    while i < n && matches!(class(b.c[i], big), 0 | 3) {
        i += 1;
    }
    if i >= n {
        return n - 1;
    }
    let k = class(b.c[i], big);
    while i + 1 < n && class(b.c[i + 1], big) == k {
        i += 1;
    }
    i
}

/// Last char of the word under `i` (for `cw`, which does not jump ahead).
fn end_of_word_here(b: &Buf, mut i: usize, big: bool) -> usize {
    let k = class(b.c[i], big);
    while i + 1 < b.len() && class(b.c[i + 1], big) == k {
        i += 1;
    }
    i
}

fn word_motion(b: &Buf, cur: usize, n: usize, big: bool, op: Option<Op>) -> Motion {
    if op == Some(Op::Change) && cur < b.len() && !b.c[cur].is_whitespace() {
        // `cw` changes to the end of the word, like `ce`, without the blank after it.
        let mut pos = end_of_word_here(b, cur, big);
        for _ in 1..n {
            pos = word_end(b, pos, big);
        }
        return go(pos, Kind::Inclusive);
    }
    let mut pos = cur;
    for i in 0..n {
        let mut next = word_fwd(b, pos, big);
        let le = b.line_end(pos);
        // With an operator, the last word moved over stops at its line end.
        if op.is_some() && i == n - 1 && next > le && le > pos {
            next = le;
        }
        pos = next;
    }
    go(pos, Kind::Exclusive)
}

// ---- text objects ---------------------------------------------------------

fn text_object(b: &Buf, cur: usize, inner: bool, c: char, count: usize) -> Option<(usize, usize)> {
    match c {
        'w' => word_object(b, cur, inner, false),
        'W' => word_object(b, cur, inner, true),
        '"' | '\'' | '`' => quote_object(b, cur, inner, c),
        '(' | ')' | 'b' => bracket_object(b, cur, inner, '(', ')', count),
        '[' | ']' => bracket_object(b, cur, inner, '[', ']', count),
        '{' | '}' | 'B' => bracket_object(b, cur, inner, '{', '}', count),
        '<' | '>' => bracket_object(b, cur, inner, '<', '>', count),
        _ => None,
    }
}

fn word_object(b: &Buf, cur: usize, inner: bool, big: bool) -> Option<(usize, usize)> {
    if cur >= b.len() || b.c[cur] == '\n' {
        return None;
    }
    let ls = b.line_start(cur);
    let le = b.line_end(cur);
    let k = class(b.c[cur], big);
    let mut s = cur;
    while s > ls && class(b.c[s - 1], big) == k {
        s -= 1;
    }
    let mut e = cur + 1;
    while e < le && class(b.c[e], big) == k {
        e += 1;
    }
    if inner {
        return Some((s, e));
    }
    if k == 0 {
        if e < le {
            let k2 = class(b.c[e], big);
            while e < le && class(b.c[e], big) == k2 {
                e += 1;
            }
        }
        return Some((s, e));
    }
    let mut trail = e;
    while trail < le && is_blank(b.c[trail]) {
        trail += 1;
    }
    if trail > e {
        return Some((s, trail));
    }
    let mut lead = s;
    while lead > ls && is_blank(b.c[lead - 1]) {
        lead -= 1;
    }
    Some((lead, e))
}

fn quote_object(b: &Buf, cur: usize, inner: bool, q: char) -> Option<(usize, usize)> {
    let ls = b.line_start(cur);
    let le = b.line_end(cur);
    let quotes: Vec<usize> = (ls..le)
        .filter(|&i| b.c[i] == q && (i == ls || b.c[i - 1] != '\\'))
        .collect();
    let pairs: Vec<(usize, usize)> = quotes.chunks_exact(2).map(|p| (p[0], p[1])).collect();
    let (open, close) = pairs
        .iter()
        .copied()
        .find(|&(o, c)| o <= cur && cur <= c)
        .or_else(|| pairs.iter().copied().find(|&(o, _)| o > cur))?;
    if inner {
        return Some((open + 1, close));
    }
    let mut e = close + 1;
    while e < le && is_blank(b.c[e]) {
        e += 1;
    }
    if e > close + 1 {
        return Some((open, e));
    }
    let mut s = open;
    while s > ls && is_blank(b.c[s - 1]) {
        s -= 1;
    }
    Some((s, close + 1))
}

fn bracket_object(
    b: &Buf,
    cur: usize,
    inner: bool,
    open: char,
    close: char,
    count: usize,
) -> Option<(usize, usize)> {
    if b.len() == 0 {
        return None;
    }
    let cur = cur.min(b.len() - 1);
    let find_open = |from: usize| -> Option<usize> {
        let mut depth = 0usize;
        let mut i = from;
        loop {
            let c = b.c[i];
            if c == close {
                depth += 1;
            } else if c == open {
                if depth == 0 {
                    return Some(i);
                }
                depth -= 1;
            }
            if i == 0 {
                return None;
            }
            i -= 1;
        }
    };
    let mut o = if b.c[cur] == open {
        cur
    } else if b.c[cur] == close {
        if cur == 0 {
            return None;
        }
        find_open(cur - 1)?
    } else {
        find_open(cur)?
    };
    for _ in 1..count.min(COUNT_LIMIT) {
        if o == 0 {
            return None;
        }
        o = find_open(o - 1)?;
    }
    let mut depth = 0usize;
    let mut i = o + 1;
    let c = loop {
        if i >= b.len() {
            return None;
        }
        let ch = b.c[i];
        if ch == open {
            depth += 1;
        } else if ch == close {
            if depth == 0 {
                break i;
            }
            depth -= 1;
        }
        i += 1;
    };
    if inner {
        Some((o + 1, c))
    } else {
        Some((o, c + 1))
    }
}

fn shift_lines(text: &mut String, first: usize, last: usize, right: bool, times: usize) {
    let times = times.clamp(1, 100);
    let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
    let last = last.min(lines.len().saturating_sub(1));
    for line in lines.iter_mut().take(last + 1).skip(first) {
        for _ in 0..times {
            if right {
                if !line.is_empty() {
                    line.insert_str(0, SHIFT);
                }
            } else if line.starts_with('\t') {
                line.remove(0);
            } else {
                let spaces = line
                    .chars()
                    .take(SHIFT.len())
                    .take_while(|&c| c == ' ')
                    .count();
                line.replace_range(0..spaces, "");
            }
        }
    }
    *text = lines.join("\n");
}

/// Byte length of the common prefix of `a` and `b`, on a char boundary.
fn common_prefix(a: &str, b: &str) -> usize {
    a.chars()
        .zip(b.chars())
        .take_while(|(x, y)| x == y)
        .map(|(x, _)| x.len_utf8())
        .sum()
}

/// What was typed in an insert session: `new` minus the common prefix and
/// suffix it shares with `old`.
fn inserted_text(old: &str, new: &str) -> String {
    let prefix = common_prefix(old, new);
    let room = old.len().min(new.len()) - prefix;
    let mut suffix = 0;
    for (a, b) in old[prefix..].chars().rev().zip(new[prefix..].chars().rev()) {
        if a != b || suffix + a.len_utf8() > room {
            break;
        }
        suffix += a.len_utf8();
    }
    new[prefix..new.len() - suffix].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `seq` from Normal mode on `before` (cursor marked with `|`). Insert
    /// typing is simulated the way the composer does it: PassThrough keys in
    /// Insert mode go into the text.
    fn run_on(vim: &mut Vim, before: &str, seq: &str) -> (String, Vec<Outcome>) {
        let cursor = before.find('|').expect("cursor marker");
        let mut text = before.replacen('|', "", 1);
        let mut c = cursor;
        vim.set_mode(Mode::Normal, &text, &mut c);
        let mut outs = Vec::new();
        for key in keys(seq) {
            let out = vim.handle(&key, &mut text, &mut c);
            if out == Outcome::PassThrough && vim.mode() == Mode::Insert {
                match key.key.as_str() {
                    "enter" => {
                        text.insert(c, '\n');
                        c += 1;
                    }
                    "backspace" => {
                        if let Some(prev) = text[..c].chars().next_back() {
                            c -= prev.len_utf8();
                            text.remove(c);
                        }
                    }
                    _ => {
                        if let Some(ch) = key.ch {
                            text.insert(c, ch);
                            c += ch.len_utf8();
                        }
                    }
                }
            }
            assert!(text.is_char_boundary(c), "cursor off a boundary");
            outs.push(out);
        }
        let mut shown = text.clone();
        shown.insert(c, '|');
        (shown, outs)
    }

    fn run(before: &str, seq: &str) -> (String, Mode) {
        let mut vim = Vim::new();
        let (shown, _) = run_on(&mut vim, before, seq);
        (shown, vim.mode())
    }

    use Mode::{Insert as I, Normal as N, Visual as V, VisualLine as VL};

    const CASES: &[(&str, &str, &str, Mode)] = &[
        // motions
        ("|hello world", "w", "hello |world", N),
        ("hello |world", "b", "|hello world", N),
        ("|hello world", "e", "hell|o world", N),
        ("|foo.bar baz", "w", "foo|.bar baz", N),
        ("|foo.bar baz", "W", "foo.bar |baz", N),
        ("foo.bar |baz", "B", "|foo.bar baz", N),
        ("|foo.bar baz", "E", "foo.ba|r baz", N),
        ("|a b c d", "3w", "a b c |d", N),
        ("|ab\n\ncd", "w", "ab\n|\ncd", N),
        ("ab\n\n|cd", "b", "ab\n|\ncd", N),
        ("|ab\ncd", "w", "ab\n|cd", N),
        ("  he|llo", "0", "|  hello", N),
        ("  he|llo", "^", "  |hello", N),
        ("|hello", "$", "hell|o", N),
        ("|hello", "3l", "hel|lo", N),
        ("hel|lo", "2h", "h|ello", N),
        ("|hello", "10l", "hell|o", N),
        ("|hello", "<right><space>", "he|llo", N),
        ("he|llo", "<left>", "h|ello", N),
        ("ab\n|cd", "<bs>", "a|b\ncd", N),
        ("|ab\ncd\nef", "j", "ab\n|cd\nef", N),
        ("|ab\ncd\nef", "2j", "ab\ncd\n|ef", N),
        ("|ab\ncd\nef", "<down>", "ab\n|cd\nef", N),
        ("ab\ncd\ne|f", "k", "ab\nc|d\nef", N),
        ("abcdef|g\nab\nabcdefgh", "jj", "abcdefg\nab\nabcdef|gh", N),
        ("|a\nb\nc", "G", "a\nb\n|c", N),
        ("a\nb\n|c", "gg", "|a\nb\nc", N),
        ("|a\nb\nc", "2G", "a\n|b\nc", N),
        ("a\nb\n|c", "2gg", "a\n|b\nc", N),
        ("|a,b,c", "f,", "a|,b,c", N),
        ("|a,b,c", "2f,", "a,b|,c", N),
        ("|a,b,c", "t,", "|a,b,c", N),
        ("|a.b.c", "t.;", "a.|b.c", N),
        ("|a,b,c", "f,;", "a,b|,c", N),
        ("a,b,|c", "F,", "a,b|,c", N),
        ("a,b,|c", "T,", "a,b,|c", N),
        ("|a,b,c", "f,;,", "a|,b,c", N),
        ("|abc", "fz", "|abc", N),
        // delete
        ("|one two three", "dw", "|two three", N),
        ("one |two three", "dw", "one |three", N),
        ("one two |three", "dw", "one two| ", N),
        ("|one two three", "d2w", "|three", N),
        ("|one two three", "2dw", "|three", N),
        ("|a b c d e f g", "2d3w", "|g", N),
        ("one |two three", "de", "one | three", N),
        ("one two |three", "db", "one |three", N),
        ("|one two\nthree", "wdw", "one| \nthree", N),
        ("hel|lo", "d$", "he|l", N),
        ("hel|lo", "D", "he|l", N),
        ("h|ello\nworld", "2D", "|h", N),
        ("|a b\nc d\ne", "dj", "|e", N),
        ("a\n|b\nc", "dk", "|c", N),
        ("a\n|b\nc", "dd", "a\n|c", N),
        ("a\nb\n|c", "dd", "a\n|b", N),
        ("|a\nb\nc", "2dd", "|c", N),
        ("|a\nb\nc", "5dd", "|", N),
        ("  |a\n  b", "dd", "  |b", N),
        ("|a\nb\nc", "dG", "|", N),
        ("a\n|b\nc", "dgg", "|c", N),
        ("|hello world", "dfo", "| world", N),
        ("|hello world", "dtw", "|world", N),
        ("hello |world", "dFe", "h|world", N),
        ("|abc", "dh", "|abc", N),
        ("a|bc", "dl", "a|c", N),
        ("|a\nb", "dj", "|", N),
        ("a\n|b", "dj", "a\n|b", N),
        ("|hello", "x", "|ello", N),
        ("|hello", "3x", "|lo", N),
        ("hel|lo", "5x", "he|l", N),
        ("|hello", "<del>", "|ello", N),
        ("hel|lo", "X", "he|lo", N),
        ("hel|lo", "2X", "h|lo", N),
        ("|hello", "rj", "|jello", N),
        ("|hello", "3rx", "xx|xlo", N),
        ("|hi", "3rx", "|hi", N),
        ("|hello", "~", "H|ello", N),
        ("|hello", "3~", "HEL|lo", N),
        ("|a\nb\nc", "J", "a| b\nc", N),
        ("|a\n  b\nc", "3J", "a b| c", N),
        ("|a \nb", "J", "a |b", N),
        ("|\nb", "J", "|b", N),
        ("|a\n)", "J", "a|)", N),
        // change
        ("one |two three", "cwX<esc>", "one |X three", N),
        ("one tw|o three", "cwX<esc>", "one tw|X three", N),
        ("|one two three", "c2wX<esc>", "|X three", N),
        ("one t|wo three", "ciwX<esc>", "one |X three", N),
        ("one |two three", "cawX<esc>", "one |Xthree", N),
        ("a \"q|uoted\" b", "ci\"X<esc>", "a \"|X\" b", N),
        ("a \"q|uoted\" b", "da\"", "a |b", N),
        ("|a \"x\" b", "di\"", "a \"|\" b", N),
        ("a |\"x\" b", "di\"", "a \"|\" b", N),
        ("a 'x|y' b", "di'", "a '|' b", N),
        ("a `x|y` b", "da`", "a |b", N),
        ("f(a, (b|), c)", "di(", "f(a, (|), c)", N),
        ("f(a, (b|), c)", "2di(", "f(|)", N),
        ("f(a|, b)", "da(", "|f", N),
        ("f(a|, b)", "dib", "f(|)", N),
        ("f(a|, b)", "dab", "|f", N),
        ("(a|b)", "di)", "(|)", N),
        ("|(ab)", "di(", "(|)", N),
        ("(ab|)", "da)", "|", N),
        ("x[1|2]", "ci]9<esc>", "x[|9]", N),
        ("x[1|2]", "da[", "|x", N),
        ("x[1|2]", "di[", "x[|]", N),
        ("{ |a }", "diB", "{|}", N),
        ("{ |a }", "di{", "{|}", N),
        ("{ |a }", "da}", "|", N),
        ("<a|b>", "di<", "<|>", N),
        ("<a|b>", "da>", "|", N),
        ("|a b", "di(", "|a b", N),
        ("ab|c def", "diw", "| def", N),
        ("abc |  def", "diw", "abc|def", N),
        ("abc d|ef ghi", "daw", "abc |ghi", N),
        ("abc d|ef", "daw", "ab|c", N),
        ("abc |  def x", "daw", "abc| x", N),
        ("a.b|.c d", "diW", "| d", N),
        ("a.b|.c d", "daW", "|d", N),
        ("a|bc\ndef", "ccX<esc>", "|X\ndef", N),
        ("  a|bc\ndef", "ccX<esc>", "  |X\ndef", N),
        ("a|bc", "CX<esc>", "a|X", N),
        ("a|bc", "sX<esc>", "a|Xc", N),
        ("a|bc", "SX<esc>", "|X", N),
        ("a|bcd", "2sX<esc>", "a|Xd", N),
        ("|a\nb\nc", "cjX<esc>", "|X\nc", N),
        ("one |two", "cw", "one |", I),
        // yank and paste
        ("|one two", "ywP", "one| one two", N),
        ("|one two", "ywp", "oone| ne two", N),
        ("|a\nb", "yyp", "a\n|a\nb", N),
        ("|a\nb", "yyP", "|a\na\nb", N),
        ("a\n|b", "yyp", "a\nb\n|b", N),
        ("|a\nb", "Yp", "a\n|a\nb", N),
        ("|a\nb", "ddp", "b\n|a", N),
        ("|ab", "xp", "b|a", N),
        ("|ab", "yl3p", "aaa|ab", N),
        ("|a\nb", "yj2P", "|a\nb\na\nb\na\nb", N),
        ("one |two", "yb", "|one two", N),
        ("a\n|b", "yk", "|a\nb", N),
        ("|ab", "p", "|ab", N),
        // indent
        ("|a\nb", ">>", "  |a\nb", N),
        ("|a\nb", "2>>", "  |a\n  b", N),
        ("    |a", "<<", "  |a", N),
        ("\t|a", "<<", "|a", N),
        ("|a\nb\nc", ">j", "  |a\n  b\nc", N),
        ("a|b\n\nc", ">2j", "  |ab\n\n  c", N),
        ("|a b", ">w", "  |a b", N),
        ("(|a\nb)", ">i(", "  |(a\n  b)", N),
        // insert entries
        ("a|bc", "iX<esc>", "a|Xbc", N),
        ("a|bc", "aX<esc>", "ab|Xc", N),
        ("  a|bc", "IX<esc>", "  |Xabc", N),
        ("a|bc", "AX<esc>", "abc|X", N),
        ("a|bc\nd", "oX<esc>", "abc\n|X\nd", N),
        ("a|bc\nd", "OX<esc>", "|X\nabc\nd", N),
        ("|", "iabc<esc>", "ab|c", N),
        ("a|b", "3ix<esc>", "axx|xb", N),
        ("a|b", "ix<cr>y<esc>", "ax\n|yb", N),
        ("a|b", "i", "a|b", I),
        ("a|b", "a", "ab|", I),
        ("|a", "i<esc>", "|a", N),
        // visual
        ("|hello world", "vey", "|hello world", N),
        ("|hello world", "ved", "| world", N),
        ("|hello world", "vecX<esc>", "|X world", N),
        ("|hello world", "vex", "| world", N),
        ("|a\nb\nc", "Vjd", "|c", N),
        ("|a\nb\nc", "Vjy", "|a\nb\nc", N),
        ("|a\nb\nc", "Vj>", "  |a\n  b\nc", N),
        ("  |a\n  b", "Vj<", "|a\nb", N),
        ("|a\nb", "VcX<esc>", "|X\nb", N),
        ("hel|lo", "vhhd", "h|o", N),
        ("|hello", "vllo", "|hello", V),
        ("|hello", "vll", "he|llo", V),
        ("|ab cd", "viwd", "| cd", N),
        ("|ab cd", "vawd", "|cd", N),
        ("f(a|b)", "vi(d", "f(|)", N),
        ("|ab", "v<esc>", "|ab", N),
        ("|ab", "vV", "|ab", VL),
        ("|ab", "Vv", "|ab", V),
        ("|ab", "vv", "|ab", N),
        ("|ab", "VV", "|ab", N),
        ("|abc", "vl~", "|ABc", N),
        ("|ABc", "vlu", "|abc", N),
        ("|abc", "vlU", "|ABc", N),
        ("|a\nb", "VJ", "a| b", N),
        ("|a\nb\nc", "VjJ", "a| b\nc", N),
        ("|a b c", "v2ed", "|", N),
        ("|ab c", "ved", "| c", N),
        ("|a\nb\nc", "vjd", "|\nc", N),
        ("|abc", "v$d", "|", N),
        ("|a\nb", "vjD", "|", N),
        // undo, redo
        ("|one two", "dwu", "|one two", N),
        ("|one two", "dwu<c-r>", "|two", N),
        ("one t|wo three", "ciwX<esc>u", "one |two three", N),
        ("|a b c", "dw.u", "|b c", N),
        ("|a b c", "dw.uu", "|a b c", N),
        ("|abc", "xxuu", "|abc", N),
        ("|abc", "xxuu<c-r><c-r>", "|c", N),
        ("|abc", "xx2u", "|abc", N),
        ("|abc", "iX<esc>u", "|abc", N),
        ("|abc", "u", "|abc", N),
        ("|abc", "<c-r>", "|abc", N),
        ("|abc", "xux", "|bc", N),
        ("|a\nb", "ddpu", "|b", N),
        // dot repeat
        ("|a b c d", "dw.", "|c d", N),
        ("|a b c d", "dw2.", "|d", N),
        ("|a b c d", "d2w.", "|", N),
        ("|abcd", "x..", "|d", N),
        ("|abcdef", "2x.", "|ef", N),
        ("|foo bar", "ciwX<esc>w.", "X |X", N),
        ("|a\nb", "Ax<esc>j.", "ax\nb|x", N),
        ("|a\nb", ">>j.", "  a\n  |b", N),
        ("|a\nb\nc", "2>>j.", "  a\n    |b\n  c", N),
        ("|abc", "3ix<esc>.", "xxxx|xxabc", N),
        ("|a\nb", "ox<esc>j.", "a\nx\nb\n|x", N),
        ("|a b", "wiX<esc>0.", "|Xa Xb", N),
        ("|abcd", "rx.", "|xbcd", N),
        ("|abcd", "rxl.", "x|xcd", N),
        ("|a\nb\nc", "ddj.", "|b", N),
        ("|a b c", "dwj.", "|c", N),
        ("|abc", "yl.", "|abc", N),
        ("|ab", ".", "|ab", N),
        // multibyte
        ("|héllo wörld", "w", "héllo |wörld", N),
        ("|héllo", "x", "|éllo", N),
        ("|héllo", "lx", "h|llo", N),
        ("a|😀b", "x", "a|b", N),
        ("|é", "rx", "|x", N),
        ("caf|é", "a!<esc>", "café|!", N),
        ("|日本 語", "de", "| 語", N),
        ("\"é|ü\"", "di\"", "\"|\"", N),
        // empty buffer and trailing newline
        ("|", "x", "|", N),
        ("|", "dd", "|", N),
        ("|", "p", "|", N),
        ("|", "w", "|", N),
        ("|", "diw", "|", N),
        ("|", "J", "|", N),
        ("|", "u", "|", N),
        ("|", "G", "|", N),
        ("|", "$", "|", N),
        ("|", "vd", "|", N),
        ("|", "Vd", "|", N),
        ("ab\n|", "k", "|ab\n", N),
        ("ab\n|", "dd", "|ab", N),
        ("|ab\n", "G", "ab\n|", N),
        ("|ab\n", "j", "ab\n|", N),
        // cancel and unknown keys
        ("|abc", "d<esc>x", "|bc", N),
        ("|abc", "2<esc>x", "|bc", N),
        ("|abc", "dzx", "|bc", N),
        ("|abc", "Q", "|abc", N),
        ("|abc", "<tab>", "|abc", N),
    ];

    #[test]
    fn key_sequences() {
        let mut failures = Vec::new();
        for &(before, seq, after, mode) in CASES {
            let (got, got_mode) = run(before, seq);
            if got != after || got_mode != mode {
                failures.push(format!(
                    "{before:?} {seq:?}: got {got:?} {got_mode:?}, want {after:?} {mode:?}"
                ));
            }
        }
        assert!(failures.is_empty(), "\n{}", failures.join("\n"));
        assert!(CASES.len() >= 80);
    }

    #[test]
    fn registers() {
        let mut vim = Vim::new();
        run_on(&mut vim, "|hello world", "yw");
        assert_eq!(vim.register(), "hello ");
        run_on(&mut vim, "|a\nb", "yy");
        assert_eq!(vim.register(), "a\n");
        run_on(&mut vim, "|hello world", "vey");
        assert_eq!(vim.register(), "hello");
        run_on(&mut vim, "|ab\ncd", "Vjy");
        assert_eq!(vim.register(), "ab\ncd\n");
        run_on(&mut vim, "one |two", "ciwX<esc>");
        assert_eq!(vim.register(), "two");
    }

    #[test]
    fn outcomes() {
        let mut vim = Vim::new();
        let (_, outs) = run_on(&mut vim, "|abc", "<cr>");
        assert_eq!(outs, [Outcome::Submit]);
        let (_, outs) = run_on(&mut vim, "|abc", "<esc>");
        assert_eq!(outs, [Outcome::Escape]);
        let (_, outs) = run_on(&mut vim, "|abc", "d<esc><esc>");
        assert_eq!(outs, [Outcome::Handled, Outcome::Handled, Outcome::Escape]);
        let (_, outs) = run_on(&mut vim, "|abc", "2<cr>");
        assert_eq!(outs, [Outcome::Handled, Outcome::Handled]);
        let (_, outs) = run_on(&mut vim, "|abc", "v<esc><esc>");
        assert_eq!(outs, [Outcome::Handled, Outcome::Handled, Outcome::Escape]);
        let (_, outs) = run_on(&mut vim, "|abc", "v<cr>");
        assert_eq!(outs, [Outcome::Handled, Outcome::Handled]);
        let (_, outs) = run_on(&mut vim, "|abc", "iq<cr><esc>");
        assert_eq!(
            outs,
            [
                Outcome::Handled,
                Outcome::PassThrough,
                Outcome::PassThrough,
                Outcome::Handled
            ]
        );
        // Chords belong to the app in every mode.
        let mut text = "abc".to_string();
        let mut c = 0;
        vim.set_mode(Mode::Normal, &text, &mut c);
        let cmd_c = Key {
            cmd: true,
            ..Key::char('c')
        };
        assert_eq!(vim.handle(&cmd_c, &mut text, &mut c), Outcome::PassThrough);
        assert_eq!(
            vim.handle(&Key::ctrl("v"), &mut text, &mut c),
            Outcome::PassThrough
        );
        assert_eq!(
            vim.handle(&Key::ctrl("["), &mut text, &mut c),
            Outcome::Escape
        );
        vim.set_mode(Mode::Insert, &text, &mut c);
        assert_eq!(
            vim.handle(&Key::char('x'), &mut text, &mut c),
            Outcome::PassThrough
        );
        assert_eq!(
            vim.handle(&Key::ctrl("["), &mut text, &mut c),
            Outcome::Handled
        );
        assert_eq!(vim.mode(), Mode::Normal);
    }

    #[test]
    fn pending_indicator() {
        let mut vim = Vim::new();
        let mut text = "abc def".to_string();
        let mut c = 0;
        vim.set_mode(Mode::Normal, &text, &mut c);
        for (key, want) in [
            ('2', "2"),
            ('d', "2d"),
            ('3', "2d3"),
            ('i', "2d3i"),
            ('w', ""),
        ] {
            vim.handle(&Key::char(key), &mut text, &mut c);
            assert_eq!(vim.pending(), want);
        }
        vim.handle(&Key::char('g'), &mut text, &mut c);
        assert_eq!(vim.pending(), "g");
        vim.handle(&Key::named("escape"), &mut text, &mut c);
        vim.handle(&Key::char('f'), &mut text, &mut c);
        assert_eq!(vim.pending(), "f");
        vim.handle(&Key::named("escape"), &mut text, &mut c);
        vim.handle(&Key::char('r'), &mut text, &mut c);
        assert_eq!(vim.pending(), "r");
    }

    #[test]
    fn selection_ranges() {
        let mut vim = Vim::new();
        let mut text = "ab\ncd\nef".to_string();
        let mut c = 1;
        vim.set_mode(Mode::Normal, &text, &mut c);
        assert_eq!(vim.selection(&text, c), None);
        vim.handle(&Key::char('v'), &mut text, &mut c);
        assert_eq!(vim.selection(&text, c), Some(1..2));
        vim.handle(&Key::char('j'), &mut text, &mut c);
        assert_eq!(vim.selection(&text, c), Some(1..5));
        vim.handle(&Key::char('V'), &mut text, &mut c);
        assert_eq!(vim.mode(), Mode::VisualLine);
        assert_eq!(vim.selection(&text, c), Some(0..6));
        vim.handle(&Key::char('j'), &mut text, &mut c);
        assert_eq!(vim.selection(&text, c), Some(0..8));
        // Multibyte: the range ends after the whole char.
        let mut text = "é".to_string();
        let mut c = 0;
        vim.set_mode(Mode::Visual, &text, &mut c);
        assert_eq!(vim.selection(&text, c), Some(0..2));
        vim.handle(&Key::char('d'), &mut text, &mut c);
        assert_eq!(text, "");
    }

    #[test]
    fn clamp_and_set_mode() {
        let mut vim = Vim::new();
        assert_eq!(vim.mode(), Mode::Insert);
        assert_eq!(vim.clamp("abc", 3), 3);
        assert_eq!(vim.clamp("é", 1), 0);
        let mut c = 3;
        vim.set_mode(Mode::Normal, "abc", &mut c);
        assert_eq!(c, 2);
        assert_eq!(vim.clamp("abc", 9), 2);
        assert_eq!(vim.clamp("", 9), 0);
        assert_eq!(vim.clamp("ab\n", 3), 3);
        assert_eq!(Mode::Normal.label(), "NORMAL");
        assert_eq!(Mode::Insert.label(), "INSERT");
        assert_eq!(Mode::Visual.label(), "VISUAL");
        assert_eq!(Mode::VisualLine.label(), "V-LINE");
    }

    #[test]
    fn insert_session_started_by_caller_is_one_undo_step() {
        let mut vim = Vim::new();
        let mut text = String::new();
        let mut c = 0;
        for ch in "hi".chars() {
            assert_eq!(
                vim.handle(&Key::char(ch), &mut text, &mut c),
                Outcome::PassThrough
            );
            text.insert(c, ch);
            c += 1;
        }
        vim.handle(&Key::named("escape"), &mut text, &mut c);
        assert_eq!((text.as_str(), c), ("hi", 1));
        vim.handle(&Key::char('u'), &mut text, &mut c);
        assert_eq!(text, "");
    }

    #[test]
    fn parses_key_sequences() {
        let ks = keys("3dw<esc><cr><bs><c-r><lt><space><tab><left>x<y");
        let names: Vec<(&str, Option<char>, bool)> =
            ks.iter().map(|k| (k.key.as_str(), k.ch, k.ctrl)).collect();
        assert_eq!(
            names,
            [
                ("3", Some('3'), false),
                ("d", Some('d'), false),
                ("w", Some('w'), false),
                ("escape", None, false),
                ("enter", None, false),
                ("backspace", None, false),
                ("r", None, true),
                ("<", Some('<'), false),
                ("space", Some(' '), false),
                ("tab", None, false),
                ("left", None, false),
                ("x", Some('x'), false),
                ("<", Some('<'), false),
                ("y", Some('y'), false),
            ]
        );
        assert_eq!(keys("G")[0], Key::char('G'));
        assert_eq!(keys("G")[0].key, "g");
        assert_eq!(keys("<<").len(), 2);
        assert_eq!(keys("<nope>").len(), 6);
        assert!(keys("").is_empty());
    }

    #[test]
    fn inserted_text_diff() {
        assert_eq!(inserted_text("ac", "abc"), "b");
        assert_eq!(inserted_text("", "xyz"), "xyz");
        assert_eq!(inserted_text("aa", "aaa"), "a");
        assert_eq!(inserted_text("é", "éé"), "é");
        assert_eq!(inserted_text("abc", "abc"), "");
    }

    #[test]
    fn random_keys_never_panic() {
        let pool: Vec<Key> = {
            let mut pool: Vec<Key> =
                "hjklwbeWBE0^$gGftFT;,dcy<>xXDCsSrJpPuiaIAoOvV~.123 \"'()[]{}zY"
                    .chars()
                    .map(Key::char)
                    .collect();
            for name in [
                "escape",
                "enter",
                "backspace",
                "delete",
                "left",
                "right",
                "up",
                "down",
                "tab",
            ] {
                pool.push(Key::named(name));
            }
            pool.push(Key::ctrl("r"));
            pool.push(Key::char('é'));
            pool.push(Key::char('😀'));
            pool.push(Key::char('\n'));
            pool
        };
        let buffers = [
            "",
            "hello world",
            "one two\nthree (four [five] {six})\n\n  seven \"eight\" 'nine'",
            "é😀 ü\n\nx",
            "trailing\n",
            "\n\n\n",
        ];
        let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
        let mut next = || {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (seed >> 33) as usize
        };
        for buffer in buffers {
            let mut vim = Vim::new();
            let mut text = buffer.to_string();
            let mut c = 0;
            for _ in 0..6000 {
                let key = &pool[next() % pool.len()];
                let out = vim.handle(key, &mut text, &mut c);
                if out == Outcome::PassThrough
                    && vim.mode() == Mode::Insert
                    && let Some(ch) = key.ch
                {
                    text.insert(c, ch);
                    c += ch.len_utf8();
                }
                assert!(c <= text.len(), "cursor past end");
                assert!(text.is_char_boundary(c), "cursor off a boundary");
                if let Some(sel) = vim.selection(&text, c) {
                    assert!(sel.start <= sel.end && sel.end <= text.len());
                    assert!(text.is_char_boundary(sel.start) && text.is_char_boundary(sel.end));
                }
                if text.len() > 20_000 {
                    text.truncate(0);
                    c = vim.clamp(&text, c);
                }
                if vim.mode() != Mode::Insert {
                    assert_eq!(vim.clamp(&text, c), c, "normal cursor not clamped");
                }
            }
        }
    }
}
