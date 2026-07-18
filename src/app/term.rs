//! Terminal lifecycle: raw mode + alternate screen setup/teardown, editor
//! suspension (`$EDITOR`), and the OSC 52 clipboard write.

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
    // covered text is copied to the clipboard (OSC 52) on release (see the
    // Down/Drag/Up handlers in `handle_event` and the highlight overlay in
    // `crate::ui`). Holding Shift still forces the terminal's own selection as
    // a fallback. Bracketed paste stays on so pasted text lands in the composer
    // as one chunk.
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

/// Copy `text` to the system clipboard with the OSC 52 terminal escape. This
/// needs no clipboard daemon and works over SSH, as long as the terminal
/// supports OSC 52 (most modern ones do). The sequence is written straight to
/// stdout — it's non-printing, so it doesn't disturb the rendered frame.
pub(super) fn copy_to_clipboard(text: &str) -> Result<()> {
    use base64::Engine;
    use std::io::Write;

    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let mut stdout = std::io::stdout();
    // OSC 52: ESC ] 52 ; c ; <base64> BEL  — `c` targets the clipboard.
    write!(stdout, "\x1b]52;c;{encoded}\x07").context("writing clipboard escape")?;
    stdout.flush().context("flushing clipboard escape")?;
    Ok(())
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
