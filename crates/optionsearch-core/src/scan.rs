//! Parallel filesystem scan built on `jwalk`.

use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use jwalk::WalkDirGeneric;

use crate::config::Config;
use crate::index::Index;
use crate::model::flags;

#[derive(Debug, Default, Clone, Copy)]
pub struct ScanStats {
    pub dirs: usize,
    pub files: usize,
    pub errors: usize,
    pub elapsed: Duration,
}

impl ScanStats {
    pub fn total(&self) -> usize {
        self.dirs + self.files
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct Meta {
    size: u64,
    mtime: i64,
    flags: u8,
}

type State = ((), Meta);

/// Walks every configured root and returns a fresh index.
pub fn scan(config: &Config) -> (Index, ScanStats) {
    let mut index = Index::new();
    // ~20 bytes per name is the measured average for a typical $HOME.
    index.reserve(1 << 20, 24 << 20);
    let stats = scan_into(&mut index, config);
    (index, stats)
}

pub fn scan_into(index: &mut Index, config: &Config) -> ScanStats {
    let started = Instant::now();
    let mut stats = ScanStats::default();
    let mut dir_ids: HashMap<PathBuf, u32> = HashMap::with_capacity(1 << 16);

    for root in &config.roots {
        let root = match root.canonicalize() {
            Ok(p) => p,
            Err(_) => root.clone(),
        };
        let root_id = index.ensure_dir_path(&root);
        dir_ids.insert(root.clone(), root_id);
        scan_root(index, &root, config, &mut dir_ids, &mut stats);
    }

    stats.elapsed = started.elapsed();
    stats
}

fn scan_root(
    index: &mut Index,
    root: &Path,
    config: &Config,
    dir_ids: &mut HashMap<PathBuf, u32>,
    stats: &mut ScanStats,
) {
    let excludes: Vec<String> = config.scan_excludes.clone();
    let exclude_paths: Vec<PathBuf> = config.exclude_paths.clone();

    let walk = WalkDirGeneric::<State>::new(root)
        .skip_hidden(false)
        .follow_links(false)
        .parallelism(jwalk::Parallelism::RayonDefaultPool {
            busy_timeout: Duration::from_secs(300),
        })
        .process_read_dir(move |_depth, _path, _state, children| {
            children.retain(|c| match c {
                Ok(e) => {
                    if !e.file_type().is_dir() {
                        return true;
                    }
                    let name = e.file_name().to_string_lossy().into_owned();
                    !excludes.contains(&name) && !exclude_paths.iter().any(|p| *p == e.path())
                }
                Err(_) => true,
            });
            // Stat happens here so it runs on the walker's thread pool.
            for c in children.iter_mut().flatten() {
                let mut m = Meta::default();
                if c.file_type().is_symlink() {
                    m.flags |= flags::SYMLINK;
                }
                if c.file_type().is_dir() {
                    m.flags |= flags::DIR;
                }
                if c.file_name().as_encoded_bytes().first() == Some(&b'.') {
                    m.flags |= flags::HIDDEN;
                }
                if let Ok(md) = c.metadata() {
                    m.size = md.len();
                    m.mtime = md.mtime();
                }
                c.client_state = m;
            }
        });

    for result in walk {
        let entry = match result {
            Ok(e) => e,
            Err(_) => {
                stats.errors += 1;
                continue;
            }
        };
        if entry.depth() == 0 {
            continue;
        }
        let Some(&parent) = dir_ids.get(entry.parent_path()) else {
            stats.errors += 1;
            continue;
        };
        let name = entry.file_name();
        let name = <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::as_bytes(name);
        let m = entry.client_state;
        index.push_entry(parent, name, m.flags, m.size, m.mtime);

        if entry.file_type().is_dir() {
            let id = index.add_dir(parent, name);
            dir_ids.insert(entry.path(), id);
            stats.dirs += 1;
        } else {
            stats.files += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::Query;
    use crate::search::search;

    fn write(path: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn scans_tree_and_finds_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        write(&root.join("a/b/hello.txt"), b"hi");
        write(&root.join("a/skipme/junk.bin"), b"0123456789");
        write(&root.join(".hidden"), b"");

        let config = Config {
            roots: vec![root.clone()],
            scan_excludes: vec!["skipme".to_string()],
            ..Config::default()
        };
        let (index, stats) = scan(&config);
        assert_eq!(stats.errors, 0);
        assert!(stats.dirs >= 2, "dirs: {}", stats.dirs);

        let hits = search(&index, &Query::parse("hello.txt"), 10);
        assert_eq!(hits.hits.len(), 1);
        assert_eq!(hits.hits[0].path, root.join("a/b/hello.txt"));
        assert_eq!(hits.hits[0].size, 2);

        assert!(
            search(&index, &Query::parse("junk.bin"), 10)
                .hits
                .is_empty()
        );
        assert_eq!(search(&index, &Query::parse(".hidden"), 10).hits.len(), 1);
        assert!(search(&index, &Query::parse("dir: b"), 10).hits.len() == 1);
    }
}
