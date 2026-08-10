//! Terminal lifecycle: raw mode + alternate screen setup/teardown, editor
//! suspension (`$EDITOR`), and clipboard writes (native tools + OSC 52).

use anyhow::{Context, Result};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::config::Config;

use super::App;

pub(super) type Tui = Terminal<CrosstermBackend<std::io::Stdout>>;

pub(super) fn setup_terminal() -> Result<Tui> {
    crossterm::terminal::enable_raw_mode().context("enabling raw mode")?;
    let mut stdout = std::io::stdout();
    // Capture the mouse so the scroll wheel scrolls the transcript (see the
    // ScrollUp/ScrollDown handler in `handle_event`). Without capture, the
    // terminal translates the wheel into ↑/↓ arrow keys in the alternate
    // screen, which the composer reads as input-history recall — so spinning
    // the wheel cycled previous messages instead of scrolling the text.
    // Tradeoff: capture pre-empts the terminal's native click-drag-to-select,
    // so wizard draws its own selection instead — drag to highlight, and the
    // covered text is copied to the clipboard on release (native tool first,
    // OSC 52 as fallback — see `copy_to_clipboard`, the Down/Drag/Up handlers
    // in `handle_event`, and the highlight overlay in `crate::ui`). Holding
    // Shift still forces the terminal's own selection as a fallback. Bracketed
    // paste stays on so pasted text lands in the composer as one chunk.
    crossterm::execute!(
        stdout,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableBracketedPaste,
        crossterm::event::EnableMouseCapture,
    )
    .context("entering alternate screen")?;
    // Kitty keyboard protocol (best-effort): with disambiguation on, terminals
    // report Shift+Enter as Enter+SHIFT instead of a bare Enter, which lets the
    // composer bind it to a newline. Push unconditionally — unsupported
    // terminals ignore the CSI, and Pop in `restore_terminal` is also a no-op
    // there. Do **not** call `supports_keyboard_enhancement()` here: that
    // probe writes a CSI query and blocks on the reply (up to 2s, or forever
    // if another reader is draining stdin), and we already entered the
    // alternate screen above, so a hang looks like a blank Wizard that only
    // Ctrl-C can leave. Alt+Enter remains the fallback where the push is a
    // no-op.
    let _ = crossterm::execute!(
        stdout,
        crossterm::event::PushKeyboardEnhancementFlags(
            crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES,
        ),
    );
    Terminal::new(CrosstermBackend::new(stdout)).context("creating terminal")
}

/// Resolve the external editor: `$VISUAL`, then `$EDITOR`, then `nvim` when
/// it's on PATH. `None` means nothing usable is configured.
fn resolve_editor() -> Option<String> {
    for var in ["VISUAL", "EDITOR"] {
        if let Ok(editor) = std::env::var(var)
            && !editor.trim().is_empty()
        {
            return Some(editor);
        }
    }
    let nvim_on_path = std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join("nvim").is_file()));
    nvim_on_path.then(|| "nvim".to_string())
}

/// The command that opens `editor` on `path`.
///
/// Through the platform shell so editors configured with flags ("code --wait",
/// "emacsclient -t") work: the setting is a command *line*, not a program
/// name, so `Command::new(editor)` would look for a binary called
/// "code --wait". Its own function so that property is testable without
/// suspending a terminal.
fn editor_command(editor: &str, path: &std::path::Path) -> std::process::Command {
    crate::platform::shell::command(&format!("{editor} \"{}\"", path.display()))
}

/// Suspend the TUI, run `editor` on `path`, then restore the TUI. Returns the
/// editor's exit status, or `None` when the TUI could not be suspended or
/// restored (a notice is posted either way; an unrestored terminal is fatal
/// to the session, so the caller must not continue).
fn run_editor_suspended(
    app: &mut App,
    terminal: &mut Tui,
    editor: &str,
    path: &std::path::Path,
) -> Option<std::io::Result<std::process::ExitStatus>> {
    // Leave the alternate screen so the editor draws on the real terminal.
    if let Err(err) = restore_terminal() {
        app.notice(format!("could not suspend the TUI: {err:#}"));
        return None;
    }
    let status = editor_command(editor, path).status();

    // Re-enter the TUI regardless of how the editor exited.
    match setup_terminal() {
        Ok(new_terminal) => {
            *terminal = new_terminal;
            let _ = terminal.clear();
            Some(status)
        }
        Err(err) => {
            app.notice(format!(
                "could not restore the TUI: {err:#} — /quit and relaunch"
            ));
            None
        }
    }
}

/// Suspend the TUI, open the external editor on `~/.wizard/config.toml`, then
/// restore the TUI and reload the edited config. Driven by the `/settings`
/// "Open config file" row; runs from the main loop because it owns `terminal`.
/// Falls back to a path notice when no editor is configured.
pub(super) fn edit_config_file(app: &mut App, terminal: &mut Tui) {
    let path = match Config::path() {
        Ok(path) => path,
        Err(err) => {
            app.notice(format!("could not locate config: {err:#}"));
            return;
        }
    };
    let Some(editor) = resolve_editor() else {
        app.notice(format!(
            "no $EDITOR set — edit {} by hand, then /reload",
            path.display()
        ));
        return;
    };

    let Some(status) = run_editor_suspended(app, terminal, &editor, &path) else {
        return;
    };
    match status {
        Ok(status) if status.success() => match Config::load() {
            Ok(config) => {
                app.config = config;
                app.status.mode = app.config.mode;
                app.status.model = app.config.active().model;
                app.notice("config reloaded — restart for provider/model changes to take effect");
            }
            Err(err) => app.notice(format!("config not reloaded (parse error): {err:#}")),
        },
        Ok(_) => app.notice("editor exited without success — config not reloaded"),
        Err(err) => app.notice(format!("could not launch editor: {err:#}")),
    }
}

/// The name every composer draft starts with. The pid keeps two live sessions
/// off each other's file; the prefix is what the sweep matches on.
const DRAFT_PREFIX: &str = "wizard-prompt-";

/// How long an abandoned draft survives before the next Ctrl-G removes it.
///
/// The editor is modal (it owns the terminal while it runs), so a draft this
/// old belongs to a session that died between staging the file and reading it
/// back, and nothing is going to come for it. Generous anyway, because the
/// cost of sweeping too eagerly is deleting text a user still wants and the
/// cost of sweeping too late is a few kilobytes.
const DRAFT_TTL: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 60 * 60);

/// Where the composer draft is staged for the external editor.
///
/// Wizard's own private scratch directory rather than the shared temp dir: the
/// draft is an unsent prompt (often the most sensitive text in the session, and
/// frequently pasted credentials or logs), the name is predictable from the
/// pid, and `/tmp` is world-writable, so another local user could read it or
/// pre-plant the name and have the editor follow their symlink.
///
/// Hardened best-effort, not strictly, which is why this does not go through
/// [`crate::platform::paths::staging_dir`]: that helper's policy is set by its
/// other caller, where the staged file becomes the argument to `sudo install`
/// and a loose mode is worse than no directory at all. Here a `chmod` that the
/// filesystem cannot express (exFAT, WSL DrvFs, a CIFS mount with `WIZARD_HOME`
/// on it) would take Ctrl-G away entirely, in exchange for a protection that
/// filesystem was never going to provide. That is the same trade, and the same
/// reasoning, as the state tree's own directories.
fn prompt_scratch_path() -> Result<std::path::PathBuf> {
    let dir = crate::platform::paths::state_dir()?.join("scratch");
    crate::platform::secrets::create_private_dir(&dir)?;
    // The draft outlives the process whenever the TUI could not be restored
    // around the editor, and `/tmp`'s reaper is not here to collect it any
    // more. Sweeping on the way in keeps that from accumulating unsent prompts
    // for the life of the install.
    sweep_stale_drafts(&dir, DRAFT_TTL);
    Ok(dir.join(format!("{DRAFT_PREFIX}{}.md", std::process::id())))
}

/// Remove drafts in `dir` that have not been touched for `ttl`. Best effort
/// throughout: a scratch sweep must never be the reason the editor does not
/// open.
fn sweep_stale_drafts(dir: &std::path::Path, ttl: std::time::Duration) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        let is_draft = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(DRAFT_PREFIX));
        if !is_draft {
            continue;
        }
        // An unreadable mtime (or one in the future, which a clock change or a
        // restored backup can produce) reads as "young": the sweep's job is to
        // stop unbounded growth, not to guess at a file it cannot date.
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= ttl);
        if stale {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Write the draft the editor is about to open.
///
/// Owner-only where the filesystem can express it, and best effort where it
/// cannot, for the same reason [`prompt_scratch_path`] hardens its directory
/// that way. Deliberately not [`crate::platform::secrets::write_private_atomic`]:
/// that primitive hardens the parent strictly, which would put the hard
/// failure straight back. The 0700 directory is the lock that actually holds
/// here; the file mode is a second one on the same door.
fn stage_draft(path: &std::path::Path, text: &str) -> Result<()> {
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
    if let Err(err) = crate::platform::secrets::harden_file(path) {
        tracing::warn!(
            "could not restrict permissions on {} ({err:#}); \
             the draft is readable by other users on this filesystem",
            path.display()
        );
    }
    Ok(())
}

/// Suspend the TUI and open the composer draft in the external editor
/// (Ctrl-G); on a clean exit the edited file replaces the input with the
/// cursor at the end. A nonzero exit leaves the composer untouched. Runs from
/// the main loop because it owns `terminal`.
pub(super) fn edit_prompt_in_editor(app: &mut App, terminal: &mut Tui) {
    let Some(editor) = resolve_editor() else {
        app.notice("no $VISUAL/$EDITOR set and nvim not on PATH — cannot edit the prompt");
        return;
    };

    let path = match prompt_scratch_path() {
        Ok(path) => path,
        Err(err) => {
            app.notice(format!("could not stage the prompt: {err:#}"));
            return;
        }
    };
    if let Err(err) = stage_draft(&path, &app.input) {
        app.notice(format!("could not stage the prompt: {err:#}"));
        return;
    }

    let Some(status) = run_editor_suspended(app, terminal, &editor, &path) else {
        // The draft stays behind here on purpose: the editor may already have
        // run, in which case this file holds the only copy of what the user
        // just wrote, and the session is over either way. `sweep_stale_drafts`
        // is what keeps it from outliving its usefulness.
        return;
    };
    match status {
        Ok(status) if status.success() => match std::fs::read_to_string(&path) {
            Ok(text) => app.set_input_from_editor(text),
            Err(err) => app.notice(format!("could not read the edited prompt: {err:#}")),
        },
        Ok(_) => app.notice("editor exited without success — prompt unchanged"),
        Err(err) => app.notice(format!("could not launch editor: {err:#}")),
    }
    let _ = std::fs::remove_file(&path);
}

/// Copy `text` to the system clipboard.
///
/// Prefers a native clipboard tool (`wl-copy` / `xclip` / `pbcopy` / …) when one
/// is available: OSC 52 alone often looks successful while never reaching the
/// host clipboard — common under tmux, and under Alacritty on Wayland where the
/// sequence is ignored or stripped. Falls back to OSC 52 so copy still works
/// over SSH when no local clipboard tool is on PATH.
pub(super) fn copy_to_clipboard(text: &str) -> Result<()> {
    if copy_via_native(text).is_ok() {
        // Still emit OSC 52 best-effort so a remote client (or a terminal that
        // only understands the escape) also picks it up. Failures here are
        // silent: the native write already succeeded.
        let _ = copy_via_osc52(text);
        return Ok(());
    }
    copy_via_osc52(text).context(
        "no native clipboard tool succeeded (wl-copy/xclip/xsel/pbcopy) and OSC 52 write failed",
    )
}

/// OSC 52: `ESC ] 52 ; c ; <base64> BEL` — `c` targets the clipboard. Needs no
/// clipboard daemon and works over SSH when the terminal (and any multiplexer)
/// forwards the sequence. Written straight to stdout; non-printing, so it does
/// not disturb the rendered frame.
fn copy_via_osc52(text: &str) -> Result<()> {
    use base64::Engine;
    use std::io::Write;

    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let mut stdout = std::io::stdout();
    write!(stdout, "\x1b]52;c;{encoded}\x07").context("writing clipboard escape")?;
    stdout.flush().context("flushing clipboard escape")?;
    Ok(())
}

/// Pipe `text` into the first available OS clipboard writer.
fn copy_via_native(text: &str) -> Result<()> {
    // (program, args). Order: Wayland first when a Wayland session is visible,
    // then X11, then the macOS / Windows builtins. `pipe_to` only returns Ok
    // when the process exits zero, so a missing binary or a dead compositor
    // just falls through to the next candidate.
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var_os("WAYLAND_SOCKET").is_some();

    let mut candidates: Vec<(&str, &[&str])> = Vec::new();
    if wayland {
        candidates.push(("wl-copy", &[]));
    }
    candidates.extend_from_slice(&[
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
        ("pbcopy", &[]),   // macOS
        ("clip.exe", &[]), // Windows (WSL / native)
    ]);
    if !wayland {
        // wl-copy can still work under XWayland-ish setups; try it last if we
        // didn't already lead with it.
        candidates.push(("wl-copy", &[]));
    }

    let mut last_err: Option<anyhow::Error> = None;
    for (cmd, args) in candidates {
        match pipe_to(cmd, args, text.as_bytes()) {
            Ok(()) => return Ok(()),
            Err(err) => last_err = Some(err),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no clipboard tool available")))
}

/// Spawn `cmd args`, write `bytes` to its stdin, and require a zero exit.
fn pipe_to(cmd: &str, args: &[&str], bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("spawning {cmd}"))?;
    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("{cmd} stdin missing"))?;
        stdin
            .write_all(bytes)
            .with_context(|| format!("writing to {cmd} stdin"))?;
        // Drop stdin to close the pipe so tools that read to EOF (wl-copy,
        // pbcopy) finish rather than hanging.
    }
    let status = child.wait().with_context(|| format!("waiting for {cmd}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow::anyhow!("{cmd} exited with {status}"))
    }
}

fn restore_terminal() -> Result<()> {
    // Pop the keyboard-enhancement flags pushed in `setup_terminal`. Done
    // unconditionally (and ignoring errors): popping an empty/absent stack is a
    // no-op on supporting terminals and an ignored escape elsewhere, which is
    // safer than re-querying support from a panic/teardown path.
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::event::PopKeyboardEnhancementFlags,
    );
    crossterm::execute!(
        std::io::stdout(),
        crossterm::event::DisableMouseCapture,
        crossterm::event::DisableBracketedPaste,
        crossterm::terminal::LeaveAlternateScreen,
    )
    .context("leaving alternate screen")?;
    crossterm::terminal::disable_raw_mode().context("disabling raw mode")?;
    Ok(())
}

/// Restore the terminal if (and only if) raw mode is active. Safe to call
/// from a panic hook or after a headless run — it does nothing when the TUI
/// never started.
pub fn restore_terminal_best_effort() {
    if is_terminal_armed() {
        let _ = restore_terminal();
    }
}

/// Whether the terminal is still in the state [`setup_terminal`] put it in.
///
/// Raw mode is the marker for the whole arrangement: it goes on first and
/// comes off last, and every teardown path in this module (and the panic hook
/// in `main`) moves it in lockstep with the alternate screen, mouse capture,
/// and bracketed paste. The event loop polls this every frame because the
/// panic hook can strip all of it while the process keeps running — see
/// [`crate::app::recover::TerminalWatchdog`] for why that happens and what the
/// loop does about it.
pub(super) fn is_terminal_armed() -> bool {
    crossterm::terminal::is_raw_mode_enabled().unwrap_or(false)
}

/// Restores the terminal when the main loop unwinds or errors out.
pub(super) struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal_best_effort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prompt_scratch_file_is_private_and_not_in_the_shared_temp_dir() {
        // The draft is unsent user text and the file name is derivable from the
        // pid, so a world-writable directory is both a disclosure and a
        // symlink-planting opportunity. Pin the directory, not just the name.
        let path = prompt_scratch_path().expect("scratch path");
        let dir = path.parent().expect("scratch parent");
        assert_eq!(dir.file_name().and_then(|n| n.to_str()), Some("scratch"));
        assert_eq!(
            dir.parent(),
            Some(
                crate::platform::paths::state_dir()
                    .expect("state dir")
                    .as_path()
            ),
            "the scratch file must live under Wizard's own state tree"
        );
        assert_ne!(
            dir,
            crate::platform::paths::temp_dir(),
            "the shared temp dir is world-writable"
        );
        assert!(
            crate::platform::secrets::is_protected(dir).expect("stat the scratch dir"),
            "{} must be owner-only",
            dir.display()
        );

        // And the file itself, which is what actually holds the text: a 0700
        // parent is the lock that matters, but a draft written at the process
        // umask is one `chmod` on the directory away from being world-readable.
        stage_draft(&path, "an unsent prompt with a pasted key in it").expect("stage the draft");
        assert!(
            crate::platform::secrets::is_protected(&path).expect("stat the draft"),
            "{} must be owner-only",
            path.display()
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("read the draft"),
            "an unsent prompt with a pasted key in it"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn abandoned_drafts_are_swept_and_live_ones_are_not() {
        // `/tmp` had a reaper; `~/.wizard/scratch` does not, so a session that
        // died between staging the draft and reading it back used to leave an
        // unsent prompt on disk for the life of the install, one file per pid.
        let dir = tempfile::tempdir().expect("tempdir");
        let old = dir.path().join(format!("{DRAFT_PREFIX}101.md"));
        let fresh = dir.path().join(format!("{DRAFT_PREFIX}102.md"));
        let unrelated = dir.path().join("notes.md");
        for path in [&old, &fresh, &unrelated] {
            std::fs::write(path, "draft").expect("write");
        }
        let ancient = std::time::SystemTime::now() - std::time::Duration::from_secs(30 * 86_400);
        for path in [&old, &unrelated] {
            std::fs::File::options()
                .write(true)
                .open(path)
                .expect("open")
                .set_modified(ancient)
                .expect("backdate");
        }

        sweep_stale_drafts(dir.path(), DRAFT_TTL);

        assert!(!old.exists(), "an abandoned draft must be swept");
        assert!(fresh.exists(), "a draft in use must survive");
        assert!(
            unrelated.exists(),
            "the sweep must only touch its own file names"
        );

        // A sweep that ran with the shipped TTL against files this new would
        // delete a draft the user is editing right now.
        sweep_stale_drafts(dir.path(), std::time::Duration::from_secs(0));
        assert!(!fresh.exists(), "the age threshold is what decides");
        assert!(unrelated.exists());
    }

    #[test]
    fn the_editor_runs_as_a_command_line_not_a_program_name() {
        // `$EDITOR` is routinely "code --wait" or "emacsclient -t", so the
        // whole string has to reach a shell as one argument; splitting it (or
        // passing it to `Command::new`) would look for a binary with a space in
        // its name. Built through the same function `run_editor_suspended`
        // calls, so reverting that call to `Command::new(editor).arg(path)`
        // fails here.
        let command = editor_command("code --wait", std::path::Path::new("/some/draft.md"));
        let program = command.get_program().to_string_lossy().into_owned();
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_ne!(
            program, "code --wait",
            "the editor setting is not a program name"
        );
        assert_eq!(program, crate::platform::shell::name());
        assert_eq!(args.len(), 2, "expected <flag> <line>, got {args:?}");
        assert_eq!(args[1], "code --wait \"/some/draft.md\"");
    }
}
