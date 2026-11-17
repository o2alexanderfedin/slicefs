//! Per-entry directory CAS subtrees for SliceFS.
//!
//! Each directory entry is recorded in a compact "entry list", stored as one
//! node in the chunk store. The list contains every entry's:
//!   - `key`  — `ObjectKey` = BLAKE3-256 of the name (see [`name_key`])
//!   - `ino`  — `u64` inode number
//!   - `name` — UTF-8 name bytes
//!
//! The whole list is re-serialized only when entries are added or removed; each
//! re-serialization is cheap (compact binary format, only name strings + keys).
//!
//! Lookup is O(log n) via an in-memory BTreeMap keyed by `ObjectKey` name hash —
//! far better than O(n) linear scan and consistent with the per-entry-CAS design.
//!
//! Entry list wire format:
//! ```text
//! [count: u32 LE]
//! for each entry:
//!   [key:      tag + digest = 33 bytes]
//!   [ino:      u64 LE      = 8 bytes ]
//!   [name_len: u32 LE      = 4 bytes ]
//!   [name:     name_len bytes        ]
//! ```

use std::collections::BTreeMap;

use slicefs_traits::key::{ObjectKey, KEY_BYTES};
use slicefs_traits::metadata::{DirEntry, MetaError};

// ─── public primitives ────────────────────────────────────────────────────────

/// Compute the CAS key (name hash) for a directory entry name.
///
/// The same as [`name_key`]; `storage` is not used (the names are in the
/// entry list).
pub fn entry_key(_storage: &mut impl crate::store_io::NodeWrite, name: &str) -> ObjectKey {
    name_key(name)
}

// ─── entry list internal representation ──────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
struct EntryRecord {
    /// BLAKE3-256 of the entry name bytes.
    key: ObjectKey,
    /// Inode number this entry resolves to.
    ino: u64,
    /// Entry name (needed for listing).
    name: String,
}

// ─── entry list serialization ─────────────────────────────────────────────────

fn serialize_entry_list(entries: &BTreeMap<ObjectKey, EntryRecord>) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for e in entries.values() {
        // key: KEY_BYTES
        buf.extend_from_slice(&e.key.0);
        // ino: 8 bytes
        buf.extend_from_slice(&e.ino.to_le_bytes());
        // name: 4-byte length + bytes
        let name_bytes = e.name.as_bytes();
        buf.extend_from_slice(&(name_bytes.len() as u32).to_le_bytes());
        buf.extend_from_slice(name_bytes);
    }
    buf
}

fn deserialize_entry_list(bytes: &[u8]) -> Result<BTreeMap<ObjectKey, EntryRecord>, MetaError> {
    // An empty slice means zero entries (can happen for empty node).
    if bytes.is_empty() {
        return Ok(BTreeMap::new());
    }
    if bytes.len() < 4 {
        return Err(MetaError::Corrupted(format!(
            "entry list too short: {} bytes",
            bytes.len()
        )));
    }
    let count = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;
    let mut map = BTreeMap::new();
    let mut i = 4;

    for _ in 0..count {
        // key: KEY_BYTES
        if i + KEY_BYTES > bytes.len() {
            return Err(MetaError::Corrupted("entry list truncated at key".into()));
        }
        let key = ObjectKey::read(bytes, i);
        i += KEY_BYTES;

        // ino: 8 bytes
        if i + 8 > bytes.len() {
            return Err(MetaError::Corrupted("entry list truncated at ino".into()));
        }
        let ino = u64::from_le_bytes(bytes[i..i + 8].try_into().unwrap());
        i += 8;

        // name_len + name
        if i + 4 > bytes.len() {
            return Err(MetaError::Corrupted("entry list truncated at name_len".into()));
        }
        let name_len = u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap()) as usize;
        i += 4;
        if i + name_len > bytes.len() {
            return Err(MetaError::Corrupted("entry list truncated at name bytes".into()));
        }
        let name = String::from_utf8(bytes[i..i + name_len].to_vec())
            .map_err(|_| MetaError::Corrupted("entry name not utf-8".into()))?;
        i += name_len;

        map.insert(key, EntryRecord { key, ino, name });
    }
    Ok(map)
}

fn intern_entry_list(storage: &mut impl crate::store_io::NodeWrite, entries: &BTreeMap<ObjectKey, EntryRecord>) -> ObjectKey {
    let bytes = serialize_entry_list(entries);
    (storage).put_node(&bytes)
}

fn load_entry_list(
    io: &mut impl crate::store_io::NodeRead,
    list_digest: &ObjectKey,
) -> Result<BTreeMap<ObjectKey, EntryRecord>, MetaError> {
    let bytes = (io).get_node(list_digest)
        .ok_or_else(|| MetaError::Corrupted(format!("missing dir entry list {:?}", list_digest)))?;
    deserialize_entry_list(&bytes)
}

// ─── public directory operations ─────────────────────────────────────────────

/// Create the initial `.` and `..` entries for a new directory.
///
/// Both entries are stored as part of the entry list. Returns the entry list
/// `ObjectKey` which acts as the directory's identity in the metadata store.
pub fn create_dir_entries(
    storage: &mut impl crate::store_io::NodeWrite,
    self_ino: u64,
    parent_ino: u64,
) -> ObjectKey {
    let dot_key = entry_key(storage, ".");
    let dotdot_key = entry_key(storage, "..");

    let mut entries = BTreeMap::new();
    entries.insert(dot_key, EntryRecord { key: dot_key, ino: self_ino, name: ".".to_string() });
    entries.insert(dotdot_key, EntryRecord { key: dotdot_key, ino: parent_ino, name: "..".to_string() });

    intern_entry_list(storage, &entries)
}

/// Add a single directory entry `name → ino`.
///
/// Returns the new entry list `ObjectKey`.  Errors:
/// - `MetaError::InvalidName` if `name` is `.` or `..`
/// - `MetaError::AlreadyExists(0)` if the name is already in the directory
pub fn add_dir_entry(
    io: &mut impl crate::store_io::NodeWrite,
    dir_digest: &ObjectKey,
    name: &str,
    ino: u64,
) -> Result<ObjectKey, MetaError> {
    if name == "." || name == ".." {
        return Err(MetaError::InvalidName(name.to_string()));
    }
    // Read phase: load current entries (io borrow released after this)
    let mut entries = load_entry_list(io, dir_digest)?;
    // Write phase: create FSA and compute key + write new entry list
    let mut fsa = io;
    let key = entry_key(&mut fsa, name);
    if entries.contains_key(&key) {
        return Err(MetaError::AlreadyExists(ino));
    }
    entries.insert(key, EntryRecord { key, ino, name: name.to_string() });
    Ok(intern_entry_list(&mut fsa, &entries))
}

/// Remove a directory entry by name.
///
/// Returns the new entry list `ObjectKey`.  Errors:
/// - `MetaError::InvalidName` if `name` is `.` or `..`
/// - `MetaError::NotFound(0)` if the name is not in the directory
pub fn remove_dir_entry(
    io: &mut impl crate::store_io::NodeWrite,
    dir_digest: &ObjectKey,
    name: &str,
) -> Result<ObjectKey, MetaError> {
    if name == "." || name == ".." {
        return Err(MetaError::InvalidName(name.to_string()));
    }
    // Read phase
    let mut entries = load_entry_list(io, dir_digest)?;
    // Write phase: compute key and write updated list
    let mut fsa = io;
    let key = entry_key(&mut fsa, name);
    if entries.remove(&key).is_none() {
        return Err(MetaError::NotFound(0));
    }
    Ok(intern_entry_list(&mut fsa, &entries))
}

/// Point the `..` entry of a directory at `parent_ino` (used when a
/// directory moves to a new parent). Returns the new entry list `ObjectKey`.
pub fn set_dotdot(
    io: &mut impl crate::store_io::NodeWrite,
    dir_digest: &ObjectKey,
    parent_ino: u64,
) -> Result<ObjectKey, MetaError> {
    let mut entries = load_entry_list(io, dir_digest)?;
    let mut fsa = io;
    let key = entry_key(&mut fsa, "..");
    entries.insert(key, EntryRecord { key, ino: parent_ino, name: "..".to_string() });
    Ok(intern_entry_list(&mut fsa, &entries))
}

/// Look up a single directory entry by name. O(log n) in the number of entries.
///
/// Errors:
/// - `MetaError::NotFound(0)` if the name is not in the directory
pub fn lookup_dir_entry(
    io: &mut impl crate::store_io::NodeRead,
    dir_digest: &ObjectKey,
    name: &str,
) -> Result<u64, MetaError> {
    // Read phase: load entries
    let entries = load_entry_list(io, dir_digest)?;
    let key = name_key(name);
    entries
        .get(&key)
        .map(|r| r.ino)
        .ok_or(MetaError::NotFound(0))
}

/// List all entries in a directory.
pub fn list_dir_entries(
    io: &mut impl crate::store_io::NodeRead,
    dir_digest: &ObjectKey,
) -> Result<Vec<DirEntry>, MetaError> {
    let entries = load_entry_list(io, dir_digest)?;
    Ok(entries
        .into_values()
        .map(|r| DirEntry { name: r.name, ino: r.ino })
        .collect())
}

// ─── in-memory directory (cached between commits) ───────────────────────────

/// Hash a name to its entry key without writing anything to the store.
///
/// Gives the same `ObjectKey` as [`entry_key`]. The key is a lookup key,
/// not a content address: it must stay the same for the life of a store, so
/// it is always BLAKE3-256, never the store's write hash (which can change).
pub fn name_key(name: &str) -> ObjectKey {
    use chunk_store::ObjectHasher;
    crate::store_io::to_key(chunk_store::Blake3_256.id(name.as_bytes()))
}

/// A decoded directory entry list that the metadata store holds in memory.
///
/// Changes cost O(log n) and write nothing. [`DirList::intern`] writes the
/// list in the same wire format as before, so its digest is the same as the
/// digest of the old copy-on-every-change path.
#[derive(Debug, Clone, Default)]
pub struct DirList {
    entries: BTreeMap<ObjectKey, EntryRecord>,
}

impl DirList {
    /// A new directory with only `.` and `..`.
    pub fn new_dir(self_ino: u64, parent_ino: u64) -> Self {
        let mut d = DirList::default();
        d.put(".", self_ino);
        d.put("..", parent_ino);
        d
    }

    /// Read a stored entry list.
    pub fn load(io: &mut impl crate::store_io::NodeRead, digest: &ObjectKey) -> Result<Self, MetaError> {
        Ok(DirList { entries: load_entry_list(io, digest)? })
    }

    /// Write the list to the store and return its digest.
    pub fn intern(&self, storage: &mut impl crate::store_io::NodeWrite) -> ObjectKey {
        intern_entry_list(storage, &self.entries)
    }

    fn put(&mut self, name: &str, ino: u64) {
        let key = name_key(name);
        self.entries.insert(key, EntryRecord { key, ino, name: name.to_string() });
    }

    /// Add `name -> ino`. Same errors as [`add_dir_entry`].
    pub fn add(&mut self, name: &str, ino: u64) -> Result<(), MetaError> {
        if name == "." || name == ".." {
            return Err(MetaError::InvalidName(name.to_string()));
        }
        let key = name_key(name);
        if self.entries.contains_key(&key) {
            return Err(MetaError::AlreadyExists(ino));
        }
        self.entries.insert(key, EntryRecord { key, ino, name: name.to_string() });
        Ok(())
    }

    /// Remove `name`. Same errors as [`remove_dir_entry`].
    pub fn remove(&mut self, name: &str) -> Result<(), MetaError> {
        if name == "." || name == ".." {
            return Err(MetaError::InvalidName(name.to_string()));
        }
        self.entries.remove(&name_key(name)).map(|_| ()).ok_or(MetaError::NotFound(0))
    }

    /// Point `..` at `parent_ino`.
    pub fn set_dotdot(&mut self, parent_ino: u64) {
        self.put("..", parent_ino);
    }

    /// Find the inode of `name`.
    pub fn lookup(&self, name: &str) -> Result<u64, MetaError> {
        self.entries.get(&name_key(name)).map(|r| r.ino).ok_or(MetaError::NotFound(0))
    }

    /// All entries, in the same order as [`list_dir_entries`].
    pub fn list(&self) -> Vec<DirEntry> {
        self.entries.values().map(|r| DirEntry { name: r.name.clone(), ino: r.ino }).collect()
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store_io::StoreIo;
    use tempfile::TempDir;

    fn make_io() -> (TempDir, StoreIo) {
        let dir = TempDir::new().unwrap();
        let io = StoreIo::open(dir.path()).unwrap();
        (dir, io)
    }

    #[test]
    fn dir_list_interns_same_digest_as_per_change_path() {
        let (_dir, mut io) = make_io();
        let mut old = { let mut fsa = &mut io; create_dir_entries(&mut fsa, 5, 1) };
        let mut list = DirList::new_dir(5, 1);
        for (i, n) in ["a", "b.hpp", "c", "dir"].iter().enumerate() {
            old = add_dir_entry(&mut io, &old, n, 10 + i as u64).unwrap();
            list.add(n, 10 + i as u64).unwrap();
        }
        old = remove_dir_entry(&mut io, &old, "c").unwrap();
        list.remove("c").unwrap();
        old = set_dotdot(&mut io, &old, 7).unwrap();
        list.set_dotdot(7);
        let new = { let mut fsa = &mut io; list.intern(&mut fsa) };
        assert_eq!(old, new);
        assert_eq!(list.lookup("b.hpp").unwrap(), 11);
        assert!(matches!(list.lookup("c"), Err(MetaError::NotFound(_))));
        assert!(matches!(list.add("a", 99), Err(MetaError::AlreadyExists(_))));
        assert_eq!(list_dir_entries(&mut io, &new).unwrap(), list.list());
        let reloaded = DirList::load(&mut io, &new).unwrap();
        assert_eq!(reloaded.list(), list.list());
    }

    #[test]
    fn test_create_dir_entries_has_dot_and_dotdot() {
        let (_dir, mut io) = make_io();
        let dir_d = { let mut fsa = &mut io; create_dir_entries(&mut fsa, 1, 1) };
        let entries = list_dir_entries(&mut io, &dir_d).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"."), ". not found: {:?}", names);
        assert!(names.contains(&".."), ".. not found: {:?}", names);
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn test_create_dir_entries_dot_points_to_self() {
        let (_dir, mut io) = make_io();
        let dir_d = { let mut fsa = &mut io; create_dir_entries(&mut fsa, 5, 2) };
        let entries = list_dir_entries(&mut io, &dir_d).unwrap();
        let dot = entries.iter().find(|e| e.name == ".").unwrap();
        let dotdot = entries.iter().find(|e| e.name == "..").unwrap();
        assert_eq!(dot.ino, 5);
        assert_eq!(dotdot.ino, 2);
    }

    #[test]
    fn test_add_dir_entry_appears_in_list() {
        let (_dir, mut io) = make_io();
        let dir_d = { let mut fsa = &mut io; create_dir_entries(&mut fsa, 1, 1) };
        let dir_d2 = add_dir_entry(&mut io, &dir_d, "hello", 42).unwrap();
        let entries = list_dir_entries(&mut io, &dir_d2).unwrap();
        let e = entries.iter().find(|e| e.name == "hello").unwrap();
        assert_eq!(e.ino, 42);
    }

    #[test]
    fn test_add_dir_entry_duplicate_rejected() {
        let (_dir, mut io) = make_io();
        let dir_d = { let mut fsa = &mut io; create_dir_entries(&mut fsa, 1, 1) };
        let dir_d2 = add_dir_entry(&mut io, &dir_d, "foo", 10).unwrap();
        let result = add_dir_entry(&mut io, &dir_d2, "foo", 11);
        assert!(matches!(result, Err(MetaError::AlreadyExists(_))));
    }

    #[test]
    fn test_lookup_finds_entry() {
        let (_dir, mut io) = make_io();
        let dir_d = { let mut fsa = &mut io; create_dir_entries(&mut fsa, 1, 1) };
        let dir_d2 = add_dir_entry(&mut io, &dir_d, "bar", 99).unwrap();
        let ino = lookup_dir_entry(&mut io, &dir_d2, "bar").unwrap();
        assert_eq!(ino, 99);
    }

    #[test]
    fn test_lookup_not_found() {
        let (_dir, mut io) = make_io();
        let dir_d = { let mut fsa = &mut io; create_dir_entries(&mut fsa, 1, 1) };
        let result = lookup_dir_entry(&mut io, &dir_d, "nonexistent");
        assert!(matches!(result, Err(MetaError::NotFound(_))));
    }

    #[test]
    fn test_remove_dir_entry_gone_from_list() {
        let (_dir, mut io) = make_io();
        let dir_d = { let mut fsa = &mut io; create_dir_entries(&mut fsa, 1, 1) };
        let dir_d2 = add_dir_entry(&mut io, &dir_d, "todelete", 77).unwrap();
        let dir_d3 = remove_dir_entry(&mut io, &dir_d2, "todelete").unwrap();
        let entries = list_dir_entries(&mut io, &dir_d3).unwrap();
        assert!(!entries.iter().any(|e| e.name == "todelete"));
    }

    #[test]
    fn test_remove_nonexistent_returns_not_found() {
        let (_dir, mut io) = make_io();
        let dir_d = { let mut fsa = &mut io; create_dir_entries(&mut fsa, 1, 1) };
        let result = remove_dir_entry(&mut io, &dir_d, "ghost");
        assert!(matches!(result, Err(MetaError::NotFound(_))));
    }

    #[test]
    fn test_remove_dot_rejected() {
        let (_dir, mut io) = make_io();
        let dir_d = { let mut fsa = &mut io; create_dir_entries(&mut fsa, 1, 1) };
        let result = remove_dir_entry(&mut io, &dir_d, ".");
        assert!(matches!(result, Err(MetaError::InvalidName(_))));
    }

    #[test]
    fn test_remove_dotdot_rejected() {
        let (_dir, mut io) = make_io();
        let dir_d = { let mut fsa = &mut io; create_dir_entries(&mut fsa, 1, 1) };
        let result = remove_dir_entry(&mut io, &dir_d, "..");
        assert!(matches!(result, Err(MetaError::InvalidName(_))));
    }

    #[test]
    fn test_add_dot_rejected() {
        let (_dir, mut io) = make_io();
        let dir_d = { let mut fsa = &mut io; create_dir_entries(&mut fsa, 1, 1) };
        let result = add_dir_entry(&mut io, &dir_d, ".", 5);
        assert!(matches!(result, Err(MetaError::InvalidName(_))));
    }

    #[test]
    fn test_multiple_entries_round_trip() {
        let (_dir, mut io) = make_io();
        let dir_d = { let mut fsa = &mut io; create_dir_entries(&mut fsa, 1, 1) };
        let dir_d = add_dir_entry(&mut io, &dir_d, "alpha", 2).unwrap();
        let dir_d = add_dir_entry(&mut io, &dir_d, "beta", 3).unwrap();
        let dir_d = add_dir_entry(&mut io, &dir_d, "gamma", 4).unwrap();
        let entries = list_dir_entries(&mut io, &dir_d).unwrap();
        assert_eq!(entries.len(), 5); // . + .. + 3
        let ino = lookup_dir_entry(&mut io, &dir_d, "beta").unwrap();
        assert_eq!(ino, 3);
    }

    #[test]
    fn test_original_entry_list_unchanged_after_add() {
        // The original dir_digest still resolves to the old list (CAS immutability).
        let (_dir, mut io) = make_io();
        let dir_d1 = { let mut fsa = &mut io; create_dir_entries(&mut fsa, 1, 1) };
        let dir_d2 = add_dir_entry(&mut io, &dir_d1, "x", 7).unwrap();
        assert_ne!(dir_d1, dir_d2);
        // old list still has only . and ..
        let old_entries = list_dir_entries(&mut io, &dir_d1).unwrap();
        assert_eq!(old_entries.len(), 2);
    }
}
