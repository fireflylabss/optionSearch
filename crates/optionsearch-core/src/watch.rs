//! Incremental filesystem watching.
//!
//! One inotify watch is registered per directory (`notify`'s recursive mode is
//! not used, because it cannot skip `node_modules` and friends). Raw events are
//! coalesced into a set of *touched paths* over a debounce window and each path
//! is then re-stated once. That single rule covers every case the kernel throws
//! at us — create, modify, delete, and both halves of a rename, whether or not
//! the other half is inside a watched root — because the state on disk at the
//! end of the window is the state the index should converge to.
//!
//! Watches are a finite resource (`fs.inotify.max_user_watches`), so
//! registration runs against a budget and never fails hard: subtrees that could
//! not be watched are recorded in [`WatchStatus::unwatched_roots`] and picked up
//! by a periodic diff instead.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use notify::event::{ModifyKind, RenameMode};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher as NotifyWatcher};

use crate::config::Config;
use crate::engine::{Engine, IndexEvent};
use crate::index::Index;

/// Upper bound on raw events pulled from one debounce window.
const MAX_RAW_EVENTS: usize = 32_768;
/// Events handed to [`Engine::apply_events`] in one call.
const MAX_BATCH: usize = 8_192;
/// Entries a single subtree diff will look at before giving up on precision.
const MAX_RESCAN_ENTRIES: usize = 500_000;
/// Unwatched subtree roots tracked individually before falling back to
/// recording the whole configured root.
const MAX_UNWATCHED_TRACKED: usize = 512;
/// Unwatched roots surfaced through [`Watcher::status`].
const STATUS_UNWATCHED_SHOWN: usize = 32;
/// Longest the worker sleeps without checking the shutdown flag.
const TICK: Duration = Duration::from_millis(200);

#[derive(Debug, Clone)]
pub struct WatchOptions {
    pub roots: Vec<PathBuf>,
    /// Directory names that are indexed but never watched.
    pub excludes: Vec<String>,
    /// Maximum inotify watches this watcher may hold.
    pub budget: usize,
    pub debounce: Duration,
    /// How often unwatched subtrees are diffed while degraded.
    pub rescan_interval: Duration,
}

impl WatchOptions {
    pub fn from_config(config: &Config) -> WatchOptions {
        WatchOptions {
            roots: config.roots.clone(),
            excludes: config.watch_excludes.clone(),
            budget: config.watch_budget,
            debounce: Duration::from_millis(config.watch_debounce_ms),
            rescan_interval: Duration::from_secs(300),
        }
    }
}

/// Snapshot of what the watcher is doing, for the UI's status line.
#[derive(Debug, Clone, Default)]
pub struct WatchStatus {
    /// Still walking the tree registering watches. Registration is done on the
    /// worker thread so that starting the watcher never blocks the UI.
    pub registering: bool,
    pub watched_dirs: usize,
    /// True when part of the tree is covered by periodic rescans instead of
    /// inotify, so the UI can warn that results may lag there.
    pub degraded: bool,
    /// Subtrees that are not watched (capped; see `unwatched_total`).
    pub unwatched_roots: Vec<PathBuf>,
    pub unwatched_total: usize,
    pub last_event: Option<SystemTime>,
    pub events_applied: u64,
    pub batches: u64,
    pub rescans: u64,
    /// Why the watcher degraded, if it did.
    pub reason: Option<String>,
}

#[derive(Default)]
struct Shared {
    stop: AtomicBool,
    status: Mutex<WatchStatus>,
}

/// A running watcher. Dropping it stops the worker thread.
pub struct Watcher {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Watcher {
    /// Starts watching every root in the engine's configuration.
    pub fn start(engine: Arc<Engine>) -> Result<Watcher> {
        let opts = WatchOptions::from_config(&engine.config());
        Watcher::start_with(engine, opts)
    }

    pub fn start_with(engine: Arc<Engine>, opts: WatchOptions) -> Result<Watcher> {
        let (tx, rx) = std::sync::mpsc::channel();
        let notify = notify::recommended_watcher(tx).context("creating the inotify watcher")?;
        let shared = Arc::new(Shared::default());
        shared.status.lock().unwrap().registering = true;
        let worker = Worker {
            engine,
            opts: opts.canonicalized(),
            shared: shared.clone(),
            rx,
            notify,
            watched: 0,
            unwatched: Vec::new(),
            unwatched_total: 0,
            exhausted: false,
            reason: None,
            events_applied: 0,
            batches: 0,
            rescans: 0,
            last_event: None,
            last_rescan: Instant::now(),
        };
        let thread = std::thread::Builder::new()
            .name("optionsearch-watch".into())
            .spawn(move || worker.run())
            .context("spawning the watcher thread")?;
        Ok(Watcher {
            shared,
            thread: Some(thread),
        })
    }

    pub fn status(&self) -> WatchStatus {
        self.shared.status.lock().unwrap().clone()
    }

    /// Asks the worker to stop without waiting for it.
    pub fn stop(&self) {
        self.shared.stop.store(true, Ordering::Release);
    }

    /// Stops the worker and waits for the final flush.
    pub fn shutdown(mut self) {
        self.stop();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl WatchOptions {
    fn canonicalized(mut self) -> WatchOptions {
        self.roots = self
            .roots
            .iter()
            .map(|r| r.canonicalize().unwrap_or_else(|_| r.clone()))
            .collect();
        self
    }
}

// ---- event coalescing ----------------------------------------------------

/// What a touched path needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Touch {
    /// Stat it and upsert or remove accordingly.
    Check,
    /// Same, plus walk it if it turned out to be a directory. Needed because a
    /// directory can be populated before its watch is registered (`git clone`,
    /// `npm install`, or any `mv` of a whole tree).
    Rescan,
}

#[derive(Debug, Default)]
struct Plan {
    touches: BTreeMap<PathBuf, Touch>,
    /// The kernel dropped events; only a rescan can restore consistency.
    overflow: bool,
    /// Renames whose two halves both landed in this window.
    pairs: usize,
    /// Renames with only one half, i.e. moved in or out of a watched root.
    halves: usize,
}

impl Plan {
    fn mark(&mut self, path: &Path, touch: Touch) {
        let slot = self.touches.entry(path.to_path_buf()).or_insert(touch);
        *slot = (*slot).max(touch);
    }
}

fn coalesce(events: &[Event]) -> Plan {
    let mut plan = Plan::default();
    // tracker (inotify cookie) -> (saw From, saw To)
    let mut sides: HashMap<usize, (bool, bool)> = HashMap::new();

    for ev in events {
        if ev.need_rescan() {
            plan.overflow = true;
            continue;
        }
        match ev.kind {
            EventKind::Create(_) => {
                for p in &ev.paths {
                    plan.mark(p, Touch::Rescan);
                }
            }
            EventKind::Remove(_) => {
                for p in &ev.paths {
                    plan.mark(p, Touch::Check);
                }
            }
            EventKind::Modify(ModifyKind::Name(mode)) => {
                let tracker = ev.tracker();
                match mode {
                    RenameMode::From => {
                        if let Some(t) = tracker {
                            sides.entry(t).or_default().0 = true;
                        }
                        for p in &ev.paths {
                            plan.mark(p, Touch::Check);
                        }
                    }
                    RenameMode::To => {
                        if let Some(t) = tracker {
                            sides.entry(t).or_default().1 = true;
                        }
                        for p in &ev.paths {
                            plan.mark(p, Touch::Rescan);
                        }
                    }
                    RenameMode::Both => {
                        if let Some(t) = tracker {
                            sides.insert(t, (true, true));
                        }
                        if let Some(src) = ev.paths.first() {
                            plan.mark(src, Touch::Check);
                        }
                        if let Some(dst) = ev.paths.get(1) {
                            plan.mark(dst, Touch::Rescan);
                        }
                    }
                    // Direction unknown: treat both ends as possibly new.
                    _ => {
                        for p in &ev.paths {
                            plan.mark(p, Touch::Rescan);
                        }
                    }
                }
            }
            EventKind::Modify(_) => {
                for p in &ev.paths {
                    plan.mark(p, Touch::Check);
                }
            }
            EventKind::Any => {
                for p in &ev.paths {
                    plan.mark(p, Touch::Rescan);
                }
            }
            EventKind::Access(_) | EventKind::Other => {}
        }
    }

    for (from, to) in sides.values() {
        if *from && *to {
            plan.pairs += 1;
        } else {
            plan.halves += 1;
        }
    }
    plan
}

// ---- worker --------------------------------------------------------------

struct Worker {
    engine: Arc<Engine>,
    opts: WatchOptions,
    shared: Arc<Shared>,
    rx: Receiver<notify::Result<Event>>,
    notify: RecommendedWatcher,
    watched: usize,
    unwatched: Vec<PathBuf>,
    unwatched_total: usize,
    exhausted: bool,
    reason: Option<String>,
    events_applied: u64,
    batches: u64,
    rescans: u64,
    last_event: Option<SystemTime>,
    last_rescan: Instant,
}

impl Worker {
    fn run(mut self) {
        self.register_roots();
        self.shared.status.lock().unwrap().registering = false;
        self.publish();

        while !self.shared.stop.load(Ordering::Acquire) {
            let mut raw = Vec::new();
            match self.rx.recv_timeout(TICK) {
                Ok(ev) => raw.push(ev),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if !raw.is_empty() {
                let deadline = Instant::now() + self.opts.debounce;
                while let Some(rest) = deadline.checked_duration_since(Instant::now()) {
                    match self.rx.recv_timeout(rest) {
                        Ok(ev) => {
                            raw.push(ev);
                            if raw.len() >= MAX_RAW_EVENTS {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                self.process(raw);
            }
            self.maybe_rescan();
        }
        let _ = self.engine.flush();
    }

    fn process(&mut self, raw: Vec<notify::Result<Event>>) {
        let mut events = Vec::with_capacity(raw.len());
        for r in raw {
            match r {
                Ok(ev) => events.push(ev),
                Err(e) => {
                    if is_budget_error(&e) {
                        self.degrade(&format!("inotify watch limit reached: {e}"));
                    }
                }
            }
        }
        let plan = coalesce(&events);
        self.last_event = Some(SystemTime::now());

        if plan.overflow {
            self.handle_overflow();
            return;
        }

        let mut new_dirs = Vec::new();
        let t = Instant::now();
        let index_events = self.resolve(&plan, &mut new_dirs);
        eprintln!(
            "DBG window: {} raw, {} touches, {} events, resolve {:?}",
            events.len(),
            plan.touches.len(),
            index_events.len(),
            t.elapsed()
        );
        self.apply(&index_events);
        for dir in new_dirs {
            self.watch_dir(&dir);
        }
        self.publish();
    }

    /// Turns touched paths into index events by looking at what is on disk now.
    fn resolve(&self, plan: &Plan, new_dirs: &mut Vec<PathBuf>) -> Vec<IndexEvent> {
        let mut out = Vec::new();
        let mut rescan = Vec::new();
        for (path, touch) in &plan.touches {
            if !self.under_roots(path) {
                continue;
            }
            match std::fs::symlink_metadata(path) {
                Ok(md) => {
                    let is_dir = md.is_dir();
                    out.push(IndexEvent::Upsert {
                        path: path.clone(),
                        size: md.len(),
                        mtime: md.mtime(),
                        is_dir,
                    });
                    // Only a rescan implies the directory is new to us; a plain
                    // check means it is already watched, and re-registering
                    // would double-count it against the budget.
                    if is_dir && *touch == Touch::Rescan {
                        if self.watchable(path) {
                            new_dirs.push(path.clone());
                        }
                        rescan.push(path.clone());
                    }
                }
                // Gone: `RemoveDir` drops the entry and, when the path was a
                // directory, everything below it. It is the right event for a
                // plain file too, so no index lookup is needed here.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    out.push(IndexEvent::RemoveDir { path: path.clone() });
                }
                Err(_) => {}
            }
        }
        self.diff_subtrees(&rescan, new_dirs, &mut out);
        out
    }

    fn apply(&mut self, events: &[IndexEvent]) {
        if events.is_empty() {
            return;
        }
        for chunk in events.chunks(MAX_BATCH) {
            match self.engine.apply_events(chunk) {
                Ok(out) => {
                    self.events_applied += (out.added + out.updated + out.removed) as u64;
                    self.batches += 1;
                }
                Err(e) => self.reason = Some(format!("applying events: {e}")),
            }
        }
        if let Err(e) = self.engine.flush() {
            self.reason = Some(format!("flushing the database: {e}"));
        }
    }

    /// The kernel dropped events, so nothing incremental can be trusted.
    fn handle_overflow(&mut self) {
        self.rescans += 1;
        if let Err(e) = self.engine.reindex() {
            self.reason = Some(format!("rescan after event overflow failed: {e}"));
        }
        self.register_roots();
        self.publish();
    }

    fn maybe_rescan(&mut self) {
        if self.unwatched.is_empty() || self.last_rescan.elapsed() < self.opts.rescan_interval {
            return;
        }
        self.last_rescan = Instant::now();
        self.rescans += 1;
        let roots = self.unwatched.clone();
        let mut events = Vec::new();
        let mut alive = Vec::new();
        for root in roots {
            match std::fs::symlink_metadata(&root) {
                Ok(md) if md.is_dir() => {
                    events.push(IndexEvent::Upsert {
                        path: root.clone(),
                        size: md.len(),
                        mtime: md.mtime(),
                        is_dir: true,
                    });
                    alive.push(root);
                }
                Ok(_) => {}
                Err(_) => events.push(IndexEvent::RemoveDir { path: root }),
            }
        }
        self.diff_subtrees(&alive, &mut Vec::new(), &mut events);
        self.apply(&events);
        self.publish();
    }

    /// Reconciles subtrees with the index: emits upserts for entries that are
    /// new or changed on disk and removals for entries that only exist in the
    /// index.
    ///
    /// All roots are diffed together because the index-side snapshot costs one
    /// pass over every entry; doing that per subtree would make a degraded
    /// rescan of a few hundred subtrees quadratic.
    fn diff_subtrees(
        &self,
        roots: &[PathBuf],
        new_dirs: &mut Vec<PathBuf>,
        out: &mut Vec<IndexEvent>,
    ) {
        if roots.is_empty() {
            return;
        }
        let known = self.engine.with_index(|index| index_snapshot(index, roots));
        let mut seen: HashSet<PathBuf> = HashSet::new();

        for root in roots {
            for (path, size, mtime, is_dir) in walk_subtree(root) {
                if is_dir && self.watchable(&path) {
                    new_dirs.push(path.clone());
                }
                let unchanged = known
                    .as_ref()
                    .and_then(|k| k.get(&path))
                    .is_some_and(|e| e.size == size && e.mtime == mtime && e.is_dir == is_dir);
                if !unchanged {
                    out.push(IndexEvent::Upsert {
                        path: path.clone(),
                        size,
                        mtime,
                        is_dir,
                    });
                }
                if known.is_some() {
                    seen.insert(path);
                }
            }
        }

        let Some(known) = known else { return };
        for (path, entry) in &known {
            if seen.contains(path) {
                continue;
            }
            out.push(if entry.is_dir {
                IndexEvent::RemoveDir { path: path.clone() }
            } else {
                IndexEvent::Remove { path: path.clone() }
            });
        }
    }

    // ---- watch registration ---------------------------------------------

    fn register_roots(&mut self) {
        self.watched = 0;
        self.unwatched.clear();
        self.unwatched_total = 0;
        self.exhausted = false;

        for root in self.opts.roots.clone() {
            if self.shared.stop.load(Ordering::Acquire) {
                return;
            }
            if !root.is_dir() {
                continue;
            }
            let dirs = dirs_from_index(&self.engine, &root, &self.opts.excludes)
                .unwrap_or_else(|| dirs_from_fs(&root, &self.opts.excludes));
            self.register(&root, dirs);
        }
    }

    /// Registers a pre-order list of `(dir, depth)`. When the budget runs out the
    /// current node is recorded as an unwatched subtree root and its whole
    /// subtree is skipped, which keeps the recorded list to the frontier rather
    /// than every directory below it.
    fn register(&mut self, root: &Path, dirs: Vec<(PathBuf, u16)>) {
        let mut i = 0;
        while i < dirs.len() {
            // Registering a real $HOME takes seconds; shutdown must not wait for
            // it to finish.
            if self.shared.stop.load(Ordering::Acquire) {
                return;
            }
            let (path, depth) = &dirs[i];
            if self.exhausted || self.watched >= self.opts.budget {
                if !self.exhausted {
                    self.degrade(&format!(
                        "watch budget of {} directories exhausted",
                        self.opts.budget
                    ));
                    self.exhausted = true;
                }
                self.record_unwatched(root, path);
                i += 1;
                while i < dirs.len() && dirs[i].1 > *depth {
                    i += 1;
                }
                continue;
            }
            match self.notify.watch(path, RecursiveMode::NonRecursive) {
                Ok(()) => {
                    self.watched += 1;
                    i += 1;
                }
                Err(e) if is_budget_error(&e) => {
                    self.degrade(&format!("inotify refused more watches: {e}"));
                    self.exhausted = true;
                }
                // Vanished or unreadable: skip it, its children will fail too but
                // that costs nothing.
                Err(_) => i += 1,
            }
        }
    }

    fn watch_dir(&mut self, path: &Path) {
        if self.exhausted || self.watched >= self.opts.budget {
            self.record_unwatched(path, path);
            return;
        }
        match self.notify.watch(path, RecursiveMode::NonRecursive) {
            Ok(()) => self.watched += 1,
            Err(e) if is_budget_error(&e) => {
                self.degrade(&format!("inotify refused more watches: {e}"));
                self.exhausted = true;
                self.record_unwatched(path, path);
            }
            Err(_) => {}
        }
    }

    fn record_unwatched(&mut self, root: &Path, path: &Path) {
        self.unwatched_total += 1;
        if self.unwatched.len() < MAX_UNWATCHED_TRACKED {
            if !self.unwatched.iter().any(|p| path.starts_with(p)) {
                self.unwatched.push(path.to_path_buf());
            }
        } else if self.unwatched.last().map(PathBuf::as_path) != Some(root) {
            // Too fragmented to track precisely; fall back to the whole root.
            self.unwatched.clear();
            self.unwatched.push(root.to_path_buf());
        }
    }

    fn degrade(&mut self, reason: &str) {
        if self.reason.is_none() {
            self.reason = Some(reason.to_string());
        }
    }

    // ---- helpers ---------------------------------------------------------

    fn under_roots(&self, path: &Path) -> bool {
        self.opts.roots.iter().any(|r| path.starts_with(r))
    }

    /// Excludes are matched only below the root, so a root that happens to sit
    /// inside a directory called `target` still gets watched.
    fn watchable(&self, path: &Path) -> bool {
        let relative = self
            .opts
            .roots
            .iter()
            .find_map(|r| path.strip_prefix(r).ok())
            .unwrap_or(path);
        !relative.components().any(|c| {
            let s = c.as_os_str().to_string_lossy();
            self.opts.excludes.iter().any(|e| *e == s)
        })
    }

    fn publish(&self) {
        let mut status = self.shared.status.lock().unwrap();
        status.watched_dirs = self.watched;
        status.degraded = !self.unwatched.is_empty();
        status.unwatched_roots = self
            .unwatched
            .iter()
            .take(STATUS_UNWATCHED_SHOWN)
            .cloned()
            .collect();
        status.unwatched_total = self.unwatched_total;
        status.last_event = self.last_event;
        status.events_applied = self.events_applied;
        status.batches = self.batches;
        status.rescans = self.rescans;
        status.reason = self.reason.clone();
    }
}

// ---- free functions ------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct Known {
    size: u64,
    mtime: i64,
    is_dir: bool,
}

/// Everything the index believes lives under `roots`, or `None` when it has
/// never heard of any of them — which is the common case for a freshly created
/// directory and lets the caller skip the full-index pass entirely.
fn index_snapshot(index: &Index, roots: &[PathBuf]) -> Option<HashMap<PathBuf, Known>> {
    let mut subtree: Vec<u32> = roots.iter().filter_map(|r| index.dir_by_path(r)).collect();
    if subtree.is_empty() {
        return None;
    }
    let mut i = 0;
    while i < subtree.len() {
        let d = subtree[i];
        i += 1;
        subtree.extend_from_slice(index.dir_children(d));
    }
    let mut in_subtree = vec![false; index.dir_count()];
    for d in subtree {
        in_subtree[d as usize] = true;
    }

    let mut out = HashMap::new();
    for id in 0..index.len() as u32 {
        let e = index.entry(id);
        if e.is_dead() || !in_subtree[e.dir as usize] {
            continue;
        }
        out.insert(
            index.path_of(id),
            Known {
                size: e.size,
                mtime: e.mtime,
                is_dir: e.is_dir(),
            },
        );
    }
    Some(out)
}

fn walk_subtree(root: &Path) -> Vec<(PathBuf, u64, i64, bool)> {
    let mut out = Vec::new();
    let walk = jwalk::WalkDir::new(root)
        .skip_hidden(false)
        .follow_links(false);
    for entry in walk {
        let Ok(entry) = entry else { continue };
        if entry.depth() == 0 {
            continue;
        }
        let is_dir = entry.file_type().is_dir();
        let (size, mtime) = match entry.metadata() {
            Ok(md) => (md.len(), md.mtime()),
            Err(_) => (0, 0),
        };
        out.push((entry.path(), size, mtime, is_dir));
        if out.len() >= MAX_RESCAN_ENTRIES {
            break;
        }
    }
    out
}

/// Directory list taken from the in-memory index, in pre-order with depths.
///
/// The index already holds the whole tree, so this avoids a second walk over
/// hundreds of thousands of directories at startup.
fn dirs_from_index(
    engine: &Engine,
    root: &Path,
    excludes: &[String],
) -> Option<Vec<(PathBuf, u16)>> {
    engine.with_index(|index| {
        let root_id = index.dir_by_path(root)?;
        let paths = index.dir_paths();
        let mut out = Vec::new();
        let mut stack = vec![(root_id, 0u16)];
        while let Some((dir, depth)) = stack.pop() {
            out.push((path_from_bytes(paths.get(dir)), depth));
            for &child in index.dir_children(dir) {
                if excludes
                    .iter()
                    .any(|e| e.as_bytes() == index.dir_name(child))
                {
                    continue;
                }
                stack.push((child, depth + 1));
            }
        }
        Some(out)
    })
}

fn dirs_from_fs(root: &Path, excludes: &[String]) -> Vec<(PathBuf, u16)> {
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0u16)];
    while let Some((dir, depth)) = stack.pop() {
        out.push((dir.clone(), depth));
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            if excludes
                .iter()
                .any(|e| e.as_str() == entry.file_name().to_string_lossy())
            {
                continue;
            }
            stack.push((entry.path(), depth + 1));
        }
    }
    out
}

/// Trailing slash in, `Path` out.
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    let trimmed = match bytes {
        [b'/'] => bytes,
        [rest @ .., b'/'] => rest,
        _ => bytes,
    };
    PathBuf::from(
        <std::ffi::OsString as std::os::unix::ffi::OsStringExt>::from_vec(trimmed.to_vec()),
    )
}

fn is_budget_error(e: &notify::Error) -> bool {
    match &e.kind {
        notify::ErrorKind::MaxFilesWatch => true,
        // ENOSPC is what inotify_add_watch actually returns at the limit.
        notify::ErrorKind::Io(io) => io.raw_os_error() == Some(28),
        _ => false,
    }
}

#[cfg(test)]
mod tests;
