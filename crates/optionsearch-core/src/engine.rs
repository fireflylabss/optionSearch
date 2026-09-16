//! Facade tying the index, the database and the matcher together.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::Duration;

use anyhow::Result;

use crate::config::Config;
use crate::db::{Db, DbOp};
use crate::index::Index;
use crate::model::{SearchResults, flags};
use crate::query::Query;
use crate::scan::{self, ScanStats};
use crate::search;

/// Compact once a quarter of the slots are tombstones.
const GARBAGE_THRESHOLD: f32 = 0.25;

/// A filesystem change to fold into the index.
///
/// This is the vocabulary of the watcher seam: `watch.rs` (phase 2) translates
/// debounced inotify events into these and hands batches to
/// [`Engine::apply_events`].
#[derive(Debug, Clone)]
pub enum IndexEvent {
    /// File or directory created or modified.
    Upsert {
        path: PathBuf,
        size: u64,
        mtime: i64,
        is_dir: bool,
    },
    /// A single entry disappeared.
    Remove { path: PathBuf },
    /// A directory disappeared: it and everything below it are removed.
    RemoveDir { path: PathBuf },
}

#[derive(Debug, Default, Clone, Copy)]
pub struct EventOutcome {
    pub added: usize,
    pub updated: usize,
    pub removed: usize,
    /// True when the index was compacted, which renumbers ids and forces a full
    /// database rewrite on the next [`Engine::flush`].
    pub compacted: bool,
}

#[derive(Debug, Clone)]
pub struct EngineStats {
    pub entries: usize,
    pub live_entries: usize,
    pub dead_entries: usize,
    pub dirs: usize,
    pub name_bytes: usize,
    pub memory_bytes: usize,
    pub db_bytes: u64,
    pub pending_writes: usize,
    pub roots: Vec<PathBuf>,
    pub last_scan: Option<String>,
}

struct SearchCache {
    query: Query,
    ids: Vec<u32>,
    generation: u64,
}

pub struct Engine {
    config: RwLock<Config>,
    index: RwLock<Index>,
    db: Mutex<Db>,
    pending: Mutex<Vec<DbOp>>,
    cache: Mutex<Option<SearchCache>>,
    generation: AtomicU64,
}

impl Engine {
    /// Opens the database and loads the snapshot into memory.
    pub fn open(config: Config) -> Result<Engine> {
        config.ensure_dirs()?;
        let db = Db::open(&config.db_path)?;
        let mut index = Index::new();
        db.load(&mut index)?;
        index.warm();
        Ok(Engine {
            config: RwLock::new(config),
            index: RwLock::new(index),
            db: Mutex::new(db),
            pending: Mutex::new(Vec::new()),
            cache: Mutex::new(None),
            generation: AtomicU64::new(0),
        })
    }

    pub fn config(&self) -> Config {
        self.config.read().unwrap().clone()
    }

    /// Bumped whenever the index changes; a UI can use it to discard stale
    /// results or to know when to re-run the current query.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    pub fn is_empty(&self) -> bool {
        self.index.read().unwrap().live_len() == 0
    }

    /// Runs a query and returns at most `limit` ranked hits.
    ///
    /// Consecutive queries that only narrow the previous one (typing) are
    /// answered by filtering the previous match set instead of rescanning.
    pub fn search(&self, query: &str, limit: usize) -> SearchResults {
        let q = Query::parse(query);
        let index = self.index.read().unwrap();
        let generation = self.generation();
        let mut cache = self.cache.lock().unwrap();

        let reusable = cache
            .as_ref()
            .filter(|c| c.generation == generation && q.refines(&c.query));
        let (results, ids) = match reusable {
            Some(c) => search::search_within(&index, &q, &c.ids, limit),
            None => search::search_collect(&index, &q, limit),
        };
        *cache = ids.map(|ids| SearchCache {
            query: q,
            ids,
            generation,
        });
        results
    }

    /// Search without touching the refinement cache (for background/bench use).
    pub fn search_uncached(&self, query: &str, limit: usize) -> SearchResults {
        let q = Query::parse(query);
        let index = self.index.read().unwrap();
        search::search(&index, &q, limit)
    }

    pub fn preview(&self, path: &Path) -> crate::preview::Preview {
        let config = self.config();
        crate::preview::preview(path, &crate::preview::PreviewLimits::from_config(&config))
    }

    /// Registers a new root, scans it into the live index and persists the new
    /// root set. The new root is also picked up by future [`Engine::reindex`]
    /// calls.
    pub fn add_root(&self, root: PathBuf) -> Result<()> {
        let root = root.canonicalize().unwrap_or(root);
        {
            let mut config = self.config.write().unwrap();
            if config.roots.contains(&root) {
                return Ok(());
            }
            config.roots.push(root.clone());
        }
        let config = self.config.read().unwrap().clone();
        let mut scan_cfg = config.clone();
        scan_cfg.roots = vec![root.clone()];
        {
            let mut index = self.index.write().unwrap();
            scan::scan_into(&mut index, &scan_cfg);
            let mut db = self.db.lock().unwrap();
            db.save_all(&index)?;
            db.meta_set(
                "roots",
                &config
                    .roots
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(":"),
            )?;
            db.checkpoint()?;
        }
        self.pending.lock().unwrap().clear();
        self.invalidate();
        Ok(())
    }

    /// Rescans every root from scratch and replaces both the in-memory index and
    /// the persisted snapshot.
    pub fn reindex(&self) -> Result<ScanStats> {
        let (fresh, stats) = scan::scan(&self.config.read().unwrap().clone());
        fresh.warm();
        {
            let mut index = self.index.write().unwrap();
            *index = fresh;
            let mut db = self.db.lock().unwrap();
            db.save_all(&index)?;
            db.meta_set("last_scan", &unix_now().to_string())?;
            db.meta_set(
                "roots",
                &self
                    .config
                    .read()
                    .unwrap()
                    .roots
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(":"),
            )?;
            db.checkpoint()?;
        }
        self.pending.lock().unwrap().clear();
        self.invalidate();
        Ok(stats)
    }

    /// Applies a batch of filesystem events to the in-memory index and queues
    /// the matching database writes.
    ///
    /// This is the seam the watcher builds on. Contract:
    ///
    /// * Call it with **batches** (one debounce window's worth). Name lookups
    ///   are resolved in a single pass over the index, so one call with 500
    ///   events costs about the same as one call with a single event.
    /// * It takes the index write lock for the duration, so searches block
    ///   briefly; keep batches bounded.
    /// * Database writes are queued, not committed. Call [`Engine::flush`]
    ///   after the batch (or on a timer) to commit them.
    /// * Ids may be renumbered when a compaction triggers; anything holding
    ///   entry ids across calls must re-check [`Engine::generation`].
    pub fn apply_events(&self, events: &[IndexEvent]) -> Result<EventOutcome> {
        if events.is_empty() {
            return Ok(EventOutcome::default());
        }
        let mut index = self.index.write().unwrap();
        let mut ops: Vec<DbOp> = Vec::with_capacity(events.len());
        let mut out = EventOutcome::default();

        // Resolve (parent dir, name) for every event in one pass.
        let mut wanted: Vec<(u32, Vec<u8>)> = Vec::with_capacity(events.len());
        let mut slots: Vec<Option<usize>> = Vec::with_capacity(events.len());
        for ev in events {
            let path = event_path(ev);
            let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
                slots.push(None);
                continue;
            };
            let parent_id = match ev {
                IndexEvent::Upsert { .. } => {
                    let before = index.dir_count();
                    let id = index.ensure_dir_path(parent);
                    for new in before..index.dir_count() {
                        let node = index.dir_node(new as u32);
                        ops.push(DbOp::AddDir {
                            id: new as u32,
                            parent: node.parent,
                            name: index.dir_name(new as u32).to_vec(),
                        });
                    }
                    id
                }
                _ => match index.dir_by_path(parent) {
                    Some(id) => id,
                    None => {
                        slots.push(None);
                        continue;
                    }
                },
            };
            slots.push(Some(wanted.len()));
            wanted.push((
                parent_id,
                <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::as_bytes(name).to_vec(),
            ));
        }
        let found = index.find_batch(&wanted);

        for (ev, slot) in events.iter().zip(slots.iter()) {
            let Some(slot) = *slot else { continue };
            let (dir, name) = &wanted[slot];
            let existing = found[slot];
            match ev {
                IndexEvent::Upsert {
                    path,
                    size,
                    mtime,
                    is_dir,
                } => {
                    let id = match existing {
                        Some(id) => {
                            index.update_entry(id, *size, *mtime);
                            out.updated += 1;
                            id
                        }
                        None => {
                            let mut f = if *is_dir { flags::DIR } else { 0 };
                            if name.first() == Some(&b'.') {
                                f |= flags::HIDDEN;
                            }
                            let id = index.push_entry(*dir, name, f, *size, *mtime);
                            if *is_dir && index.child_dir(*dir, name).is_none() {
                                let new = index.add_dir(*dir, name);
                                ops.push(DbOp::AddDir {
                                    id: new,
                                    parent: *dir,
                                    name: name.clone(),
                                });
                            }
                            out.added += 1;
                            id
                        }
                    };
                    let e = index.entry(id);
                    ops.push(DbOp::Upsert {
                        id,
                        dir: e.dir,
                        name: name.clone(),
                        size: e.size,
                        mtime: e.mtime,
                        flags: e.flags,
                    });
                    let _ = path;
                }
                IndexEvent::Remove { .. } => {
                    if let Some(id) = existing {
                        index.remove_entry(id);
                        ops.push(DbOp::Delete { id });
                        out.removed += 1;
                    }
                }
                IndexEvent::RemoveDir { path } => {
                    if let Some(id) = existing {
                        index.remove_entry(id);
                        ops.push(DbOp::Delete { id });
                        out.removed += 1;
                    }
                    if let Some(dir_id) = index.dir_by_path(path) {
                        for id in index.remove_dir_subtree(dir_id) {
                            ops.push(DbOp::Delete { id });
                            out.removed += 1;
                        }
                    }
                }
            }
        }

        if index.compact_if_needed(GARBAGE_THRESHOLD) {
            out.compacted = true;
            ops.clear();
            ops.push(DbOp::FullResave);
        }
        drop(index);

        let mut pending = self.pending.lock().unwrap();
        if out.compacted {
            pending.clear();
        }
        pending.extend(ops);
        drop(pending);
        self.invalidate();
        Ok(out)
    }

    /// Commits queued database writes.
    pub fn flush(&self) -> Result<()> {
        let ops = std::mem::take(&mut *self.pending.lock().unwrap());
        if ops.is_empty() {
            return Ok(());
        }
        let index = self.index.read().unwrap();
        self.db.lock().unwrap().apply_ops(&index, &ops)
    }

    pub fn stats(&self) -> EngineStats {
        let index = self.index.read().unwrap();
        let db = self.db.lock().unwrap();
        EngineStats {
            entries: index.len(),
            live_entries: index.live_len(),
            dead_entries: index.dead_len(),
            dirs: index.dir_count(),
            name_bytes: index.name_bytes(),
            memory_bytes: index.memory_bytes(),
            db_bytes: db.size_on_disk(),
            pending_writes: self.pending.lock().unwrap().len(),
            roots: self.config.read().unwrap().roots.clone(),
            last_scan: db.meta_get("last_scan").ok().flatten(),
        }
    }

    /// Read access to the index for callers that need raw entries (the UI reads
    /// paths and metadata straight from hits, so this is rarely needed).
    pub fn with_index<R>(&self, f: impl FnOnce(&Index) -> R) -> R {
        f(&self.index.read().unwrap())
    }

    fn invalidate(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        *self.cache.lock().unwrap() = None;
    }
}

fn event_path(ev: &IndexEvent) -> &Path {
    match ev {
        IndexEvent::Upsert { path, .. }
        | IndexEvent::Remove { path }
        | IndexEvent::RemoveDir { path } => path,
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_engine(root: &Path, db: &Path) -> Engine {
        Engine::open(Config {
            roots: vec![root.to_path_buf()],
            db_path: db.to_path_buf(),
            cache_dir: db.parent().unwrap().join("cache"),
            ..Config::default()
        })
        .unwrap()
    }

    /// The UI keeps the engine in an `Arc` and searches from a background
    /// thread, so this must never regress.
    #[test]
    fn engine_is_send_and_sync() {
        fn assert<T: Send + Sync>() {}
        assert::<Engine>();
    }

    #[test]
    fn index_search_persist_and_reload() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/alpha.txt"), b"x").unwrap();
        std::fs::write(root.join("beta.md"), b"yy").unwrap();
        let db = tmp.path().join("index.db");

        let engine = temp_engine(&root, &db);
        let stats = engine.reindex().unwrap();
        assert_eq!(stats.files, 2);
        assert_eq!(engine.search("alpha", 10).hits.len(), 1);
        assert_eq!(engine.stats().live_entries, 3);

        let reopened = temp_engine(&root, &db);
        let hits = reopened.search("alpha", 10);
        assert_eq!(
            hits.hits[0].path,
            root.canonicalize().unwrap().join("sub/alpha.txt")
        );
    }

    #[test]
    fn events_add_update_and_remove() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        let db = tmp.path().join("index.db");
        let engine = temp_engine(&root, &db);
        engine.reindex().unwrap();
        let root = root.canonicalize().unwrap();

        let gen0 = engine.generation();
        engine
            .apply_events(&[IndexEvent::Upsert {
                path: root.join("new/deep.log"),
                size: 7,
                mtime: 11,
                is_dir: false,
            }])
            .unwrap();
        assert!(engine.generation() > gen0);
        let hits = engine.search("deep.log", 10);
        assert_eq!(hits.hits.len(), 1);
        assert_eq!(hits.hits[0].size, 7);

        engine
            .apply_events(&[IndexEvent::Upsert {
                path: root.join("new/deep.log"),
                size: 9,
                mtime: 12,
                is_dir: false,
            }])
            .unwrap();
        assert_eq!(engine.search("deep.log", 10).hits[0].size, 9);

        engine.flush().unwrap();
        let out = engine
            .apply_events(&[IndexEvent::Remove {
                path: root.join("new/deep.log"),
            }])
            .unwrap();
        assert_eq!(out.removed, 1);
        assert!(engine.search("deep.log", 10).hits.is_empty());
        engine.flush().unwrap();

        let reopened = temp_engine(&root, &db);
        assert!(reopened.search("deep.log", 10).hits.is_empty());
    }

    #[test]
    fn removing_a_directory_drops_the_subtree() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(root.join("junk/inner")).unwrap();
        std::fs::write(root.join("junk/inner/x.bin"), b"1").unwrap();
        std::fs::write(root.join("keep.bin"), b"1").unwrap();
        let db = tmp.path().join("index.db");
        let engine = temp_engine(&root, &db);
        engine.reindex().unwrap();
        let root = root.canonicalize().unwrap();

        assert_eq!(engine.search("x.bin", 10).hits.len(), 1);
        engine
            .apply_events(&[IndexEvent::RemoveDir {
                path: root.join("junk"),
            }])
            .unwrap();
        assert!(engine.search("x.bin", 10).hits.is_empty());
        assert!(engine.search("junk", 10).hits.is_empty());
        assert_eq!(engine.search("keep.bin", 10).hits.len(), 1);
        engine.flush().unwrap();

        let reopened = temp_engine(&root, &db);
        assert!(reopened.search("x.bin", 10).hits.is_empty());
        assert_eq!(reopened.search("keep.bin", 10).hits.len(), 1);
    }

    #[test]
    fn refinement_cache_matches_a_cold_search() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        for i in 0..50 {
            std::fs::write(root.join(format!("report_{i}.txt")), b"x").unwrap();
        }
        let engine = temp_engine(&root, &tmp.path().join("index.db"));
        engine.reindex().unwrap();

        let _ = engine.search("rep", 10);
        let warm = engine.search("report_4", 10);
        let cold = engine.search_uncached("report_4", 10);
        assert_eq!(warm.total, cold.total);
        assert_eq!(warm.hits[0].path, cold.hits[0].path);
    }
}
