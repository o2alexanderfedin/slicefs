//! The old-to-new map of a rehash run: (node kind, old key) -> new key, so
//! that a value shared by many inodes, roots or snapshots is written once.
//!
//! Correctness never depends on it: a miss is a rewrite with the same
//! result (the key of a fresh write). Only the engine inserts, and only
//! after a successful translation, so a hit is always right. The map is
//! never trusted across runs: the caller gives a new directory each run.
//!
//! On disk: one file `MAP_FILE` of fixed [`SLOT`]-byte slots (kind byte, old
//! key, new key; kind 0 = empty), an open-addressing table with linear
//! probing, read and written at offsets. At half load it grows to twice the
//! slots by one sequential scan. In RAM: a direct-mapped cache of a fixed
//! slot count in front of the file (copies of whole slots, so it cannot be
//! wrong), allocated once. RAM does not grow with the entries.
//!
//! The node kind is part of the key: one old key can be both a file content
//! and a node (a file whose bytes equal a node), and each has its own
//! translation.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use slicefs_traits::key::{ObjectKey, KEY_BYTES};

/// Bytes of a slot: kind, old key, new key.
pub const SLOT: usize = 1 + 2 * KEY_BYTES;

/// Slots of the RAM cache of [`KeyMap::on_disk`] (about 1.1 MB).
pub const CACHE_SLOTS: usize = 1 << 14;

/// Slots of a new map file of [`KeyMap::on_disk`] (4.4 MB).
pub const START_SLOTS: u64 = 1 << 16;

/// The name of the map file in the map directory.
pub const MAP_FILE: &str = "map";

/// Bytes read at a time when the file grows.
const GROW_READ: usize = (64 << 10) / SLOT * SLOT;

/// The node kind of an entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    /// A byte-copied value: file content, a directory node, or a spilled
    /// xattr value.
    Value = 1,
    ManifestList = 2,
    /// A `u64 -> key` map of directory nodes.
    ValueMap = 3,
    // 4 is not used (it was the flat manifest map, now a tree).
    // 5 is not used (it was the inode map, removed in store format 8).
    // 6 is not used (it was the flat refcount table, now a tree).
    Root = 7,
    /// The prolly tree of manifest lists (`manifest_data`).
    ManifestTree = 8,
    /// The prolly tree of refcounts (`refcounts`).
    RefcountTree = 9,
    /// The prolly tree of xattrs by name (`xattr_data`).
    XattrTree = 10,
    /// The prolly tree of inline inode records (`inode_data`).
    InodeTree = 11,
}

const EMPTY: [u8; SLOT] = [0; SLOT];

fn slot(kind: Kind, old: &ObjectKey, new: &ObjectKey) -> [u8; SLOT] {
    let mut s = [0u8; SLOT];
    s[0] = kind as u8;
    s[1..1 + KEY_BYTES].copy_from_slice(&old.0);
    s[1 + KEY_BYTES..].copy_from_slice(&new.0);
    s
}

/// Does slot `s` hold the entry of `kind` and `old`?
fn is(s: &[u8; SLOT], kind: Kind, old: &ObjectKey) -> bool {
    s[0] == kind as u8 && s[1..1 + KEY_BYTES] == old.0
}

fn new_of(s: &[u8; SLOT]) -> ObjectKey {
    ObjectKey::read(s, 1 + KEY_BYTES)
}

/// The hash of an entry (digests are uniform; the kind moves it).
fn hash(kind: u8, old: &[u8]) -> u64 {
    let d = u64::from_le_bytes(old[1..9].try_into().unwrap());
    (d ^ u64::from(kind).wrapping_mul(0x9e37_79b9_7f4a_7c15)).wrapping_mul(0xff51_afd7_ed55_8ccd)
}

fn hash_slot(s: &[u8; SLOT]) -> u64 {
    hash(s[0], &s[1..1 + KEY_BYTES])
}

/// The on-disk table.
struct Table {
    dir: PathBuf,
    file: File,
    slots: u64,
}

impl Table {
    /// The slot of `kind` and `old`, or the empty slot where it goes.
    fn find(&self, kind: Kind, old: &ObjectKey, h: u64) -> io::Result<(u64, [u8; SLOT])> {
        let mut i = h & (self.slots - 1);
        let mut s = [0u8; SLOT];
        loop {
            self.file.read_exact_at(&mut s, i * SLOT as u64)?;
            if s[0] == 0 || is(&s, kind, old) {
                return Ok((i, s));
            }
            i = (i + 1) & (self.slots - 1);
        }
    }

    /// Put slot `s` into the empty slot of its probe chain in `file`.
    fn place(file: &File, slots: u64, s: &[u8; SLOT]) -> io::Result<()> {
        let mut i = hash_slot(s) & (slots - 1);
        let mut cur = [0u8; SLOT];
        loop {
            file.read_exact_at(&mut cur, i * SLOT as u64)?;
            if cur[0] == 0 {
                return file.write_all_at(s, i * SLOT as u64);
            }
            i = (i + 1) & (slots - 1);
        }
    }

    /// A file of twice the slots with the same entries, by one sequential
    /// scan, renamed over the old file.
    fn grow(&mut self) -> io::Result<()> {
        let slots = self.slots * 2;
        let tmp = self.dir.join(format!("{MAP_FILE}.grow"));
        let file = new_file(&tmp, slots)?;
        let mut buf = vec![0u8; GROW_READ];
        let total = self.slots * SLOT as u64;
        let mut off = 0u64;
        while off < total {
            let n = (total - off).min(buf.len() as u64) as usize;
            self.file.read_exact_at(&mut buf[..n], off)?;
            for s in buf[..n].as_chunks::<SLOT>().0 {
                if s[0] != 0 {
                    Self::place(&file, slots, s)?;
                }
            }
            off += n as u64;
        }
        std::fs::rename(&tmp, self.dir.join(MAP_FILE))?;
        self.file = file;
        self.slots = slots;
        Ok(())
    }
}

/// A new file of `slots` empty slots at `path` (truncated if it is there).
fn new_file(path: &Path, slots: u64) -> io::Result<File> {
    let f = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(path)?;
    f.set_len(slots * SLOT as u64)?;
    Ok(f)
}

fn check_pow2(what: &str, n: u64) -> io::Result<()> {
    if n < 2 || !n.is_power_of_two() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{what} {n}: not a power of two of at least 2")));
    }
    Ok(())
}

/// The old-to-new map (see the module doc).
pub struct KeyMap {
    /// `None`: cache only.
    table: Option<Table>,
    cache: Vec<[u8; SLOT]>,
    len: u64,
}

impl KeyMap {
    /// A map in `dir` with [`START_SLOTS`] slots and [`CACHE_SLOTS`] slots
    /// of cache.
    pub fn on_disk(dir: &Path) -> io::Result<KeyMap> {
        Self::on_disk_with(dir, START_SLOTS, CACHE_SLOTS)
    }

    /// A map in `dir` (made if missing; its `MAP_FILE` is made new) with
    /// `start_slots` slots in the file and `cache_slots` slots of cache
    /// (both powers of two, at least 2).
    pub fn on_disk_with(dir: &Path, start_slots: u64, cache_slots: usize) -> io::Result<KeyMap> {
        check_pow2("start slots", start_slots)?;
        check_pow2("cache slots", cache_slots as u64)?;
        std::fs::create_dir_all(dir)?;
        let file = new_file(&dir.join(MAP_FILE), start_slots)?;
        let table = Table { dir: dir.to_path_buf(), file, slots: start_slots };
        Ok(KeyMap { table: Some(table), cache: vec![EMPTY; cache_slots], len: 0 })
    }

    /// A map with only the RAM cache of [`CACHE_SLOTS`] slots and no file:
    /// an entry stays until another entry takes its cache slot.
    pub fn cache_only() -> KeyMap {
        KeyMap { table: None, cache: vec![EMPTY; CACHE_SLOTS], len: 0 }
    }

    fn cache_at(&self, h: u64) -> usize {
        h as usize & (self.cache.len() - 1)
    }

    /// The new key of `kind` and `old`, if the map has it.
    pub fn get(&mut self, kind: Kind, old: &ObjectKey) -> io::Result<Option<ObjectKey>> {
        let h = hash(kind as u8, &old.0);
        let c = self.cache_at(h);
        if is(&self.cache[c], kind, old) {
            return Ok(Some(new_of(&self.cache[c])));
        }
        let Some(t) = &self.table else {
            return Ok(None);
        };
        let (_, s) = t.find(kind, old, h)?;
        if s[0] == 0 {
            return Ok(None);
        }
        self.cache[c] = s;
        Ok(Some(new_of(&s)))
    }

    /// Map `kind` and `old` to `new` (an entry of the same kind and old key
    /// is replaced).
    pub fn insert(&mut self, kind: Kind, old: &ObjectKey, new: &ObjectKey) -> io::Result<()> {
        let s = slot(kind, old, new);
        let h = hash(kind as u8, &old.0);
        let c = self.cache_at(h);
        match self.table.as_mut() {
            Some(t) => {
                let (mut i, cur) = t.find(kind, old, h)?;
                if cur[0] == 0 {
                    if (self.len + 1) * 2 > t.slots {
                        t.grow()?;
                        i = t.find(kind, old, h)?.0;
                    }
                    self.len += 1;
                }
                t.file.write_all_at(&s, i * SLOT as u64)?;
            }
            None => {
                if self.cache[c][0] == 0 {
                    self.len += 1;
                }
            }
        }
        self.cache[c] = s;
        Ok(())
    }

    /// Entries in the file (cache only: cache slots in use).
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Slots of the file (0 with no file).
    pub fn slots(&self) -> u64 {
        self.table.as_ref().map_or(0, |t| t.slots)
    }

    /// Slots of the RAM cache (fixed).
    pub fn cache_slots(&self) -> usize {
        self.cache.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chunk_store::{Blake3_256, ObjectHasher};

    fn key(i: u32) -> ObjectKey {
        ObjectKey::new(0x1e, Blake3_256.digest(&i.to_le_bytes()))
    }

    fn new_key(i: u32) -> ObjectKey {
        ObjectKey::new(0xf0, Blake3_256.digest(&(i ^ 0x8000_0000).to_le_bytes()))
    }

    fn file_len(dir: &Path) -> u64 {
        std::fs::metadata(dir.join(MAP_FILE)).unwrap().len()
    }

    #[test]
    fn a_missing_key_is_none_and_an_inserted_key_is_found() {
        assert_eq!(SLOT, 1 + 2 * KEY_BYTES);
        let dir = tempfile::tempdir().unwrap();
        let mut m = KeyMap::on_disk(dir.path()).unwrap();
        assert_eq!(m.get(Kind::Value, &key(1)).unwrap(), None);
        m.insert(Kind::Value, &key(1), &new_key(1)).unwrap();
        assert_eq!(m.get(Kind::Value, &key(1)).unwrap(), Some(new_key(1)));
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn the_kind_is_part_of_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = KeyMap::on_disk(dir.path()).unwrap();
        let k = key(7);
        m.insert(Kind::Value, &k, &new_key(1)).unwrap();
        m.insert(Kind::ManifestList, &k, &new_key(2)).unwrap();
        assert_eq!(m.get(Kind::Value, &k).unwrap(), Some(new_key(1)));
        assert_eq!(m.get(Kind::ManifestList, &k).unwrap(), Some(new_key(2)));
        assert_eq!(m.get(Kind::Root, &k).unwrap(), None);
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn entries_survive_growth() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = KeyMap::on_disk_with(dir.path(), 64, 16).unwrap();
        for i in 0..1000 {
            m.insert(Kind::Value, &key(i), &new_key(i)).unwrap();
        }
        for i in 0..1000 {
            assert_eq!(m.get(Kind::Value, &key(i)).unwrap(), Some(new_key(i)), "entry {i}");
        }
        assert_eq!(m.len(), 1000);
        assert!(m.slots() >= 2048, "{} slots", m.slots());
        assert_eq!(file_len(dir.path()), m.slots() * SLOT as u64);
    }

    #[test]
    fn the_cache_stays_fixed_while_entries_grow() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = KeyMap::on_disk_with(dir.path(), 64, 16).unwrap();
        let mut seen = Vec::new();
        for (from, to) in [(0, 100), (100, 1000)] {
            for i in from..to {
                m.insert(Kind::Value, &key(i), &new_key(i)).unwrap();
            }
            assert_eq!(m.cache_slots(), 16);
            seen.push((m.slots(), file_len(dir.path())));
            for i in 0..to {
                assert_eq!(m.get(Kind::Value, &key(i)).unwrap(), Some(new_key(i)), "entry {i}");
            }
        }
        assert!(seen[1].0 > seen[0].0 && seen[1].1 > seen[0].1, "{seen:?}");
    }

    #[test]
    fn cache_only_forgets_but_never_lies() {
        let mut m = KeyMap::cache_only();
        for i in 0..10_000 {
            m.insert(Kind::Value, &key(i), &new_key(i)).unwrap();
        }
        let mut found = 0;
        for i in 0..10_000 {
            if let Some(n) = m.get(Kind::Value, &key(i)).unwrap() {
                assert_eq!(n, new_key(i), "never another value");
                found += 1;
            }
        }
        assert!(found > 0);
        assert_eq!(m.slots(), 0, "no file");
        assert_eq!(m.cache_slots(), CACHE_SLOTS);
    }
}
