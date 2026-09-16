use anyhow::Result;
use optionsearch_cli::commands;

fn main() {
    if let Err(error) = run() {
        eprintln!("optionsearch: {error}");
        for cause in error.chain().skip(1) {
            eprintln!("  ↳ {cause}");
        }
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    commands::dispatch("optionsearch")?;
    Ok(())
}
