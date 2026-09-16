//! In-memory index: a directory tree plus columnar entry storage.
//!
//! Entries are stored as parallel arrays instead of an array of structs. The
//! search hot loop only reads the 8-byte signature column, and only touches the
//! name arena for the entries that survive the prefilter.

use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::arena::Arena;
use crate::model::{Entry, flags, signature, signature2};

pub const NO_DIR: u32 = u32::MAX;

/// Materialized directory paths, each ending with `/`.
///
/// Built once per index generation and reused by every path query, so matching
/// `src/main` costs one pass over ~380k directory paths instead of rebuilding a
/// path for each of the millions of entries.
#[derive(Debug, Default)]
pub struct DirPaths {
    arena: Vec<u8>,
    off: Vec<u32>,
    len: Vec<u32>,
}

impl DirPaths {
    #[inline]
    pub fn get(&self, dir: u32) -> &[u8] {
        let i = dir as usize;
        let o = self.off[i] as usize;
        &self.arena[o..o + self.len[i] as usize]
    }

    pub fn len(&self) -> usize {
        self.off.len()
    }

    pub fn is_empty(&self) -> bool {
        self.off.is_empty()
    }

    pub fn bytes(&self) -> usize {
        self.arena.len() + self.off.len() * 8
    }
}

#[derive(Debug, Clone, Copy)]
pub struct DirNode {
    pub parent: u32,
    pub name_off: u32,
    pub name_len: u16,
    pub depth: u16,
}

#[derive(Debug, Default)]
pub struct Index {
    names: Arena,
    dir_names: Arena,
    dirs: Vec<DirNode>,
    dir_children: Vec<Vec<u32>>,
    dir_paths: Mutex<Option<Arc<DirPaths>>>,

    pub(crate) sig: Vec<u64>,
    pub(crate) sig2: Vec<u32>,
    pub(crate) off: Vec<u32>,
    pub(crate) len: Vec<u16>,
    /// Lowercased first byte of each name, so single-letter queries can be
    /// ranked without touching the name arena at all.
    pub(crate) first: Vec<u8>,
    pub(crate) eflags: Vec<u8>,
    pub(crate) edir: Vec<u32>,
    pub(crate) size: Vec<u64>,
    pub(crate) mtime: Vec<i64>,

    free: Vec<u32>,
    dead: usize,
    roots: Vec<u32>,
}

impl Index {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reserve(&mut self, entries: usize, name_bytes: usize) {
        self.sig.reserve(entries);
        self.sig2.reserve(entries);
        self.off.reserve(entries);
        self.len.reserve(entries);
        self.first.reserve(entries);
        self.eflags.reserve(entries);
        self.edir.reserve(entries);
        self.size.reserve(entries);
        self.mtime.reserve(entries);
        self.names = Arena::with_capacity(name_bytes);
    }

    // ---- directories -----------------------------------------------------

    pub fn dir_count(&self) -> usize {
        self.dirs.len()
    }

    pub fn roots(&self) -> &[u32] {
        &self.roots
    }

    pub fn dir_node(&self, dir: u32) -> DirNode {
        self.dirs[dir as usize]
    }

    pub fn dir_name(&self, dir: u32) -> &[u8] {
        let n = self.dirs[dir as usize];
        self.dir_names.get(n.name_off, n.name_len)
    }

    pub fn dir_children(&self, dir: u32) -> &[u32] {
        &self.dir_children[dir as usize]
    }

    /// Directory paths, built on first use and cached until the tree changes.
    pub fn dir_paths(&self) -> Arc<DirPaths> {
        let mut slot = self.dir_paths.lock().unwrap();
        if slot.is_none() {
            *slot = Some(Arc::new(self.build_dir_paths()));
        }
        slot.clone().unwrap()
    }

    /// Precomputes everything a search needs, so the first query after a scan is
    /// not the one that pays for it.
    pub fn warm(&self) {
        self.dir_paths();
    }

    fn build_dir_paths(&self) -> DirPaths {
        let n = self.dirs.len();
        let mut dp = DirPaths {
            arena: Vec::with_capacity(n * 48),
            off: Vec::with_capacity(n),
            len: Vec::with_capacity(n),
        };
        for d in 0..n {
            let node = self.dirs[d];
            let start = dp.arena.len();
            // Parents always have a lower id than their children, so the parent
            // path is already materialized.
            if node.parent != NO_DIR && (node.parent as usize) < d {
                let p = node.parent as usize;
                let (po, pl) = (dp.off[p] as usize, dp.len[p] as usize);
                dp.arena.extend_from_within(po..po + pl);
            } else if node.parent != NO_DIR {
                let mut buf = Vec::new();
                self.dir_path_into(node.parent, &mut buf);
                dp.arena.extend_from_slice(&buf);
                dp.arena.push(b'/');
            }
            let name = self.dir_names.get(node.name_off, node.name_len);
            if name.is_empty() {
                if dp.arena.len() == start {
                    dp.arena.push(b'/');
                }
            } else {
                dp.arena.extend_from_slice(name);
                dp.arena.push(b'/');
            }
            dp.off.push(start as u32);
            dp.len.push((dp.arena.len() - start) as u32);
        }
        dp
    }

    fn invalidate_dir_paths(&mut self) {
        if let Ok(slot) = self.dir_paths.get_mut() {
            *slot = None;
        }
    }

    /// Adds a child directory. The caller must guarantee it does not exist yet;
    /// use [`Index::child_dir`] first when unsure.
    pub fn add_dir(&mut self, parent: u32, name: &[u8]) -> u32 {
        self.invalidate_dir_paths();
        let (name_off, name_len) = self.dir_names.push(name);
        let depth = if parent == NO_DIR {
            0
        } else {
            self.dirs[parent as usize].depth.saturating_add(1)
        };
        let id = self.dirs.len() as u32;
        self.dirs.push(DirNode {
            parent,
            name_off,
            name_len,
            depth,
        });
        self.dir_children.push(Vec::new());
        if parent == NO_DIR {
            self.roots.push(id);
        } else {
            self.dir_children[parent as usize].push(id);
        }
        id
    }

    pub fn child_dir(&self, parent: u32, name: &[u8]) -> Option<u32> {
        let children: &[u32] = if parent == NO_DIR {
            &self.roots
        } else {
            &self.dir_children[parent as usize]
        };
        children.iter().copied().find(|&c| self.dir_name(c) == name)
    }

    /// Resolves (creating as needed) the directory node chain for `path`.
    pub fn ensure_dir_path(&mut self, path: &Path) -> u32 {
        let mut cur = NO_DIR;
        for comp in path.components() {
            let name: &[u8] = match comp {
                Component::RootDir => b"",
                Component::Normal(s) => std::os::unix::ffi::OsStrExt::as_bytes(s),
                Component::CurDir => continue,
                Component::ParentDir | Component::Prefix(_) => continue,
            };
            cur = match self.child_dir(cur, name) {
                Some(id) => id,
                None => self.add_dir(cur, name),
            };
        }
        cur
    }

    pub fn dir_by_path(&self, path: &Path) -> Option<u32> {
        let mut cur = NO_DIR;
        for comp in path.components() {
            let name: &[u8] = match comp {
                Component::RootDir => b"",
                Component::Normal(s) => std::os::unix::ffi::OsStrExt::as_bytes(s),
                _ => continue,
            };
            cur = self.child_dir(cur, name)?;
        }
        if cur == NO_DIR { None } else { Some(cur) }
    }

    // ---- entries ---------------------------------------------------------

    pub fn len(&self) -> usize {
        self.sig.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sig.is_empty()
    }

    pub fn live_len(&self) -> usize {
        self.sig.len() - self.dead
    }

    pub fn dead_len(&self) -> usize {
        self.dead
    }

    pub fn name_bytes(&self) -> usize {
        self.names.len()
    }

    #[inline]
    pub fn name(&self, id: u32) -> &[u8] {
        self.names.get(self.off[id as usize], self.len[id as usize])
    }

    #[inline]
    pub(crate) fn names_arena(&self) -> &Arena {
        &self.names
    }

    /// Starts loading an entry's name into cache. The search loop runs a batch
    /// of these before evaluating the batch, which is what keeps the scan from
    /// stalling on DRAM latency one name at a time.
    #[inline]
    pub(crate) fn prefetch_name(&self, id: u32) {
        #[cfg(target_arch = "x86_64")]
        {
            let off = self.off[id as usize] as usize;
            // SAFETY: `off` is a valid arena offset, and prefetching is a hint
            // with no architectural effect.
            unsafe {
                let ptr = self.names.as_ptr().add(off);
                std::arch::x86_64::_mm_prefetch(ptr as *const i8, std::arch::x86_64::_MM_HINT_T0);
            }
        }
        #[cfg(not(target_arch = "x86_64"))]
        let _ = id;
    }

    pub fn entry(&self, id: u32) -> Entry {
        let i = id as usize;
        Entry {
            dir: self.edir[i],
            name_off: self.off[i],
            name_len: self.len[i],
            flags: self.eflags[i],
            size: self.size[i],
            mtime: self.mtime[i],
        }
    }

    pub fn push_entry(&mut self, dir: u32, name: &[u8], flags: u8, size: u64, mtime: i64) -> u32 {
        let (off, len) = self.names.push(name);
        let sig = signature(&name[..len as usize]);
        let sig2 = signature2(&name[..len as usize]);
        let first = name.first().map_or(0, |b| b | 0x20);
        if let Some(slot) = self.free.pop() {
            let i = slot as usize;
            self.sig[i] = sig;
            self.sig2[i] = sig2;
            self.off[i] = off;
            self.len[i] = len;
            self.first[i] = first;
            self.eflags[i] = flags;
            self.edir[i] = dir;
            self.size[i] = size;
            self.mtime[i] = mtime;
            self.dead -= 1;
            slot
        } else {
            self.sig.push(sig);
            self.sig2.push(sig2);
            self.off.push(off);
            self.len.push(len);
            self.first.push(first);
            self.eflags.push(flags);
            self.edir.push(dir);
            self.size.push(size);
            self.mtime.push(mtime);
            (self.sig.len() - 1) as u32
        }
    }

    /// Appends a tombstoned slot that is *not* added to the free list, used to
    /// keep ids aligned when loading a snapshot with holes. The slot is
    /// reclaimed by the next [`Index::compact`].
    pub fn push_hole(&mut self) -> u32 {
        self.sig.push(0);
        self.sig2.push(0);
        self.off.push(0);
        self.len.push(0);
        self.first.push(0);
        self.eflags.push(flags::DEAD);
        self.edir.push(0);
        self.size.push(0);
        self.mtime.push(0);
        self.dead += 1;
        (self.sig.len() - 1) as u32
    }

    pub fn update_entry(&mut self, id: u32, size: u64, mtime: i64) {
        let i = id as usize;
        self.size[i] = size;
        self.mtime[i] = mtime;
    }

    /// Tombstones an entry. The slot is reused by the next `push_entry`; the
    /// name bytes stay in the arena until [`Index::compact`].
    pub fn remove_entry(&mut self, id: u32) {
        let i = id as usize;
        if self.eflags[i] & flags::DEAD != 0 {
            return;
        }
        self.eflags[i] |= flags::DEAD;
        self.sig[i] = 0;
        self.sig2[i] = 0; // fails the prefilter for every non-empty query
        self.free.push(id);
        self.dead += 1;
    }

    /// Single pass resolving several `(dir, name)` lookups at once. Watcher
    /// batches should use this instead of calling it per event: the index has
    /// no per-directory entry map, by design, to keep memory down.
    pub fn find_batch(&self, wanted: &[(u32, Vec<u8>)]) -> Vec<Option<u32>> {
        let mut out = vec![None; wanted.len()];
        if wanted.is_empty() {
            return out;
        }
        // A membership bitmap over parent directories rejects almost every entry
        // with a single indexed load, so only the handful of entries living in a
        // touched directory pay for a name hash.
        let mut touched = vec![false; self.dirs.len()];
        let mut lookup: std::collections::HashMap<(u32, &[u8]), Vec<usize>> =
            std::collections::HashMap::with_capacity(wanted.len());
        for (i, (dir, name)) in wanted.iter().enumerate() {
            if let Some(slot) = touched.get_mut(*dir as usize) {
                *slot = true;
            }
            lookup.entry((*dir, name.as_slice())).or_default().push(i);
        }

        for i in 0..self.sig.len() {
            if self.eflags[i] & flags::DEAD != 0 {
                continue;
            }
            let d = self.edir[i];
            if !touched.get(d as usize).copied().unwrap_or(false) {
                continue;
            }
            let name = self.names.get(self.off[i], self.len[i]);
            if let Some(slots) = lookup.get(&(d, name)) {
                for &s in slots {
                    if out[s].is_none() {
                        out[s] = Some(i as u32);
                    }
                }
            }
        }
        out
    }

    pub fn find_in_dir(&self, dir: u32, name: &[u8]) -> Option<u32> {
        self.find_batch(&[(dir, name.to_vec())])[0]
    }

    /// Tombstones every entry under `dir` (inclusive) and detaches the subtree
    /// from its parent. Returns the removed entry ids so the caller can queue
    /// the matching database deletes.
    pub fn remove_dir_subtree(&mut self, dir: u32) -> Vec<u32> {
        self.invalidate_dir_paths();
        let mut in_subtree = vec![false; self.dirs.len()];
        let mut stack = vec![dir];
        while let Some(d) = stack.pop() {
            if in_subtree[d as usize] {
                continue;
            }
            in_subtree[d as usize] = true;
            stack.extend_from_slice(&self.dir_children[d as usize]);
        }
        let parent = self.dirs[dir as usize].parent;
        if parent == NO_DIR {
            self.roots.retain(|&r| r != dir);
        } else {
            self.dir_children[parent as usize].retain(|&c| c != dir);
        }
        let mut removed = Vec::new();
        for i in 0..self.sig.len() {
            if self.eflags[i] & flags::DEAD == 0 && in_subtree[self.edir[i] as usize] {
                self.remove_entry(i as u32);
                removed.push(i as u32);
            }
        }
        removed
    }

    // ---- paths -----------------------------------------------------------

    pub fn dir_path_into(&self, dir: u32, out: &mut Vec<u8>) {
        let mut stack: [u32; 64] = [0; 64];
        let mut n = 0usize;
        let mut cur = dir;
        let mut overflow: Vec<u32> = Vec::new();
        while cur != NO_DIR {
            if n < stack.len() {
                stack[n] = cur;
                n += 1;
            } else {
                overflow.push(cur);
            }
            cur = self.dirs[cur as usize].parent;
        }
        for &d in overflow.iter().rev() {
            self.push_component(d, out);
        }
        for &d in stack[..n].iter().rev() {
            self.push_component(d, out);
        }
    }

    fn push_component(&self, dir: u32, out: &mut Vec<u8>) {
        let name = self.dir_name(dir);
        if name.is_empty() {
            if out.is_empty() {
                out.push(b'/');
            }
            return;
        }
        if out.last().copied() != Some(b'/') {
            out.push(b'/');
        }
        out.extend_from_slice(name);
    }

    pub fn path_into(&self, id: u32, out: &mut Vec<u8>) {
        out.clear();
        self.dir_path_into(self.edir[id as usize], out);
        if out.last().copied() != Some(b'/') {
            out.push(b'/');
        }
        out.extend_from_slice(self.name(id));
    }

    pub fn path_of(&self, id: u32) -> PathBuf {
        let mut buf = Vec::with_capacity(128);
        self.path_into(id, &mut buf);
        PathBuf::from(<std::ffi::OsString as std::os::unix::ffi::OsStringExt>::from_vec(buf))
    }

    pub fn dir_path_of(&self, dir: u32) -> PathBuf {
        let mut buf = Vec::with_capacity(128);
        self.dir_path_into(dir, &mut buf);
        PathBuf::from(<std::ffi::OsString as std::os::unix::ffi::OsStringExt>::from_vec(buf))
    }

    // ---- maintenance -----------------------------------------------------

    pub fn garbage_ratio(&self) -> f32 {
        if self.sig.is_empty() {
            0.0
        } else {
            self.dead as f32 / self.sig.len() as f32
        }
    }

    /// Rebuilds the arena and columns, dropping tombstones and unreachable
    /// directories. **Entry and directory ids are renumbered.**
    pub fn compact(&mut self) {
        let mut new = Index::new();
        new.reserve(self.live_len(), self.names.len());

        let mut dir_map = vec![NO_DIR; self.dirs.len()];
        let mut stack: Vec<(u32, u32)> = self.roots.iter().map(|&r| (r, NO_DIR)).collect();
        stack.reverse();
        while let Some((old, new_parent)) = stack.pop() {
            let name = self.dir_name(old).to_vec();
            let id = new.add_dir(new_parent, &name);
            dir_map[old as usize] = id;
            for &c in self.dir_children[old as usize].iter().rev() {
                stack.push((c, id));
            }
        }

        for i in 0..self.sig.len() {
            if self.eflags[i] & flags::DEAD != 0 {
                continue;
            }
            let nd = dir_map[self.edir[i] as usize];
            if nd == NO_DIR {
                continue;
            }
            let name = self.names.get(self.off[i], self.len[i]).to_vec();
            new.push_entry(nd, &name, self.eflags[i], self.size[i], self.mtime[i]);
        }
        *self = new;
    }

    pub fn compact_if_needed(&mut self, threshold: f32) -> bool {
        if self.garbage_ratio() > threshold {
            self.compact();
            true
        } else {
            false
        }
    }

    /// Approximate resident size of the index in bytes.
    pub fn memory_bytes(&self) -> usize {
        let per_entry = 8 + 4 + 4 + 2 + 1 + 1 + 4 + 8 + 8;
        self.names.capacity()
            + self.dir_names.capacity()
            + self.sig.capacity() * per_entry
            + self.dirs.capacity() * std::mem::size_of::<DirNode>()
            + self
                .dir_paths
                .lock()
                .unwrap()
                .as_ref()
                .map_or(0, |d| d.bytes())
            + self
                .dir_children
                .iter()
                .map(|c| c.capacity() * 4)
                .sum::<usize>()
            + self.dir_children.capacity() * std::mem::size_of::<Vec<u32>>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> (Index, u32) {
        let mut idx = Index::new();
        let home = idx.ensure_dir_path(Path::new("/home/gabriel"));
        let id = idx.push_entry(home, b"notes.md", 0, 42, 7);
        (idx, id)
    }

    #[test]
    fn materializes_absolute_paths() {
        let (idx, id) = sample();
        assert_eq!(idx.path_of(id), PathBuf::from("/home/gabriel/notes.md"));
    }

    #[test]
    fn root_path_has_single_slash() {
        let mut idx = Index::new();
        let root = idx.ensure_dir_path(Path::new("/"));
        let id = idx.push_entry(root, b"vmlinuz", 0, 1, 1);
        assert_eq!(idx.dir_path_of(root), PathBuf::from("/"));
        assert_eq!(idx.path_of(id), PathBuf::from("/vmlinuz"));
    }

    #[test]
    fn ensure_dir_path_is_idempotent() {
        let mut idx = Index::new();
        let a = idx.ensure_dir_path(Path::new("/a/b/c"));
        let b = idx.ensure_dir_path(Path::new("/a/b/c"));
        assert_eq!(a, b);
        assert_eq!(idx.dir_count(), 4); // "/", a, b, c
        assert_eq!(idx.dir_by_path(Path::new("/a/b")), Some(2));
        assert_eq!(idx.dir_by_path(Path::new("/a/x")), None);
    }

    #[test]
    fn deep_paths_beyond_inline_stack() {
        let mut idx = Index::new();
        let mut p = PathBuf::from("/");
        for i in 0..90 {
            p.push(format!("d{i}"));
        }
        let dir = idx.ensure_dir_path(&p);
        let id = idx.push_entry(dir, b"leaf", 0, 0, 0);
        assert_eq!(idx.path_of(id), p.join("leaf"));
    }

    #[test]
    fn tombstoned_slots_are_reused_and_compaction_renumbers() {
        let mut idx = Index::new();
        let d = idx.ensure_dir_path(Path::new("/x"));
        let a = idx.push_entry(d, b"a", 0, 1, 1);
        let b = idx.push_entry(d, b"b", 0, 2, 2);
        idx.remove_entry(a);
        assert_eq!(idx.live_len(), 1);
        assert!(idx.garbage_ratio() > 0.4);
        let c = idx.push_entry(d, b"c", 0, 3, 3);
        assert_eq!(c, a, "free slot must be reused");
        assert_eq!(idx.live_len(), 2);

        idx.remove_entry(b);
        idx.compact();
        assert_eq!(idx.live_len(), 1);
        assert_eq!(idx.len(), 1);
        assert_eq!(idx.path_of(0), PathBuf::from("/x/c"));
    }

    #[test]
    fn find_batch_resolves_names() {
        let mut idx = Index::new();
        let d = idx.ensure_dir_path(Path::new("/x"));
        let a = idx.push_entry(d, b"a.txt", 0, 1, 1);
        idx.push_entry(d, b"b.txt", 0, 1, 1);
        let got = idx.find_batch(&[(d, b"a.txt".to_vec()), (d, b"zz".to_vec())]);
        assert_eq!(got, vec![Some(a), None]);
    }

    #[test]
    fn dir_paths_are_cached_and_invalidated() {
        let mut idx = Index::new();
        let d = idx.ensure_dir_path(Path::new("/home/gabriel/projects"));
        let dp = idx.dir_paths();
        assert_eq!(dp.get(d), b"/home/gabriel/projects/");
        assert_eq!(dp.get(0), b"/");
        assert_eq!(dp.len(), idx.dir_count());

        let e = idx.ensure_dir_path(Path::new("/home/gabriel/projects/optionsearch"));
        let dp = idx.dir_paths();
        assert_eq!(dp.get(e), b"/home/gabriel/projects/optionsearch/");
    }
}
