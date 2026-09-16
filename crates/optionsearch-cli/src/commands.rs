//! Command implementations for the optionSearch CLI.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{CommandFactory, FromArgMatches};
use optionsearch_core::{Config, Engine, Preview, Watcher, preview::mode_string};

use crate::cli::Command;

/// optionSearch's Option-family identity: data lives under `~/.option/search`.
pub fn app() -> option_sdk::App {
    option_sdk::App::SEARCH
}

/// Migrates the legacy `~/.option/needle` tree into `~/.option/search` once,
/// then ensures the app directory exists.
///
/// Shared entry point for the CLI (`config_with`) and the desktop
/// (`open_engine`). Surfacing migration failures instead of swallowing them;
/// a missing source or existing destination is a no-op via `migrate_dir`.
pub fn prepare_state() -> Result<option_sdk::App> {
    let sdk = app();
    option_sdk::migrate_dir(&option_sdk::option_root().join("needle"), &sdk.dir())
        .context("failed to migrate ~/.option/needle to ~/.option/search")?;
    sdk.ensure().context("failed to prepare ~/.option/search")?;
    Ok(sdk)
}

/// Default benchmark query set, exercising plain terms, filters and regexes.
const DEFAULT_BENCH: &[&str] = &[
    "a",
    "png",
    "config",
    "index.rs",
    "cargo.toml",
    "report",
    "ext:pdf",
    "ext:jpg,png size:>1mb",
    "src/main",
    "\"my file\"",
    r"re:^IMG_\d+",
    "dir: node_modules",
    "zzzqqq",
];

pub fn run(cmd: Command, json: bool) -> Result<()> {
    match cmd {
        Command::Index { roots } => index(roots),
        Command::Add { path } => add(path),
        Command::Watch { roots, daemon } => watch(roots, daemon),
        Command::Search {
            query,
            limit,
            preview,
        } => search(query.join(" "), limit, preview, json),
        Command::Stats => stats(),
        Command::Preview { path, lines } => file_preview(&path, lines),
        Command::Bench {
            query,
            runs,
            limit,
            typing,
        } => bench(query, runs, limit, typing),
        Command::Clear => clear(),
    }
}

fn config_with(roots: Vec<std::path::PathBuf>) -> Result<Config> {
    let sdk = prepare_state()?;
    let mut config = Config {
        db_path: sdk.path("index.sqlite3"),
        cache_dir: sdk.path("cache"),
        ..Config::default()
    };
    if !roots.is_empty() {
        config.roots = roots;
    }
    Ok(config)
}

fn index(roots: Vec<std::path::PathBuf>) -> Result<()> {
    let engine = Engine::open(config_with(roots)?)?;
    println!(
        "⌕ indexing {}",
        engine
            .config()
            .roots
            .iter()
            .map(|r| r.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    let stats = engine.reindex()?;
    let e = engine.stats();
    println!(
        "indexed {} entries ({} dirs, {} files, {} errors) in {:.2?}",
        stats.total(),
        stats.dirs,
        stats.files,
        stats.errors,
        stats.elapsed
    );
    println!(
        "memory {:.1} MiB  |  names {:.1} MiB  |  db {:.1} MiB  |  rss {:.1} MiB peak",
        mib(e.memory_bytes as u64),
        mib(e.name_bytes as u64),
        mib(e.db_bytes),
        peak_rss().map_or(0.0, mib)
    );
    Ok(())
}

/// Mirrors the desktop's "Add folder": registers the path as a root, scans
/// just it into the live index and persists the new root set.
fn add(path: PathBuf) -> Result<()> {
    if !path.is_dir() {
        anyhow::bail!("{} is not a directory", path.display());
    }
    let engine = Engine::open(config_with(Vec::new())?)?;
    engine
        .add_root(path.clone())
        .with_context(|| format!("could not add {}", path.display()))?;
    println!(
        "⌕ added {} · {} entries in the index",
        path.display(),
        engine.stats().live_entries
    );
    Ok(())
}

fn watch(roots: Vec<std::path::PathBuf>, daemon: bool) -> Result<()> {
    let engine = Arc::new(Engine::open(config_with(roots)?)?);
    if engine.is_empty() {
        let stats = engine.reindex()?;
        println!("⌕ {} entries indexed, watching for changes", stats.total());
    }
    let _watcher = Watcher::start(engine)?;
    if !daemon {
        println!("⌕ watching with inotify · Ctrl+C to stop");
    }
    loop {
        std::thread::park();
    }
}

fn search(query: String, limit: usize, preview: bool, json: bool) -> Result<()> {
    let engine = Engine::open(config_with(Vec::new())?)?;
    let r = engine.search(&query, limit);
    if json {
        for item in &r.hits {
            println!(
                "{{\"path\":{:?},\"size\":{},\"modified\":{},\"directory\":{}}}",
                item.path, item.size, item.mtime, item.is_dir
            );
        }
    } else {
        for item in &r.hits {
            println!(
                "{:>4}  {:>10}  {}{}",
                item.score,
                human(item.size),
                item.path.display(),
                if item.is_dir { "/" } else { "" }
            );
        }
        eprintln!(
            "{} hits{} of {} entries in {:.3} ms",
            r.total,
            if r.truncated { " (truncated)" } else { "" },
            r.scanned,
            ms(r.elapsed)
        );
    }
    if preview {
        if let Some(hit) = r.hits.first() {
            print_preview(&engine, &hit.path, 40);
        }
    }
    Ok(())
}

fn stats() -> Result<()> {
    let engine = Engine::open(config_with(Vec::new())?)?;
    let s = engine.stats();
    println!(
        "⌕ optionSearch\n  index    {}",
        app().path("index.sqlite3").display()
    );
    println!(
        "  roots    {}",
        s.roots
            .iter()
            .map(|r| r.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!("  files    {} live / {} slots", s.live_entries, s.entries);
    println!(
        "  memory   {:.1} MiB (names {:.1} MiB)",
        mib(s.memory_bytes as u64),
        mib(s.name_bytes as u64)
    );
    println!("  database {:.1} MiB", mib(s.db_bytes));
    println!("  bundle   {}", app().bundle_id());
    println!("  last scan {}", s.last_scan.as_deref().unwrap_or("never"));
    Ok(())
}

fn file_preview(path: &Path, lines: usize) -> Result<()> {
    let engine = Engine::open(config_with(Vec::new())?)?;
    print_preview(&engine, path, lines);
    Ok(())
}

fn print_preview(engine: &Engine, path: &Path, max_lines: usize) {
    println!("\n--- preview: {} ---", path.display());
    match engine.preview(path) {
        Preview::Meta(m) => {
            println!("kind   {}", m.kind);
            println!("size   {}", human(m.size));
            println!("mode   {} ({:o})", mode_string(m.mode), m.mode & 0o7777);
            println!("owner  {}", m.owner);
            println!("mtime  {}", m.mtime);
        }
        Preview::Text {
            content,
            truncated,
            lines,
        } => {
            let shown = content.lines().take(max_lines).collect::<Vec<_>>();
            println!(
                "{lines} lines{}",
                if truncated { " (truncated)" } else { "" }
            );
            println!("{}", shown.join("\n"));
        }
        Preview::Image {
            path,
            width,
            height,
        } => {
            println!("image {width}x{height} · {}", path.display());
        }
        Preview::Pdf { pages, text, .. } => {
            println!(
                "pdf, {pages} pages\n{}",
                text.lines().take(max_lines).collect::<Vec<_>>().join("\n")
            );
        }
        Preview::Audio { path } => println!("audio · {}", path.display()),
        Preview::Error(e) => println!("error: {e}"),
    }
}

fn bench(query: Vec<String>, runs: usize, limit: usize, typing: bool) -> Result<()> {
    let engine = Engine::open(config_with(Vec::new())?)?;
    let s = engine.stats();
    println!(
        "index: {} entries, {} dirs, {:.1} MiB resident\n",
        s.live_entries,
        s.dirs,
        mib(s.memory_bytes as u64)
    );
    let queries: Vec<String> = if query.is_empty() {
        DEFAULT_BENCH.iter().map(|s| s.to_string()).collect()
    } else {
        query
    };
    if typing {
        for q in &queries {
            println!("typing {q:?}");
            let mut prefix = String::new();
            for ch in q.chars() {
                prefix.push(ch);
                let started = Instant::now();
                let r = engine.search(&prefix, limit);
                println!(
                    "  {:<28} {:>8.3} ms {:>10} hits",
                    prefix,
                    ms(started.elapsed()),
                    r.total
                );
            }
        }
        return Ok(());
    }
    println!(
        "{:<28} {:>8} {:>8} {:>8} {:>8} {:>10}",
        "query", "min ms", "p50 ms", "p95 ms", "max ms", "hits"
    );
    let mut worst = Duration::ZERO;
    for q in &queries {
        let mut times = Vec::with_capacity(runs);
        let mut hits = 0;
        for _ in 0..runs {
            let started = Instant::now();
            let r = engine.search_uncached(q, limit);
            times.push(started.elapsed());
            hits = r.total;
        }
        times.sort();
        let p = |f: f64| times[((times.len() - 1) as f64 * f) as usize];
        worst = worst.max(p(0.95));
        println!(
            "{:<28} {:>8.3} {:>8.3} {:>8.3} {:>8.3} {:>10}",
            truncate(q, 28),
            ms(times[0]),
            ms(p(0.5)),
            ms(p(0.95)),
            ms(*times.last().unwrap()),
            hits
        );
    }
    println!("\nworst p95: {:.3} ms", ms(worst));
    Ok(())
}

fn clear() -> Result<()> {
    let engine = Engine::open(config_with(Vec::new())?)?;
    std::fs::remove_file(&engine.config().db_path).ok();
    println!("⌕ index cleared; restart optionSearch to create a fresh index");
    Ok(())
}

/// Parse and run CLI arguments under `program`'s name.
///
/// Returns `Ok(false)` when there were no arguments at all, so a desktop entry
/// point can fall through to opening its window; returns `Ok(true)` when an
/// argument was handled.
pub fn dispatch(program: &'static str) -> Result<bool> {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if args.len() <= 1 {
        return Ok(false);
    }
    let mut cmd = crate::cli::Cli::command();
    cmd = cmd.name(program);
    let cli = crate::cli::Cli::from_arg_matches(&cmd.clone().get_matches_from(args))?;
    match cli.command {
        Some(cmd) => run(cmd, cli.json)?,
        None => {
            cmd.print_help()?;
        }
    }
    Ok(true)
}

/// Peak resident set size of this process, from `/proc/self/status`.
fn peak_rss() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmHWM:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n - 1).collect::<String>() + "…"
    }
}
