//! Binary entry point: parse arguments, install a terminal-restoring panic
//! hook, and hand off to the library runner. Routing (genie TUI, sovereign
//! headless, `--evolve`) happens in [`wizard::run`].

use clap::Parser;

use wizard::cli::Cli;

#[tokio::main]
async fn main() {
    // If the TUI is up when something panics, raw mode and the alternate
    // screen must be torn down before the panic message prints, or the
    // terminal is left unusable.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        wizard::app::restore_terminal_best_effort();
        default_hook(info);
    }));

    let cli = Cli::parse();
    if let Err(err) = wizard::run(cli).await {
        // Make sure the error lands on a sane terminal even when the TUI
        // errored out mid-frame.
        wizard::app::restore_terminal_best_effort();
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}
