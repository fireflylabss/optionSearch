//! Runs the watcher against a real tree and reports what it registered.
//!
//! ```text
//! cargo run --release --example watch_home -- [root] [seconds]
//! ```
//!
//! Uses a scratch copy of the index database (`$NEEDLE_DB`, defaulting to
//! `/tmp/optionsearch-watch-example.db`) so it never fights the real app for the file.

use std::sync::Arc;
use std::time::{Duration, Instant};

use optionsearch_core::{Config, Engine, Watcher};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let root = args
        .next()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(option_sdk::home_dir);
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(20);

    let db_path = std::env::var_os("NEEDLE_DB")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp/optionsearch-watch-example.db"));
    let config = Config {
        roots: vec![root.clone()],
        db_path,
        ..Config::default()
    };

    let engine = Arc::new(Engine::open(config)?);
    if engine.is_empty() {
        println!("index empty, scanning {} ...", root.display());
        let stats = engine.reindex()?;
        println!("scanned {} entries in {:?}", stats.total(), stats.elapsed);
    }
    let stats = engine.stats();
    println!(
        "index: {} entries, {} dirs, {:.0} MB resident",
        stats.live_entries,
        stats.dirs,
        stats.memory_bytes as f64 / 1e6
    );

    let mut opts = optionsearch_core::WatchOptions::from_config(&engine.config());
    if let Some(budget) = std::env::var("NEEDLE_WATCH_BUDGET")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        opts.budget = budget;
        opts.rescan_interval = Duration::from_secs(10);
    }
    let started = Instant::now();
    let watcher = Watcher::start_with(engine.clone(), opts)?;
    while watcher.status().registering {
        std::thread::sleep(Duration::from_millis(50));
    }
    let status = watcher.status();
    println!(
        "registered {} watches in {:?} (degraded: {}, unwatched subtrees: {})",
        status.watched_dirs,
        started.elapsed(),
        status.degraded,
        status.unwatched_total
    );
    if let Some(reason) = &status.reason {
        println!("  reason: {reason}");
    }
    for p in &status.unwatched_roots {
        println!("  unwatched: {}", p.display());
    }

    // Round-trip a file through the watcher and time how long the index takes
    // to catch up.
    let probe = root.join(format!(
        "optionsearch-watch-probe-{}.tmp",
        std::process::id()
    ));
    let name = probe.file_name().unwrap().to_string_lossy().into_owned();
    std::fs::write(&probe, b"probe")?;
    println!("create seen after {:?}", wait_for(&engine, &name, true));
    std::fs::remove_file(&probe)?;
    println!("delete seen after {:?}", wait_for(&engine, &name, false));

    let generation = engine.generation();
    println!("watching for {seconds}s ...");
    let until = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < until {
        std::thread::sleep(Duration::from_secs(5));
        let s = watcher.status();
        println!(
            "  t+{:>3.0}s  events={} batches={} rescans={}",
            started.elapsed().as_secs_f64(),
            s.events_applied,
            s.batches,
            s.rescans
        );
    }
    let status = watcher.status();
    println!(
        "{} events applied in {} batches, {} rescans, generation {} -> {}",
        status.events_applied,
        status.batches,
        status.rescans,
        generation,
        engine.generation()
    );
    watcher.shutdown();
    Ok(())
}

fn wait_for(engine: &Engine, name: &str, present: bool) -> Duration {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(30) {
        if engine.search_uncached(name, 1).hits.is_empty() != present {
            return started.elapsed();
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    started.elapsed()
}
