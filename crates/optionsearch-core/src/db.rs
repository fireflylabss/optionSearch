//! SQLite persistence.
//!
//! SQLite is *not* the search engine — a `LIKE '%x%'` over 3M rows costs
//! hundreds of milliseconds. It only stores the tree so a cold start does not
//! have to walk the filesystem: boot does one sequential `SELECT` per table.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, params};

use crate::index::{Index, NO_DIR};
use crate::model::flags;

pub const SCHEMA_VERSION: i64 = 1;

/// A pending write produced by an index mutation (see [`crate::Engine::apply_events`]).
#[derive(Debug, Clone)]
pub enum DbOp {
    Upsert {
        id: u32,
        dir: u32,
        name: Vec<u8>,
        size: u64,
        mtime: i64,
        flags: u8,
    },
    Delete {
        id: u32,
    },
    AddDir {
        id: u32,
        parent: u32,
        name: Vec<u8>,
    },
    /// Ids were renumbered (compaction): the whole snapshot must be rewritten.
    FullResave,
}

pub struct Db {
    conn: Connection,
    path: PathBuf,
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )
        .with_context(|| format!("opening {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "temp_store", "MEMORY")?;
        conn.pragma_update(None, "cache_size", -64_000i64)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS dirs (id INTEGER PRIMARY KEY, parent INTEGER, name TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS entries (id INTEGER PRIMARY KEY, dir INTEGER NOT NULL, name TEXT NOT NULL,
                                                size INTEGER, mtime INTEGER, flags INTEGER);
             CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v TEXT);",
        )?;
        let db = Db {
            conn,
            path: path.to_path_buf(),
        };
        match db.meta_get("schema_version")? {
            Some(v) if v != SCHEMA_VERSION.to_string() => db.reset()?,
            None => db.meta_set("schema_version", &SCHEMA_VERSION.to_string())?,
            _ => {}
        }
        Ok(db)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn reset(&self) -> Result<()> {
        self.conn
            .execute_batch("DELETE FROM entries; DELETE FROM dirs; DELETE FROM meta;")?;
        self.meta_set("schema_version", &SCHEMA_VERSION.to_string())?;
        Ok(())
    }

    pub fn meta_get(&self, k: &str) -> Result<Option<String>> {
        let mut st = self.conn.prepare("SELECT v FROM meta WHERE k = ?1")?;
        let mut rows = st.query([k])?;
        Ok(match rows.next()? {
            Some(r) => Some(r.get(0)?),
            None => None,
        })
    }

    pub fn meta_set(&self, k: &str, v: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta(k, v) VALUES (?1, ?2) ON CONFLICT(k) DO UPDATE SET v = ?2",
            params![k, v],
        )?;
        Ok(())
    }

    /// Loads the whole snapshot into `index`. Returns the number of entries.
    ///
    /// Ids are preserved: [`Db::save_all`] always writes a compacted, densely
    /// numbered snapshot where a parent directory precedes its children.
    pub fn load(&self, index: &mut Index) -> Result<usize> {
        let count: i64 = self
            .conn
            .query_row("SELECT count(*) FROM entries", [], |r| r.get(0))?;
        index.reserve(count as usize, count as usize * 20);

        let mut st = self
            .conn
            .prepare("SELECT id, parent, name FROM dirs ORDER BY id")?;
        let mut rows = st.query([])?;
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let parent: i64 = row.get(1)?;
            let name = row_bytes(row, 2)?;
            let parent = if parent < 0 { NO_DIR } else { parent as u32 };
            let got = index.add_dir(parent, &name);
            anyhow::ensure!(got as i64 == id, "dirs table is not densely numbered");
        }

        let mut st = self
            .conn
            .prepare("SELECT id, dir, name, size, mtime, flags FROM entries ORDER BY id")?;
        let mut rows = st.query([])?;
        let mut n = 0usize;
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let dir: i64 = row.get(1)?;
            let name = row_bytes(row, 2)?;
            let size: i64 = row.get(3)?;
            let mtime: i64 = row.get(4)?;
            let f: i64 = row.get(5)?;
            // Incremental deletes leave gaps; keep ids aligned with tombstones.
            while (index.len() as i64) < id {
                index.push_hole();
            }
            let got = index.push_entry(
                dir as u32,
                &name,
                (f as u8) & !flags::DEAD,
                size as u64,
                mtime,
            );
            anyhow::ensure!(got as i64 == id, "entries table is not densely numbered");
            n += 1;
        }
        Ok(n)
    }

    /// Rewrites the full snapshot. `index` must already be compacted.
    pub fn save_all(&mut self, index: &Index) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute_batch("DELETE FROM entries; DELETE FROM dirs;")?;
        {
            let mut st = tx.prepare("INSERT INTO dirs(id, parent, name) VALUES (?1, ?2, ?3)")?;
            for id in 0..index.dir_count() as u32 {
                let node = index.dir_node(id);
                let parent = if node.parent == NO_DIR {
                    -1i64
                } else {
                    node.parent as i64
                };
                st.execute(params![id as i64, parent, index.dir_name(id)])?;
            }
            let mut st = tx.prepare(
                "INSERT INTO entries(id, dir, name, size, mtime, flags) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for id in 0..index.len() as u32 {
                let e = index.entry(id);
                if e.is_dead() {
                    continue;
                }
                st.execute(params![
                    id as i64,
                    e.dir as i64,
                    index.name(id),
                    e.size as i64,
                    e.mtime,
                    e.flags as i64
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Applies a batch of incremental writes inside one transaction.
    pub fn apply_ops(&mut self, index: &Index, ops: &[DbOp]) -> Result<()> {
        if ops.iter().any(|o| matches!(o, DbOp::FullResave)) {
            return self.save_all(index);
        }
        let tx = self.conn.transaction()?;
        {
            let mut upsert = tx.prepare(
                "INSERT INTO entries(id, dir, name, size, mtime, flags) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(id) DO UPDATE SET dir=?2, name=?3, size=?4, mtime=?5, flags=?6",
            )?;
            let mut delete = tx.prepare("DELETE FROM entries WHERE id = ?1")?;
            let mut adddir = tx.prepare(
                "INSERT INTO dirs(id, parent, name) VALUES (?1, ?2, ?3)
                 ON CONFLICT(id) DO UPDATE SET parent=?2, name=?3",
            )?;
            for op in ops {
                match op {
                    DbOp::Upsert {
                        id,
                        dir,
                        name,
                        size,
                        mtime,
                        flags,
                    } => {
                        upsert.execute(params![
                            *id as i64,
                            *dir as i64,
                            name.as_slice(),
                            *size as i64,
                            mtime,
                            *flags as i64
                        ])?;
                    }
                    DbOp::Delete { id } => {
                        delete.execute(params![*id as i64])?;
                    }
                    DbOp::AddDir { id, parent, name } => {
                        let parent = if *parent == NO_DIR {
                            -1i64
                        } else {
                            *parent as i64
                        };
                        adddir.execute(params![*id as i64, parent, name.as_slice()])?;
                    }
                    DbOp::FullResave => unreachable!(),
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Size of the database plus its WAL, in bytes.
    pub fn size_on_disk(&self) -> u64 {
        let mut total = 0;
        for suffix in ["", "-wal", "-shm"] {
            let p = if suffix.is_empty() {
                self.path.clone()
            } else {
                PathBuf::from(format!("{}{}", self.path.display(), suffix))
            };
            if let Ok(m) = std::fs::metadata(&p) {
                total += m.len();
            }
        }
        total
    }

    pub fn checkpoint(&self) -> Result<()> {
        self.conn
            .pragma_update(None, "wal_checkpoint", "TRUNCATE")?;
        Ok(())
    }
}

/// File names are arbitrary bytes on Linux, so they are stored (and read back)
/// without a UTF-8 round trip.
fn row_bytes(row: &rusqlite::Row<'_>, idx: usize) -> Result<Vec<u8>> {
    Ok(match row.get_ref(idx)? {
        rusqlite::types::ValueRef::Text(b) | rusqlite::types::ValueRef::Blob(b) => b.to_vec(),
        other => anyhow::bail!("unexpected column type {:?}", other.data_type()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn sample_index() -> Index {
        let mut idx = Index::new();
        let d = idx.ensure_dir_path(Path::new("/home/u/docs"));
        idx.push_entry(d, b"a.txt", 0, 10, 100);
        idx.push_entry(d, b"b.txt", flags::DIR, 20, 200);
        idx.push_entry(d, &[b'w', 0xff, b'x'], 0, 30, 300);
        idx
    }

    #[test]
    fn save_and_load_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("index.db");
        let idx = sample_index();
        {
            let mut db = Db::open(&path).unwrap();
            db.save_all(&idx).unwrap();
            db.meta_set("roots", "/home/u").unwrap();
        }
        let db = Db::open(&path).unwrap();
        let mut loaded = Index::new();
        let n = db.load(&mut loaded).unwrap();
        assert_eq!(n, 3);
        assert_eq!(loaded.dir_count(), idx.dir_count());
        assert_eq!(loaded.path_of(0), idx.path_of(0));
        assert_eq!(loaded.name(2), &[b'w', 0xff, b'x']);
        assert_eq!(loaded.entry(1).flags, flags::DIR);
        assert_eq!(loaded.entry(2).size, 30);
        assert_eq!(db.meta_get("roots").unwrap().as_deref(), Some("/home/u"));
        assert!(db.size_on_disk() > 0);
    }

    #[test]
    fn incremental_ops_apply() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("index.db");
        let mut idx = sample_index();
        let mut db = Db::open(&path).unwrap();
        db.save_all(&idx).unwrap();

        idx.remove_entry(1);
        idx.update_entry(0, 99, 999);
        let dir = idx.entry(0).dir;
        db.apply_ops(
            &idx,
            &[
                DbOp::Delete { id: 1 },
                DbOp::Upsert {
                    id: 0,
                    dir,
                    name: b"a.txt".to_vec(),
                    size: 99,
                    mtime: 999,
                    flags: 0,
                },
            ],
        )
        .unwrap();

        // Ids are now sparse; load keeps them aligned with tombstones.
        let mut loaded = Index::new();
        assert_eq!(db.load(&mut loaded).unwrap(), 2);
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded.live_len(), 2);
        assert_eq!(loaded.entry(0).size, 99);
        assert_eq!(loaded.path_of(2), idx.path_of(2));

        loaded.compact();
        db.save_all(&loaded).unwrap();
        let mut again = Index::new();
        assert_eq!(db.load(&mut again).unwrap(), 2);
        assert_eq!(again.len(), 2);
    }
}
