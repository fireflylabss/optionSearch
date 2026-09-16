use super::*;

use notify::event::{CreateKind, RemoveKind};

// ---- translation and coalescing -----------------------------------------

fn ev(kind: EventKind, paths: &[&str]) -> Event {
    Event {
        kind,
        paths: paths.iter().map(PathBuf::from).collect(),
        attrs: Default::default(),
    }
}

fn renamed(mode: RenameMode, tracker: usize, paths: &[&str]) -> Event {
    ev(EventKind::Modify(ModifyKind::Name(mode)), paths).set_tracker(tracker)
}

#[test]
fn creates_win_over_modifies_for_the_same_path() {
    let plan = coalesce(&[
        ev(
            EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Content)),
            &["/r/a"],
        ),
        ev(EventKind::Create(CreateKind::Folder), &["/r/a"]),
        ev(
            EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Content)),
            &["/r/a"],
        ),
    ]);
    assert_eq!(plan.touches.len(), 1);
    assert_eq!(plan.touches[Path::new("/r/a")], Touch::Rescan);
}

#[test]
fn a_window_of_events_collapses_to_one_touch_per_path() {
    let mut events = Vec::new();
    for _ in 0..500 {
        events.push(ev(
            EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Content)),
            &["/r/log.txt"],
        ));
    }
    events.push(ev(EventKind::Create(CreateKind::File), &["/r/other"]));
    let plan = coalesce(&events);
    assert_eq!(plan.touches.len(), 2);
    assert_eq!(plan.touches[Path::new("/r/log.txt")], Touch::Check);
}

#[test]
fn rename_pairs_are_correlated_by_tracker() {
    let plan = coalesce(&[
        renamed(RenameMode::From, 7, &["/r/old"]),
        renamed(RenameMode::To, 7, &["/r/new"]),
        renamed(RenameMode::Both, 7, &["/r/old", "/r/new"]),
    ]);
    assert_eq!(plan.pairs, 1);
    assert_eq!(plan.halves, 0);
    assert_eq!(plan.touches[Path::new("/r/old")], Touch::Check);
    assert_eq!(plan.touches[Path::new("/r/new")], Touch::Rescan);
}

#[test]
fn a_half_rename_still_touches_its_side() {
    let out = coalesce(&[renamed(RenameMode::From, 9, &["/r/gone"])]);
    assert_eq!((out.pairs, out.halves), (0, 1));
    assert_eq!(out.touches[Path::new("/r/gone")], Touch::Check);

    let into = coalesce(&[renamed(RenameMode::To, 11, &["/r/arrived"])]);
    assert_eq!((into.pairs, into.halves), (0, 1));
    assert_eq!(into.touches[Path::new("/r/arrived")], Touch::Rescan);
}

#[test]
fn removals_and_access_events() {
    let plan = coalesce(&[
        ev(EventKind::Remove(RemoveKind::File), &["/r/x"]),
        ev(
            EventKind::Access(notify::event::AccessKind::Read),
            &["/r/y"],
        ),
    ]);
    assert_eq!(plan.touches.len(), 1);
    assert_eq!(plan.touches[Path::new("/r/x")], Touch::Check);
}

#[test]
fn queue_overflow_is_detected() {
    let mut e = ev(EventKind::Create(CreateKind::File), &["/r/a"]);
    e.attrs.set_flag(notify::event::Flag::Rescan);
    assert!(coalesce(&[e]).overflow);
}

#[test]
fn path_bytes_round_trip() {
    assert_eq!(path_from_bytes(b"/"), PathBuf::from("/"));
    assert_eq!(path_from_bytes(b"/home/x/"), PathBuf::from("/home/x"));
}

// ---- live filesystem -----------------------------------------------------

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    engine: Arc<Engine>,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let engine = Arc::new(
        Engine::open(Config {
            roots: vec![root.clone()],
            db_path: tmp.path().join("index.db"),
            cache_dir: tmp.path().join("cache"),
            ..Config::default()
        })
        .unwrap(),
    );
    engine.reindex().unwrap();
    Fixture {
        _tmp: tmp,
        root,
        engine,
    }
}

fn options(root: &Path) -> WatchOptions {
    WatchOptions {
        roots: vec![root.to_path_buf()],
        excludes: vec!["node_modules".to_string()],
        budget: 10_000,
        debounce: Duration::from_millis(50),
        rescan_interval: Duration::from_millis(150),
    }
}

/// Polls until `check` passes. Filesystem notifications are asynchronous, so a
/// deadline is the only sane way to assert on them.
fn eventually(engine: &Engine, what: &str, mut check: impl FnMut(&Engine) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if check(engine) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what}");
}

fn found(engine: &Engine, needle: &str) -> bool {
    !engine.search_uncached(needle, 10).hits.is_empty()
}

/// Registration runs on the worker thread, so tests must not race it.
fn ready(watcher: &Watcher) -> WatchStatus {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let status = watcher.status();
        if !status.registering {
            return status;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("watcher never finished registering");
}

#[test]
fn watcher_tracks_creates_writes_renames_and_deletes() {
    let fx = fixture();
    let watcher = Watcher::start_with(fx.engine.clone(), options(&fx.root)).unwrap();
    assert!(ready(&watcher).watched_dirs >= 1);

    std::fs::write(fx.root.join("alpha.txt"), b"hello").unwrap();
    eventually(&fx.engine, "alpha.txt to be indexed", |e| {
        found(e, "alpha.txt")
    });

    std::fs::write(fx.root.join("alpha.txt"), b"hello, longer body").unwrap();
    eventually(&fx.engine, "the new size", |e| {
        e.search_uncached("alpha.txt", 10).hits[0].size == 18
    });

    std::fs::rename(fx.root.join("alpha.txt"), fx.root.join("beta.txt")).unwrap();
    eventually(&fx.engine, "the rename", |e| {
        found(e, "beta.txt") && !found(e, "alpha.txt")
    });

    std::fs::remove_file(fx.root.join("beta.txt")).unwrap();
    eventually(&fx.engine, "the delete", |e| !found(e, "beta.txt"));

    let status = watcher.status();
    assert!(status.events_applied > 0);
    assert!(status.last_event.is_some());
    assert!(!status.degraded, "{status:?}");
}

#[test]
fn a_directory_populated_before_its_watch_lands_is_indexed_anyway() {
    let fx = fixture();
    let watcher = Watcher::start_with(fx.engine.clone(), options(&fx.root)).unwrap();
    ready(&watcher);

    // Build the tree elsewhere and move it in, which is the worst case: the
    // whole subtree exists before a single event is delivered.
    let staging = fx.root.parent().unwrap().join("staging");
    std::fs::create_dir_all(staging.join("pkg/src/deep")).unwrap();
    std::fs::write(staging.join("pkg/src/deep/buried.rs"), b"fn main() {}").unwrap();
    std::fs::write(staging.join("pkg/README.md"), b"# hi").unwrap();
    std::fs::rename(staging.join("pkg"), fx.root.join("pkg")).unwrap();

    eventually(&fx.engine, "the moved subtree", |e| {
        found(e, "buried.rs") && found(e, "README.md")
    });

    // And a file created inside the newly discovered directory is watched too.
    std::fs::write(fx.root.join("pkg/src/deep/later.rs"), b"//").unwrap();
    eventually(&fx.engine, "a file in a newly watched dir", |e| {
        found(e, "later.rs")
    });
}

#[test]
fn removing_a_directory_drops_its_subtree() {
    let fx = fixture();
    std::fs::create_dir_all(fx.root.join("junk/inner")).unwrap();
    std::fs::write(fx.root.join("junk/inner/trash.bin"), b"x").unwrap();
    fx.engine.reindex().unwrap();
    assert!(found(&fx.engine, "trash.bin"));

    let watcher = Watcher::start_with(fx.engine.clone(), options(&fx.root)).unwrap();
    ready(&watcher);
    std::fs::remove_dir_all(fx.root.join("junk")).unwrap();
    eventually(&fx.engine, "the subtree removal", |e| {
        !found(e, "trash.bin") && !found(e, "junk")
    });
}

#[test]
fn excluded_directories_are_indexed_but_not_watched() {
    let fx = fixture();
    std::fs::create_dir_all(fx.root.join("node_modules/left-pad")).unwrap();
    std::fs::create_dir_all(fx.root.join("src")).unwrap();
    fx.engine.reindex().unwrap();

    let watcher = Watcher::start_with(fx.engine.clone(), options(&fx.root)).unwrap();
    // root + src, but neither node_modules nor anything below it.
    let status = ready(&watcher);
    assert_eq!(status.watched_dirs, 2);
    assert!(!status.degraded);
}

#[test]
fn an_exhausted_budget_degrades_to_periodic_rescan() {
    let fx = fixture();
    std::fs::create_dir_all(fx.root.join("far/away")).unwrap();
    fx.engine.reindex().unwrap();

    let opts = WatchOptions {
        budget: 1,
        ..options(&fx.root)
    };
    let watcher = Watcher::start_with(fx.engine.clone(), opts).unwrap();
    ready(&watcher);
    eventually(&fx.engine, "the watcher to report degradation", |_| {
        watcher.status().degraded
    });
    let status = watcher.status();
    assert_eq!(status.watched_dirs, 1);
    assert_eq!(status.unwatched_roots, vec![fx.root.join("far")]);
    assert!(status.reason.is_some(), "{status:?}");

    // No inotify watch covers this file, so only the periodic diff can find it.
    std::fs::write(fx.root.join("far/away/hidden.dat"), b"x").unwrap();
    eventually(&fx.engine, "the rescan to pick up the file", |e| {
        found(e, "hidden.dat")
    });
    // And the same diff notices deletions.
    std::fs::remove_file(fx.root.join("far/away/hidden.dat")).unwrap();
    eventually(&fx.engine, "the rescan to notice the delete", |e| {
        !found(e, "hidden.dat")
    });
    assert!(watcher.status().rescans > 0);
}

#[test]
fn changes_survive_a_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let config = Config {
        roots: vec![root.clone()],
        db_path: tmp.path().join("index.db"),
        cache_dir: tmp.path().join("cache"),
        ..Config::default()
    };

    let engine = Arc::new(Engine::open(config.clone()).unwrap());
    engine.reindex().unwrap();
    let watcher = Watcher::start_with(engine.clone(), options(&root)).unwrap();
    ready(&watcher);
    std::fs::write(root.join("persisted.txt"), b"x").unwrap();
    eventually(&engine, "the write", |e| found(e, "persisted.txt"));
    watcher.shutdown();
    drop(engine);

    let reopened = Engine::open(config).unwrap();
    assert!(found(&reopened, "persisted.txt"));
}

#[test]
fn shutdown_is_prompt() {
    let fx = fixture();
    let watcher = Watcher::start_with(fx.engine.clone(), options(&fx.root)).unwrap();
    let started = Instant::now();
    watcher.shutdown();
    assert!(started.elapsed() < Duration::from_secs(2));
}
