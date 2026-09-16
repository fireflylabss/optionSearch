//! `optionsearch-gtk` — GTK/libadwaita desktop search window over the local optionSearch index.
//!
//! Running `optionsearch-gtk` with no arguments opens the search window. Any command-line
//! argument is routed to the shared CLI (`optionsearch` / `nld` compat), so one binary keeps both the
//! Everything-style launcher and the headless toolchain.

mod desktop;

use anyhow::Result;
use optionsearch_cli::commands;

fn main() {
    match run() {
        Ok(true) => {}
        Ok(false) => {}
        Err(error) => {
            eprintln!("optionsearch-gtk: {error}");
            for cause in error.chain().skip(1) {
                eprintln!("  ↳ {cause}");
            }
            std::process::exit(1);
        }
    }
}

fn run() -> Result<bool> {
    // `--daemon` runs the index watcher headless (no window); a later plain
    // launch activates the same process and shows it. Everything else goes
    // through the shared CLI dispatch.
    let daemon = std::env::args_os()
        .skip(1)
        .any(|a| a.to_str() == Some("--daemon") || a.to_str() == Some("daemon"));
    if daemon {
        desktop::run_daemon()?;
        return Ok(true);
    }
    // Argument dispatch returns Ok(false) when no arguments were given.
    if commands::dispatch("optionsearch-gtk")? {
        return Ok(true);
    }
    desktop::run()?;
    Ok(true)
}
