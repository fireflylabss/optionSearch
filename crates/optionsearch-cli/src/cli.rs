use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Instant local-first file search for Linux.
#[derive(Parser, Debug)]
#[command(name = "optionsearch", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
    #[arg(
        global = true,
        long,
        short = 'j',
        help = "Emit newline-delimited JSON search results"
    )]
    pub json: bool,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Rescan the configured roots and rebuild the index.
    Index {
        /// Override the roots to scan.
        #[arg(long = "root")]
        roots: Vec<PathBuf>,
    },
    /// Index a folder and keep it as a root without rescanning the others.
    Add {
        /// Folder to add to the index.
        path: PathBuf,
    },
    /// Keep the index current with inotify.
    Watch {
        /// Override the roots to watch.
        #[arg(long = "root")]
        roots: Vec<PathBuf>,
        /// Run in the background once the index is current (daemon-style).
        #[arg(long)]
        daemon: bool,
    },
    /// Search the indexed paths.
    Search {
        query: Vec<String>,
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
        /// Print a preview card for the first hit.
        #[arg(long)]
        preview: bool,
    },
    /// Show index location, file count and memory usage.
    Stats,
    /// Render a safe terminal preview of a file.
    Preview {
        path: PathBuf,
        #[arg(short, long, default_value_t = 40)]
        lines: usize,
    },
    /// Time a set of queries against the index.
    Bench {
        /// Queries to time (defaults to a mixed built-in set).
        query: Vec<String>,
        #[arg(short, long, default_value_t = 20)]
        runs: usize,
        #[arg(short = 'n', long, default_value_t = 500)]
        limit: usize,
        /// Type each query one character at a time, like the UI does.
        #[arg(long)]
        typing: bool,
    },
    /// Delete every indexed entry (never your files).
    Clear,
}
