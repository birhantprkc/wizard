//! Keeping `execute` from signalling the process that runs it.
//!
//! To the model, the `wizard acp` hosting its own session is one more line in
//! `ps`. A chat that cleans up "stale" `wizard acp` processes after a flaky
//! bridge kills the one it is running in. The host shuts down on SIGTERM with
//! exit code 0, the client reports that Wizard "exited unexpectedly", and the
//! next prompt starts a fresh host that the same cleanup kills again. The
//! transcript never says why, because the tool call that did it never returns.
//!
//! [`refusal`] reads a command before it runs and refuses one that would
//! signal this process or any of its parents. It is a check on the text of the
//! command, so it covers what a model writes (`kill PID`, `kill 0`, `pkill -f
//! PATTERN`, `killall NAME`, `pgrep ... | xargs kill`,
//! `ps | grep ... | xargs kill`) and not a program that works out a pid for
//! itself. The refusal says what was hit and what to do instead, so the model
//! can carry on without the user having to explain.
//!
//! The parents are protected along with the host because they die the same
//! way: a `pkill zeron` that takes down the GUI's engine ends the session as
//! surely as one that names Wizard.

use std::path::Path;

/// A process a command must not signal: the host, or one of its parents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Process {
    pub pid: u32,
    pub pgid: u32,
    /// Executable name: what `pkill` and `killall` match without `-f`.
    pub comm: String,
    /// Full command line: what `pkill -f` and `ps` match.
    pub cmdline: String,
}

/// The message to give the model instead of running `command`, or `None` when
/// the command does not aim at this process or one of its parents.
///
/// `background` says the command runs in a process group of its own, which is
/// what makes `kill 0` harmless there and fatal in the foreground, where the
/// shell shares this process's group.
pub fn refusal(command: &str, background: bool) -> Option<String> {
    let targets = targets(command, background);
    if targets.is_empty() {
        return None;
    }
    let chain = host_chain();
    let reason = hit(&targets, &chain)?;
    Some(message(&reason, &chain))
}

fn message(reason: &str, chain: &[Process]) -> String {
    let host = &chain[0];
    let mut text = format!(
        "Refused, and not run: this command would signal the Wizard process running you. \
         It targets {reason}. You are pid {} (`{}`)",
        host.pid,
        clip(&host.cmdline, 80),
    );
    let parents: Vec<String> = chain[1..]
        .iter()
        .take(4)
        .map(|p| format!("{} (`{}`)", p.pid, clip(&p.cmdline, 60)))
        .collect();
    if !parents.is_empty() {
        text.push_str(&format!(", started by {}", parents.join(", ")));
    }
    text.push_str(&format!(
        ". Signalling you or a parent ends this session, and the chat only reports that \
         Wizard exited.\n\n\
         A pattern such as `wizard acp` matches you as well as the processes you meant. \
         Kill by explicit pid: list the candidates (`pgrep -af PATTERN`), leave {} out, and \
         `kill` the rest. Better, only signal what you started: keep `$!` or a pidfile when \
         you launch it, or use `task_kill` for a background task.",
        host.pid
    ));
    text
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max).collect();
    format!("{head}...")
}

/// What one command in the line aims at.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    /// `kill 1234`.
    Pid(u32),
    /// `kill -- -1234`: every process in group 1234.
    Group(u32),
    /// `kill -9 -1`, or a `pkill` selected by user alone: everything reachable.
    Everything,
    /// `kill 0`: the caller's own process group.
    OwnGroup,
    /// `kill $PPID`: the shell's parent, which is the host.
    Parent,
    /// A name or pattern a process is matched against.
    Pattern {
        text: String,
        /// Match the whole command line (`pkill -f`) rather than the name.
        full: bool,
        /// Match the whole string (`pkill -x`, `killall`) rather than a part.
        exact: bool,
        /// Select what does *not* match (`pkill -v`).
        invert: bool,
    },
}

/// The first target in `targets` that reaches a process in `chain`, described
/// for the model.
fn hit(targets: &[Target], chain: &[Process]) -> Option<String> {
    targets.iter().find_map(|target| match target {
        Target::Pid(pid) => chain
            .iter()
            .any(|p| p.pid == *pid)
            .then(|| format!("pid {pid}")),
        Target::Group(group) => chain
            .iter()
            .any(|p| p.pgid == *group)
            .then(|| format!("process group {group}")),
        Target::Everything => Some("every process it can signal".to_string()),
        Target::OwnGroup => Some(
            "its own process group (`kill 0`), which this command shares with Wizard".to_string(),
        ),
        Target::Parent => Some("`$PPID`, which is Wizard".to_string()),
        Target::Pattern {
            text,
            full,
            exact,
            invert,
        } => chain
            .iter()
            .find(|p| {
                let haystack = if *full { &p.cmdline } else { &p.comm };
                pattern_matches(text, haystack, *exact) != *invert
            })
            .map(|p| {
                format!(
                    "`{text}`, which matches `{}` (pid {})",
                    clip(&p.cmdline, 60),
                    p.pid
                )
            }),
    })
}

/// Characters that make a `pkill` pattern a regular expression.
const REGEX_META: &str = ".*+?[](){}^$\\";

/// Whether `pattern` would select `haystack`.
///
/// `pkill` patterns are regular expressions and there is no regex engine in
/// this crate, so this errs toward a match: each `|` branch matches when its
/// literal fragments (the text between metacharacters) appear in order. That
/// takes `wizard.*acp` and the `[w]izard` trick for keeping `pkill -f` from
/// finding itself, and it can only refuse a command that was one regex
/// operator away from hitting the host.
fn pattern_matches(pattern: &str, haystack: &str, exact: bool) -> bool {
    if exact && !pattern.contains(|c| REGEX_META.contains(c) || c == '|') {
        return pattern == haystack;
    }
    pattern.split('|').any(|branch| {
        let mut rest = haystack;
        branch
            .split(|c| REGEX_META.contains(c))
            .filter(|fragment| !fragment.is_empty())
            .all(|fragment| match rest.find(fragment) {
                Some(at) => {
                    rest = &rest[at + fragment.len()..];
                    true
                }
                None => false,
            })
    })
}

/// Every target the line's commands aim at.
fn targets(line: &str, background: bool) -> Vec<Target> {
    let mut commands = simple_commands(line, 0);
    let mut resolved: Vec<(String, Vec<String>)> = Vec::new();
    let mut at = 0;
    // `sh -c '...'` and `eval '...'` carry a command line of their own, which
    // joins the queue being walked.
    while at < commands.len() {
        let words = commands[at].clone();
        at += 1;
        let Some((name, args)) = command_word(&words) else {
            continue;
        };
        match name.as_str() {
            "sh" | "bash" | "dash" | "zsh" | "ksh" => {
                let flag = args
                    .iter()
                    .position(|a| a.starts_with('-') && !a.starts_with("--") && a.contains('c'));
                if let Some(body) = flag.and_then(|pos| args.get(pos + 1)) {
                    commands.extend(simple_commands(body, 0));
                }
            }
            "eval" => commands.extend(simple_commands(&args.join(" "), 0)),
            _ => {}
        }
        resolved.push((name, args));
    }

    // `pgrep` and `ps | grep` only pick pids; they matter when something in
    // the same line signals whatever they picked.
    let sends = resolved.iter().any(|(name, _)| name == "kill");
    let lists = resolved.iter().any(|(name, _)| name == "ps");
    let mut out = Vec::new();
    for (name, args) in &resolved {
        match name.as_str() {
            "kill" => kill_targets(args, background, &mut out),
            "pkill" => finder_targets(Finder::Pkill, args, &mut out),
            "killall" => finder_targets(Finder::Killall, args, &mut out),
            "pgrep" if sends => finder_targets(Finder::Pgrep, args, &mut out),
            "pidof" if sends => {
                for arg in args.iter().filter(|a| !a.starts_with('-')) {
                    out.push(Target::Pattern {
                        text: arg.clone(),
                        full: false,
                        exact: true,
                        invert: false,
                    });
                }
            }
            "grep" | "egrep" | "fgrep" | "rg" | "ag" | "ack" if sends && lists => {
                grep_targets(args, &mut out);
            }
            _ => {}
        }
    }
    out
}

/// A signal given as `-9`, `-TERM` or `-SIGKILL`, without the dash.
fn is_signal(body: &str) -> bool {
    !body.is_empty()
        && (body.bytes().all(|b| b.is_ascii_digit())
            || (body.len() > 2
                && body
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())))
}

/// The pids `kill` is asked to signal.
fn kill_targets(args: &[String], background: bool, out: &mut Vec<Target>) {
    let mut signalled = false;
    let mut literal = false;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        i += 1;
        if !literal {
            match arg {
                "--" => {
                    literal = true;
                    continue;
                }
                "-s" | "-n" | "-q" | "--signal" | "--queue" => {
                    i += 1;
                    signalled = true;
                    continue;
                }
                // Lists signal names and sends nothing.
                "-l" | "-L" | "--list" | "-t" | "--table" => return,
                _ => {}
            }
            if arg.starts_with("--signal=") {
                signalled = true;
                continue;
            }
            if let Some(body) = arg.strip_prefix('-')
                && !signalled
                && !body.is_empty()
                && body.bytes().all(|b| b.is_ascii_alphanumeric())
            {
                signalled = true;
                continue;
            }
        }
        match arg {
            "$PPID" | "${PPID}" => out.push(Target::Parent),
            _ => match arg.parse::<i64>() {
                Ok(0) if !background => out.push(Target::OwnGroup),
                Ok(-1) => out.push(Target::Everything),
                Ok(pid) if pid > 0 => {
                    if let Ok(pid) = u32::try_from(pid) {
                        out.push(Target::Pid(pid));
                    }
                }
                Ok(group) if group < -1 => {
                    if let Ok(group) = u32::try_from(-group) {
                        out.push(Target::Group(group));
                    }
                }
                _ => {}
            },
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Finder {
    Pkill,
    Pgrep,
    Killall,
}

/// The names `pkill`, `pgrep` or `killall` select by.
fn finder_targets(finder: Finder, args: &[String], out: &mut Vec<Target>) {
    // Short options that take a value, which is not a pattern.
    let takes_value = match finder {
        Finder::Killall => "suyoZn",
        Finder::Pkill | Finder::Pgrep => "uUgGPstTFOJdr",
    };
    let killall = finder == Finder::Killall;
    let (mut full, mut exact, mut invert, mut regex) = (false, killall, false, false);
    let mut by_user = false;
    let mut names: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        i += 1;
        if arg == "--" {
            names.extend(args[i..].iter().map(String::as_str));
            break;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (key, inline) = long
                .split_once('=')
                .map_or((long, false), |(k, _)| (k, true));
            match key {
                "full" => full = true,
                "exact" => exact = true,
                "invert-match" | "inverse" => invert = true,
                "regexp" => regex = true,
                "euid" | "uid" | "user" => by_user = true,
                _ => {}
            }
            if !inline
                && matches!(
                    key,
                    "signal"
                        | "euid"
                        | "uid"
                        | "user"
                        | "pgroup"
                        | "group"
                        | "parent"
                        | "session"
                        | "terminal"
                        | "pidfile"
                        | "ns"
                        | "nslist"
                        | "runstates"
                        | "delimiter"
                )
            {
                i += 1;
            }
            continue;
        }
        if let Some(body) = arg.strip_prefix('-').filter(|b| !b.is_empty()) {
            if is_signal(body) {
                continue;
            }
            for (at, c) in body.char_indices() {
                match c {
                    'f' if !killall => full = true,
                    'x' if !killall => exact = true,
                    'v' if !killall => invert = true,
                    'r' if killall => regex = true,
                    'u' | 'U' if !killall => {
                        by_user = true;
                        if at + 1 == body.len() {
                            i += 1;
                        }
                        break;
                    }
                    c if takes_value.contains(c) => {
                        if at + c.len_utf8() == body.len() {
                            i += 1;
                        }
                        break;
                    }
                    _ => {}
                }
            }
            continue;
        }
        names.push(arg);
    }
    if names.is_empty() {
        // `pkill -u name` with no pattern selects everything the user owns.
        if by_user && finder == Finder::Pkill {
            out.push(Target::Everything);
        }
        return;
    }
    for name in names {
        out.push(Target::Pattern {
            text: name.to_string(),
            full,
            exact: exact && !regex,
            invert,
        });
    }
}

/// The pattern a `grep` reading `ps` output selects lines by.
fn grep_targets(args: &[String], out: &mut Vec<Target>) {
    let mut pattern: Option<&str> = None;
    let mut positional: Option<&str> = None;
    let mut invert = false;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        i += 1;
        match arg {
            "-e" | "--regexp" => {
                if let Some(next) = args.get(i) {
                    pattern = pattern.or(Some(next));
                }
                i += 1;
            }
            "--invert-match" => invert = true,
            _ if arg.starts_with("--") => {
                if let Some(rest) = arg.strip_prefix("--regexp=") {
                    pattern = pattern.or(Some(rest));
                }
            }
            _ if arg.len() > 1 && arg.starts_with('-') => {
                invert |= arg.contains('v');
                // -A, -B, -C and -m take a count.
                if arg.ends_with(['A', 'B', 'C', 'm']) {
                    i += 1;
                }
            }
            _ => positional = positional.or(Some(arg)),
        }
    }
    // `grep -v grep` drops lines; it does not choose what gets signalled.
    if invert {
        return;
    }
    if let Some(text) = pattern.or(positional) {
        out.push(Target::Pattern {
            text: text.to_string(),
            full: true,
            exact: false,
            invert: false,
        });
    }
}

fn base(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// `NAME=value`, the environment prefix a shell lets a command carry.
fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// Index after any leading options, skipping the value of those in `valued`.
fn skip_options(words: &[String], mut i: usize, valued: &[&str]) -> usize {
    while let Some(word) = words.get(i) {
        if word == "--" {
            return i + 1;
        }
        if !word.starts_with('-') || word == "-" {
            break;
        }
        i += if valued.contains(&word.as_str()) {
            2
        } else {
            1
        };
    }
    i
}

/// The command a simple command runs and its arguments, looking through the
/// prefixes that only launch something else (`sudo`, `env`, `xargs`,
/// `timeout`, an `if`, an assignment).
fn command_word(words: &[String]) -> Option<(String, Vec<String>)> {
    let mut i = 0;
    while let Some(word) = words.get(i) {
        i = match base(word) {
            "if" | "then" | "else" | "elif" | "do" | "while" | "until" | "!" | "{" | "}" => i + 1,
            "time" | "command" | "builtin" | "exec" | "nohup" | "setsid" | "stdbuf" | "ionice" => {
                skip_options(words, i + 1, &[])
            }
            "sudo" | "doas" => skip_options(
                words,
                i + 1,
                &[
                    "-u", "-g", "-h", "-p", "-C", "-r", "-t", "-U", "-D", "-R", "-T",
                ],
            ),
            "env" => skip_options(words, i + 1, &["-u", "-C", "-S"]),
            "nice" => skip_options(words, i + 1, &["-n"]),
            "timeout" => {
                let at = skip_options(words, i + 1, &["-s", "-k"]);
                // The duration.
                at + usize::from(
                    words
                        .get(at)
                        .is_some_and(|w| w.starts_with(|c: char| c.is_ascii_digit())),
                )
            }
            "xargs" => skip_options(
                words,
                i + 1,
                &["-I", "-n", "-P", "-d", "-E", "-L", "-s", "-a", "-l"],
            ),
            _ if is_assignment(word) => i + 1,
            _ => return Some((base(word).to_string(), words[i + 1..].to_vec())),
        };
    }
    None
}

/// How deep `$(...)` may nest before the rest is ignored.
const MAX_NESTING: usize = 8;

/// The simple commands of a line as words with the quoting taken off.
///
/// Not a shell parser. It splits at `;`, `|`, `&`, newlines and parentheses,
/// keeps quoted text whole, drops comments and redirections, skips here-document
/// bodies (a script written with `cat <<EOF` is text, however many `kill`s it
/// holds), and reads `$(...)` and backtick bodies as commands of their own.
fn simple_commands(line: &str, depth: usize) -> Vec<Vec<String>> {
    if depth > MAX_NESTING {
        return Vec::new();
    }
    let chars: Vec<char> = line.chars().collect();
    let mut lexer = Lexer::default();
    let mut nested: Vec<String> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' => {
                if let Some(&next) = chars.get(i + 1) {
                    if next != '\n' {
                        lexer.push(next);
                    }
                    i += 1;
                }
            }
            '\'' => {
                lexer.open = true;
                i += 1;
                while i < chars.len() && chars[i] != '\'' {
                    lexer.word.push(chars[i]);
                    i += 1;
                }
            }
            '"' => {
                lexer.open = true;
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        i += 1;
                    } else if chars[i] == '$' && chars.get(i + 1) == Some(&'(') {
                        let (body, end) = paren_body(&chars, i + 2);
                        lexer.word.extend(&chars[i..end.min(chars.len())]);
                        nested.push(body);
                        i = end;
                        continue;
                    }
                    lexer.word.push(chars[i]);
                    i += 1;
                }
            }
            '`' => {
                let end = chars[i + 1..]
                    .iter()
                    .position(|&c| c == '`')
                    .map_or(chars.len(), |at| i + 1 + at);
                nested.push(chars[i + 1..end].iter().collect());
                lexer.word.push_str("$(...)");
                lexer.open = true;
                i = end;
            }
            '$' if chars.get(i + 1) == Some(&'(') => {
                let (body, end) = paren_body(&chars, i + 2);
                nested.push(body);
                lexer.word.push_str("$(...)");
                lexer.open = true;
                i = end.saturating_sub(1);
            }
            '#' if !lexer.open => {
                while i + 1 < chars.len() && chars[i + 1] != '\n' {
                    i += 1;
                }
            }
            '\n' => {
                lexer.end_command();
                if !lexer.heredocs.is_empty() {
                    i = skip_heredocs(&chars, i + 1, &mut lexer.heredocs);
                    continue;
                }
            }
            ';' | '|' | '(' | ')' => lexer.end_command(),
            '>' | '<' => {
                i = lexer.redirect(&chars, i);
                continue;
            }
            '&' => {
                if chars.get(i + 1) == Some(&'>') {
                    i = lexer.redirect(&chars, i + 1);
                    continue;
                }
                lexer.end_command();
            }
            c if c.is_whitespace() => lexer.end_word(),
            c => lexer.push(c),
        }
        i += 1;
    }
    lexer.end_command();
    let mut commands = lexer.commands;
    for body in nested {
        commands.extend(simple_commands(&body, depth + 1));
    }
    commands
}

/// The text of a `$(...)` whose body starts at `start`, and the index just
/// past its closing parenthesis.
fn paren_body(chars: &[char], start: usize) -> (String, usize) {
    let mut depth = 1;
    let mut i = start;
    while i < chars.len() {
        match chars[i] {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return (chars[start..i].iter().collect(), i + 1);
                }
            }
            _ => {}
        }
        i += 1;
    }
    (
        chars[start.min(chars.len())..].iter().collect(),
        chars.len(),
    )
}

/// Index of the line after the here-documents that opened on the line before
/// it, whose bodies run to their delimiters.
fn skip_heredocs(chars: &[char], mut i: usize, delimiters: &mut Vec<String>) -> usize {
    for delimiter in delimiters.drain(..) {
        while i < chars.len() {
            let end = chars[i..]
                .iter()
                .position(|&c| c == '\n')
                .map_or(chars.len(), |at| i + at);
            let line: String = chars[i..end].iter().collect();
            i = (end + 1).min(chars.len());
            if line.trim_start_matches('\t') == delimiter {
                break;
            }
        }
    }
    i
}

#[derive(Default)]
struct Lexer {
    commands: Vec<Vec<String>>,
    words: Vec<String>,
    word: String,
    /// A word has started, so `''` counts as one.
    open: bool,
    /// The next word is a redirection's target, not an argument.
    skip_next: bool,
    heredocs: Vec<String>,
}

impl Lexer {
    fn push(&mut self, c: char) {
        self.word.push(c);
        self.open = true;
    }

    fn end_word(&mut self) {
        if !self.open {
            return;
        }
        let word = std::mem::take(&mut self.word);
        self.open = false;
        if std::mem::take(&mut self.skip_next) {
            return;
        }
        self.words.push(word);
    }

    fn end_command(&mut self) {
        self.end_word();
        self.skip_next = false;
        if !self.words.is_empty() {
            self.commands.push(std::mem::take(&mut self.words));
        }
    }

    /// Consume the redirection operator at `chars[i]` and return the index
    /// after it. A leading file descriptor (`2>`) is part of the operator, and
    /// so is the delimiter of a here-document.
    fn redirect(&mut self, chars: &[char], mut i: usize) -> usize {
        if self.open && self.word.bytes().all(|b| b.is_ascii_digit()) {
            self.word.clear();
            self.open = false;
        } else {
            self.end_word();
        }
        let first = chars[i];
        let mut run = 0;
        while matches!(chars.get(i), Some('>' | '<')) {
            run += 1;
            i += 1;
        }
        if first == '<' && run == 2 {
            if chars.get(i) == Some(&'-') {
                i += 1;
            }
            while matches!(chars.get(i), Some(' ' | '\t')) {
                i += 1;
            }
            let mut delimiter = String::new();
            while let Some(&c) = chars.get(i) {
                if c.is_whitespace() || matches!(c, ';' | '&' | '|' | '<' | '>' | '(' | ')') {
                    break;
                }
                if !matches!(c, '\'' | '"' | '\\') {
                    delimiter.push(c);
                }
                i += 1;
            }
            if !delimiter.is_empty() {
                self.heredocs.push(delimiter);
            }
            return i;
        }
        match chars.get(i) {
            // `>&2`: a descriptor, not a file.
            Some('&') => {
                i += 1;
                while matches!(chars.get(i), Some(c) if c.is_ascii_digit() || *c == '-') {
                    i += 1;
                }
            }
            Some('|') => {
                i += 1;
                self.skip_next = true;
            }
            _ => self.skip_next = true,
        }
        i
    }
}

/// This process and its parents, nearest first, stopping before init.
fn host_chain() -> Vec<Process> {
    let mut chain = Vec::new();
    let mut pid = std::process::id();
    while chain.len() < 32 {
        let Some((process, parent)) = read_process(pid) else {
            break;
        };
        chain.push(process);
        if parent <= 1 || parent == pid {
            break;
        }
        pid = parent;
    }
    if chain.is_empty() {
        // Where process details cannot be read, the pid alone still protects
        // the host from a literal `kill`.
        let exe = std::env::current_exe().ok();
        let comm = exe
            .as_deref()
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "wizard".to_string());
        chain.push(Process {
            pid: std::process::id(),
            pgid: 0,
            cmdline: comm.clone(),
            comm,
        });
    }
    chain
}

/// `pid`'s details and its parent's pid.
#[cfg(target_os = "linux")]
fn read_process(pid: u32) -> Option<(Process, u32)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // `pid (comm) state ppid pgrp ...`; the name may hold spaces and
    // parentheses, so the fields start after the last `)`.
    let mut fields = stat[stat.rfind(')')? + 1..].split_whitespace();
    let _state = fields.next()?;
    let parent = fields.next()?.parse().ok()?;
    let pgid = fields.next()?.parse().ok()?;
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let cmdline = String::from_utf8_lossy(&cmdline)
        .trim_end_matches('\0')
        .replace('\0', " ");
    let comm = comm.trim_end().to_string();
    let cmdline = if cmdline.is_empty() {
        comm.clone()
    } else {
        cmdline
    };
    Some((
        Process {
            pid,
            pgid,
            comm,
            cmdline,
        },
        parent,
    ))
}

/// `pid`'s details and its parent's pid, from `ps` where there is no `/proc`.
#[cfg(all(unix, not(target_os = "linux")))]
fn read_process(pid: u32) -> Option<(Process, u32)> {
    let ps = |field: &str| -> Option<String> {
        let output = std::process::Command::new("ps")
            .args(["-o", field, "-p", &pid.to_string()])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
    };
    let head = ps("ppid=,pgid=,comm=")?;
    let mut rest = head.as_str();
    let parent = take_number(&mut rest)?;
    let pgid = take_number(&mut rest)?;
    let comm = base(rest.trim()).to_string();
    let cmdline = ps("args=")
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| comm.clone());
    Some((
        Process {
            pid,
            pgid,
            comm,
            cmdline,
        },
        parent,
    ))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn take_number(rest: &mut &str) -> Option<u32> {
    let text = rest.trim_start();
    let end = text.find(char::is_whitespace).unwrap_or(text.len());
    let number = text[..end].parse().ok()?;
    *rest = &text[end..];
    Some(number)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(pid: u32, pgid: u32, comm: &str, cmdline: &str) -> Process {
        Process {
            pid,
            pgid,
            comm: comm.to_string(),
            cmdline: cmdline.to_string(),
        }
    }

    /// The chain from the session that motivated this: a `wizard acp` started
    /// by the GUI's engine, under a user manager.
    fn host() -> Vec<Process> {
        vec![
            process(621495, 4240, "wizard", "/usr/local/bin/wizard acp"),
            process(
                4240,
                4240,
                "zeron",
                "/home/teddy/.zeron/app/current/zeron headless",
            ),
            process(1500, 1500, "systemd", "/usr/lib/systemd/systemd --user"),
        ]
    }

    fn refused(command: &str) -> bool {
        hit(&targets(command, false), &host()).is_some()
    }

    fn refused_in_background(command: &str) -> bool {
        hit(&targets(command, true), &host()).is_some()
    }

    /// The commands that ended the session, as the agent wrote them, with
    /// the host's pid in place of the one it had then.
    #[test]
    fn the_kills_that_ended_the_session_are_refused() {
        assert!(refused(
            "kill 621495 599776 2>/dev/null || true; sleep 0.2; \
             ps -eo pid,cmd | rg 'usb-bridge|iproxy|wizard acp' | rg -v rg || echo clean"
        ));
        assert!(refused(
            "kill 621495 2>/dev/null || true; find ios/xtool -name '*.app' -o -name '*.bundle' | head -40"
        ));
        assert!(refused(
            "kill $(cat /tmp/wizard-ios-bridge.pid) 2>/dev/null || true\n\
             pkill -f 'usb-bridge.py' || true\n\
             pgrep -af 'wizard acp' | awk '{print $1}' | xargs -r kill"
        ));
    }

    #[test]
    fn a_literal_pid_is_refused_however_it_is_written() {
        assert!(refused("kill 621495"));
        assert!(refused("kill -9 621495"));
        assert!(refused("kill -TERM 700 621495"));
        assert!(refused("kill -s KILL 621495"));
        assert!(refused("/bin/kill --signal=KILL 621495"));
        assert!(refused("sudo kill 621495"));
        assert!(refused("sudo -u root kill 621495"));
        assert!(refused("env FOO=1 kill 621495"));
        assert!(refused("FOO=1 kill 621495"));
        assert!(refused("timeout 5 kill 621495"));
        assert!(refused("echo x; kill 621495"));
        assert!(refused("true && kill 621495"));
        assert!(refused("if true; then kill 621495; fi"));
        assert!(refused("(kill 621495)"));
        assert!(refused("kill 621495 2>&1"));
        assert!(refused("kill 621495 &"));
        assert!(refused("bash -c 'kill 621495'"));
        assert!(refused("sh -c \"echo hi; kill 621495\""));
        assert!(refused("eval 'kill 621495'"));
        assert!(refused("echo $(kill 621495)"));
        assert!(refused("echo \"$(kill 621495)\""));
        assert!(refused("echo `kill 621495`"));
    }

    #[test]
    fn a_parent_of_the_host_is_protected_too() {
        assert!(refused("kill 4240"));
        assert!(refused("kill -9 1500"));
        assert!(refused("pkill zeron"));
        assert!(refused("pkill -f 'zeron headless'"));
        assert!(refused("killall systemd"));
    }

    #[test]
    fn process_groups_and_the_shell_s_parent_are_refused() {
        assert!(refused("kill -- -4240"));
        assert!(refused("kill -9 -- -4240"));
        assert!(refused("kill -TERM -4240"));
        assert!(refused("kill -9 -1"));
        assert!(refused("kill $PPID"));
        assert!(refused("kill -9 ${PPID}"));
        assert!(refused("kill 0"));
        assert!(refused("kill -TERM 0"));
        // A background task has a group of its own; `kill 0` reaches only it.
        assert!(!refused_in_background("kill 0"));
        assert!(refused_in_background("kill 621495"));
        assert!(!refused("kill -- -9999"));
    }

    #[test]
    fn a_pattern_that_matches_the_host_is_refused() {
        assert!(refused("pkill wizard"));
        assert!(refused("pkill -9 wizard"));
        assert!(refused("pkill -f 'wizard acp'"));
        assert!(refused("pkill -f \"wizard acp\" 2>/dev/null || true"));
        assert!(refused("pkill -9f wizard"));
        assert!(refused("pkill -f wizard.*acp"));
        assert!(refused("pkill -f '[w]izard acp'"));
        assert!(refused("pkill -f 'usb-bridge|wizard acp'"));
        assert!(refused("pkill -x wizard"));
        assert!(refused("pkill -TERM -f /usr/local/bin/wizard"));
        assert!(refused("killall wizard"));
        assert!(refused("killall -9 wizard"));
        assert!(refused("killall -s KILL wizard"));
        assert!(refused("killall -r 'wiz.*'"));
        assert!(refused("pkill -u teddy"));
        assert!(refused("pkill -v iproxy"));
        assert!(refused("sudo pkill -f wizard"));
    }

    /// The pid lists are worked out by another command, and a `kill` further
    /// along acts on them.
    #[test]
    fn a_pid_list_that_includes_the_host_is_refused() {
        assert!(refused("kill $(pgrep -f 'wizard acp')"));
        assert!(refused("kill -9 `pgrep wizard`"));
        assert!(refused("pgrep -af wizard | awk '{print $1}' | xargs kill"));
        assert!(refused("pgrep -f wizard | xargs -r kill -9"));
        assert!(refused("kill $(pidof wizard)"));
        assert!(refused(
            "ps -eo pid,cmd | rg 'usb-bridge|iproxy|wizard acp' | rg -v rg | awk '{print $1}' | xargs kill"
        ));
        assert!(refused(
            "ps aux | grep wizard | awk '{print $2}' | xargs kill -9"
        ));
        assert!(refused("ps aux | grep -e 'wizard' | xargs kill"));
    }

    #[test]
    fn signalling_something_else_is_left_alone() {
        assert!(!refused("kill 700"));
        assert!(!refused("kill -9 700 701"));
        assert!(!refused("kill -9 $(cat /tmp/bridge.pid)"));
        assert!(!refused("kill $!"));
        assert!(!refused("kill %1"));
        assert!(!refused("kill -l"));
        assert!(!refused("kill -0 700"));
        assert!(!refused("pkill iproxy"));
        assert!(!refused("pkill -f usb-bridge.py"));
        assert!(!refused("pkill -f 'python3 .*bridge'"));
        assert!(!refused("pkill -x wiz"));
        assert!(!refused("pkill -u teddy iproxy"));
        assert!(!refused("killall iproxy"));
        assert!(!refused("killall -9 -s KILL iproxy"));
        assert!(!refused("pgrep -f usb-bridge | xargs kill"));
        assert!(!refused("pgrep -af 'wizard acp'"));
        assert!(!refused("pgrep wizard"));
        assert!(!refused("ps aux | grep wizard"));
        assert!(!refused(
            "ps -eo pid,cmd | rg 'usb-bridge|iproxy' | awk '{print $1}' | xargs kill"
        ));
        assert!(!refused("ps aux | grep -v wizard | xargs kill"));
        assert!(!refused("cargo build && ./target/debug/wizard --version"));
        assert!(!refused("ls"));
    }

    /// The words `kill` and `pkill` appear in commands that only mention them.
    #[test]
    fn text_that_mentions_a_kill_is_not_a_kill() {
        assert!(!refused("echo kill 621495"));
        assert!(!refused("echo \"pkill wizard\""));
        assert!(!refused("git commit -m 'kill 621495 on exit'"));
        assert!(!refused("rg -n 'pkill -f wizard' src"));
        assert!(!refused("man kill"));
        assert!(!refused("which pkill"));
        assert!(!refused("cat notes.txt # pkill wizard"));
        assert!(!refused("echo hi > kill"));
        assert!(!refused("echo hi 2> pkill"));
    }

    /// A script written with a here-document is text, however many kills it
    /// holds; the model does this constantly when it builds a helper.
    #[test]
    fn a_here_document_is_text() {
        let script = "cat > stop.sh <<'EOF'\n#!/bin/sh\npkill -f 'wizard acp'\nkill 621495\nEOF\nchmod +x stop.sh";
        assert!(!refused(script));
        assert!(!refused("cat <<EOF\npkill wizard\nEOF"));
        assert!(!refused(
            "python3 - <<- 'PY'\n\tos.kill(621495, 9)\n\tPY\nls"
        ));
        // A command after the body still counts.
        assert!(refused("cat <<EOF\nhello\nEOF\nkill 621495"));
        // And one on the opening line does too.
        assert!(refused("cat <<EOF | kill 621495\nhello\nEOF"));
    }

    #[test]
    fn quoting_and_odd_input_do_not_break_the_reader() {
        assert!(refused("kill '621495'"));
        assert!(refused("kill \"621495\""));
        assert!(refused("kill\t621495"));
        assert!(refused("kill \\\n621495"));
        assert!(!refused(""));
        assert!(!refused("   "));
        assert!(!refused("kill"));
        assert!(!refused("pkill"));
        assert!(!refused("echo 'unterminated"));
        assert!(!refused("echo \"unterminated"));
        assert!(!refused("echo $(unterminated"));
        assert!(!refused("kill 99999999999999999999"));
        // Deep nesting stops reading rather than recursing without end.
        let nested = format!("{}kill 621495{}", "echo $(".repeat(50), ")".repeat(50));
        let _ = refused(&nested);
    }

    #[test]
    fn a_pattern_is_matched_by_its_literal_fragments() {
        let hay = "/usr/local/bin/wizard acp";
        assert!(pattern_matches("wizard acp", hay, false));
        assert!(pattern_matches("wizard.*acp", hay, false));
        assert!(pattern_matches("[w]izard", hay, false));
        assert!(pattern_matches("nothing|wizard", hay, false));
        assert!(pattern_matches(".*", hay, false));
        assert!(!pattern_matches("acp.*wizard", hay, false));
        assert!(!pattern_matches("iproxy", hay, false));
        assert!(pattern_matches("wizard", "wizard", true));
        assert!(!pattern_matches("wiz", "wizard", true));
        assert!(pattern_matches("wiz.*", "wizard", true));
    }

    #[test]
    fn the_refusal_names_the_process_and_the_way_out() {
        let reason = hit(&targets("kill 621495", false), &host()).expect("the host is targeted");
        let text = message(&reason, &host());
        assert!(text.starts_with("Refused, and not run"), "{text}");
        assert!(text.contains("pid 621495"), "{text}");
        assert!(text.contains("/usr/local/bin/wizard acp"), "{text}");
        assert!(text.contains("zeron headless"), "{text}");
        assert!(text.contains("leave 621495 out"), "{text}");
        assert!(text.contains("task_kill"), "{text}");
    }

    /// Reading the real process table: the host is first, and its parent
    /// follows.
    #[test]
    fn the_host_chain_starts_at_this_process() {
        let chain = host_chain();
        assert_eq!(chain[0].pid, std::process::id());
        assert!(!chain[0].comm.is_empty());
        #[cfg(target_os = "linux")]
        {
            assert!(chain.len() >= 2, "a test process has a parent: {chain:?}");
            assert_ne!(chain[0].pgid, 0);
        }
        assert!(chain.iter().all(|p| p.pid > 1));
    }

    #[test]
    fn a_kill_aimed_at_this_process_is_refused_for_real() {
        // `-0` sends nothing, so a regression fails this test rather than
        // ending the run that reports it.
        let refusal = refusal(&format!("kill -0 {}", std::process::id()), false)
            .expect("the test process is the host here");
        assert!(
            refusal.contains(&std::process::id().to_string()),
            "{refusal}"
        );
        assert!(super::refusal("kill -0 1", false).is_none());
        assert!(super::refusal("echo hello", false).is_none());
    }
}
