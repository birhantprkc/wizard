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
    // composer bind it to a newline. Terminals that don't support it are left
    // untouched (Alt+Enter is the fallback there). Popped in `restore_terminal`.
    if crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false) {
        let _ = crossterm::execute!(
            stdout,
            crossterm::event::PushKeyboardEnhancementFlags(
                crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES,
            ),
        );
    }
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
    // `sh -c` so editors with flags ("code --wait", "emacsclient -t") work.
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"{}\"", path.display()))
        .status();

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
                app.mode = app.config.mode;
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

/// Suspend the TUI and open the composer draft in the external editor
/// (Ctrl-G); on a clean exit the edited file replaces the input with the
/// cursor at the end. A nonzero exit leaves the composer untouched. Runs from
/// the main loop because it owns `terminal`.
pub(super) fn edit_prompt_in_editor(app: &mut App, terminal: &mut Tui) {
    let Some(editor) = resolve_editor() else {
        app.notice("no $VISUAL/$EDITOR set and nvim not on PATH — cannot edit the prompt");
        return;
    };

    let path = std::env::temp_dir().join(format!("wizard-prompt-{}.md", std::process::id()));
    if let Err(err) = std::fs::write(&path, &app.input) {
        app.notice(format!("could not stage the prompt: {err:#}"));
        return;
    }

    let Some(status) = run_editor_suspended(app, terminal, &editor, &path) else {
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
    if crossterm::terminal::is_raw_mode_enabled().unwrap_or(false) {
        let _ = restore_terminal();
    }
}

/// Restores the terminal when the main loop unwinds or errors out.
pub(super) struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal_best_effort();
    }
}
