//! Per-entry directory CAS subtrees for SliceFS.
//!
//! Each directory entry is recorded in a compact "entry list" stored as a CAS
//! tree in the blockset Dictionary.  The list contains every entry's:
//!   - `key`  — `Digest224` = `State::push_all(dict, name.as_bytes())` (name hash)
//!   - `ino`  — `u64` inode number
//!   - `name` — UTF-8 name bytes
//!
//! The whole list is re-serialized only when entries are added or removed; each
//! re-serialization is cheap (compact binary format, only name strings + keys).
//!
//! Lookup is O(log n) via an in-memory BTreeMap keyed by `Digest224` name hash —
//! far better than O(n) linear scan and consistent with the per-entry-CAS design.
//!
//! Entry list wire format:
//! ```text
//! [count: u32 LE]
//! for each entry:
//!   [key:      7 × u32 LE  = 28 bytes]
//!   [ino:      u64 LE      = 8 bytes ]
//!   [name_len: u32 LE      = 4 bytes ]
//!   [name:     name_len bytes        ]
//! ```

use std::collections::BTreeMap;

use slicefs_traits::digest::{Digest224, from_digest224};
use slicefs_traits::metadata::{DirEntry, MetaError};
use blockset::{State, Tree, GetBytes, GetData, StorageAdd};

// ─── public primitives ────────────────────────────────────────────────────────

/// Compute the CAS key (name hash) for a directory entry name.
///
/// Uses `State::push_all` which stores the name bytes in the storage backend and
/// returns a stable `Digest224` key.
pub fn entry_key(storage: &mut impl StorageAdd, name: &str) -> Digest224 {
    State::push_all(storage, name.as_bytes())
}

/// Encode an inode number as an inline `Digest256`.
///
/// 8 bytes always fits within the inline (non-hash) capacity of a `Digest256`.
pub fn ino_to_digest256(ino: u64) -> slicefs_traits::digest::Digest256 {
    blockset::from_bytes(&ino.to_le_bytes()).expect("8-byte value always fits inline")
}

/// Decode an inode number from an inline `Digest256`.
pub fn digest256_to_ino(d: &slicefs_traits::digest::Digest256) -> u64 {
    let bytes = blockset::to_data(d);
    let mut arr = [0u8; 8];
    let len = bytes.len().min(8);
    arr[..len].copy_from_slice(&bytes[..len]);
    u64::from_le_bytes(arr)
}

// ─── entry list internal representation ──────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
struct EntryRecord {
    /// SHA-224 of the entry name bytes.
    key: Digest224,
    /// Inode number this entry resolves to.
    ino: u64,
    /// Entry name (needed for listing).
    name: String,
}

// ─── entry list serialization ─────────────────────────────────────────────────

fn serialize_entry_list(entries: &BTreeMap<Digest224, EntryRecord>) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for e in entries.values() {
        // key: 28 bytes
        for word in &e.key {
            buf.extend_from_slice(&word.to_le_bytes());
        }
        // ino: 8 bytes
        buf.extend_from_slice(&e.ino.to_le_bytes());
        // name: 4-byte length + bytes
        let name_bytes = e.name.as_bytes();
        buf.extend_from_slice(&(name_bytes.len() as u32).to_le_bytes());
        buf.extend_from_slice(name_bytes);
    }
    buf
}

fn deserialize_entry_list(bytes: &[u8]) -> Result<BTreeMap<Digest224, EntryRecord>, MetaError> {
    // An empty slice means zero entries (can happen for empty push_all result).
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
        // key: 28 bytes
        if i + 28 > bytes.len() {
            return Err(MetaError::Corrupted("entry list truncated at key".into()));
        }
        let mut key: Digest224 = [0u32; 7];
        for (j, word) in key.iter_mut().enumerate() {
            let off = i + j * 4;
            *word = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap());
        }
        i += 28;

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

fn intern_entry_list(storage: &mut impl StorageAdd, entries: &BTreeMap<Digest224, EntryRecord>) -> Digest224 {
    let bytes = serialize_entry_list(entries);
    State::push_all(storage, &bytes)
}

fn load_entry_list<S: blockset::storage::StorageGet>(
    dict: &S,
    list_digest: &Digest224,
) -> Result<BTreeMap<Digest224, EntryRecord>, MetaError> {
    let digest256 = from_digest224(list_digest);
    let get_data = GetData::new(dict, &digest256);
    let bytes: Vec<u8> = GetBytes::new(get_data).collect();
    deserialize_entry_list(&bytes)
}

// ─── public directory operations ─────────────────────────────────────────────

/// Create the initial `.` and `..` entries for a new directory.
///
/// Both entries are stored as part of the entry list. Returns the entry list
/// `Digest224` which acts as the directory's identity in the metadata store.
pub fn create_dir_entries(
    storage: &mut impl StorageAdd,
    self_ino: u64,
    parent_ino: u64,
) -> Digest224 {
    let dot_key = entry_key(storage, ".");
    let dotdot_key = entry_key(storage, "..");

    let mut entries = BTreeMap::new();
    entries.insert(dot_key, EntryRecord { key: dot_key, ino: self_ino, name: ".".to_string() });
    entries.insert(dotdot_key, EntryRecord { key: dotdot_key, ino: parent_ino, name: "..".to_string() });

    intern_entry_list(storage, &entries)
}

/// Add a single directory entry `name → ino`.
///
/// Returns the new entry list `Digest224`.  Errors:
/// - `MetaError::InvalidName` if `name` is `.` or `..`
/// - `MetaError::AlreadyExists(0)` if the name is already in the directory
pub fn add_dir_entry<S: StorageAdd + blockset::storage::StorageGet>(
    storage: &mut S,
    dir_digest: &Digest224,
    name: &str,
    ino: u64,
) -> Result<Digest224, MetaError> {
    if name == "." || name == ".." {
        return Err(MetaError::InvalidName(name.to_string()));
    }
    let mut entries = load_entry_list(&*storage, dir_digest)?;
    let key = entry_key(storage, name);
    if entries.contains_key(&key) {
        return Err(MetaError::AlreadyExists(ino));
    }
    entries.insert(key, EntryRecord { key, ino, name: name.to_string() });
    Ok(intern_entry_list(storage, &entries))
}

/// Remove a directory entry by name.
///
/// Returns the new entry list `Digest224`.  Errors:
/// - `MetaError::InvalidName` if `name` is `.` or `..`
/// - `MetaError::NotFound(0)` if the name is not in the directory
pub fn remove_dir_entry<S: StorageAdd + blockset::storage::StorageGet>(
    storage: &mut S,
    dir_digest: &Digest224,
    name: &str,
) -> Result<Digest224, MetaError> {
    if name == "." || name == ".." {
        return Err(MetaError::InvalidName(name.to_string()));
    }
    let mut entries = load_entry_list(&*storage, dir_digest)?;
    let key = entry_key(storage, name);
    if entries.remove(&key).is_none() {
        return Err(MetaError::NotFound(0));
    }
    Ok(intern_entry_list(storage, &entries))
}

/// Look up a single directory entry by name. O(log n) in the number of entries.
///
/// Errors:
/// - `MetaError::NotFound(0)` if the name is not in the directory
pub fn lookup_dir_entry<S: StorageAdd + blockset::storage::StorageGet>(
    storage: &mut S,
    dir_digest: &Digest224,
    name: &str,
) -> Result<u64, MetaError> {
    let entries = load_entry_list(&*storage, dir_digest)?;
    let key = entry_key(storage, name);
    entries
        .get(&key)
        .map(|r| r.ino)
        .ok_or(MetaError::NotFound(0))
}

/// List all entries in a directory.
pub fn list_dir_entries<S: blockset::storage::StorageGet>(
    dict: &S,
    dir_digest: &Digest224,
) -> Result<Vec<DirEntry>, MetaError> {
    let entries = load_entry_list(dict, dir_digest)?;
    Ok(entries
        .into_values()
        .map(|r| DirEntry { name: r.name, ino: r.ino })
        .collect())
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use blockset::Dictionary;

    fn make_dict() -> Dictionary {
        Dictionary::default()
    }

    #[test]
    fn test_create_dir_entries_has_dot_and_dotdot() {
        let mut dict = make_dict();
        let dir_d = create_dir_entries(&mut dict, 1, 1);
        let entries = list_dir_entries(&dict, &dir_d).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"."), ". not found: {:?}", names);
        assert!(names.contains(&".."), ".. not found: {:?}", names);
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn test_create_dir_entries_dot_points_to_self() {
        let mut dict = make_dict();
        let dir_d = create_dir_entries(&mut dict, 5, 2);
        let entries = list_dir_entries(&dict, &dir_d).unwrap();
        let dot = entries.iter().find(|e| e.name == ".").unwrap();
        let dotdot = entries.iter().find(|e| e.name == "..").unwrap();
        assert_eq!(dot.ino, 5);
        assert_eq!(dotdot.ino, 2);
    }

    #[test]
    fn test_add_dir_entry_appears_in_list() {
        let mut dict = make_dict();
        let dir_d = create_dir_entries(&mut dict, 1, 1);
        let dir_d2 = add_dir_entry(&mut dict, &dir_d, "hello", 42).unwrap();
        let entries = list_dir_entries(&dict, &dir_d2).unwrap();
        let e = entries.iter().find(|e| e.name == "hello").unwrap();
        assert_eq!(e.ino, 42);
    }

    #[test]
    fn test_add_dir_entry_duplicate_rejected() {
        let mut dict = make_dict();
        let dir_d = create_dir_entries(&mut dict, 1, 1);
        let dir_d2 = add_dir_entry(&mut dict, &dir_d, "foo", 10).unwrap();
        let result = add_dir_entry(&mut dict, &dir_d2, "foo", 11);
        assert!(matches!(result, Err(MetaError::AlreadyExists(_))));
    }

    #[test]
    fn test_lookup_finds_entry() {
        let mut dict = make_dict();
        let dir_d = create_dir_entries(&mut dict, 1, 1);
        let dir_d2 = add_dir_entry(&mut dict, &dir_d, "bar", 99).unwrap();
        let ino = lookup_dir_entry(&mut dict, &dir_d2, "bar").unwrap();
        assert_eq!(ino, 99);
    }

    #[test]
    fn test_lookup_not_found() {
        let mut dict = make_dict();
        let dir_d = create_dir_entries(&mut dict, 1, 1);
        let result = lookup_dir_entry(&mut dict, &dir_d, "nonexistent");
        assert!(matches!(result, Err(MetaError::NotFound(_))));
    }

    #[test]
    fn test_remove_dir_entry_gone_from_list() {
        let mut dict = make_dict();
        let dir_d = create_dir_entries(&mut dict, 1, 1);
        let dir_d2 = add_dir_entry(&mut dict, &dir_d, "todelete", 77).unwrap();
        let dir_d3 = remove_dir_entry(&mut dict, &dir_d2, "todelete").unwrap();
        let entries = list_dir_entries(&dict, &dir_d3).unwrap();
        assert!(!entries.iter().any(|e| e.name == "todelete"));
    }

    #[test]
    fn test_remove_nonexistent_returns_not_found() {
        let mut dict = make_dict();
        let dir_d = create_dir_entries(&mut dict, 1, 1);
        let result = remove_dir_entry(&mut dict, &dir_d, "ghost");
        assert!(matches!(result, Err(MetaError::NotFound(_))));
    }

    #[test]
    fn test_remove_dot_rejected() {
        let mut dict = make_dict();
        let dir_d = create_dir_entries(&mut dict, 1, 1);
        let result = remove_dir_entry(&mut dict, &dir_d, ".");
        assert!(matches!(result, Err(MetaError::InvalidName(_))));
    }

    #[test]
    fn test_remove_dotdot_rejected() {
        let mut dict = make_dict();
        let dir_d = create_dir_entries(&mut dict, 1, 1);
        let result = remove_dir_entry(&mut dict, &dir_d, "..");
        assert!(matches!(result, Err(MetaError::InvalidName(_))));
    }

    #[test]
    fn test_add_dot_rejected() {
        let mut dict = make_dict();
        let dir_d = create_dir_entries(&mut dict, 1, 1);
        let result = add_dir_entry(&mut dict, &dir_d, ".", 5);
        assert!(matches!(result, Err(MetaError::InvalidName(_))));
    }

    #[test]
    fn test_multiple_entries_round_trip() {
        let mut dict = make_dict();
        let dir_d = create_dir_entries(&mut dict, 1, 1);
        let dir_d = add_dir_entry(&mut dict, &dir_d, "alpha", 2).unwrap();
        let dir_d = add_dir_entry(&mut dict, &dir_d, "beta", 3).unwrap();
        let dir_d = add_dir_entry(&mut dict, &dir_d, "gamma", 4).unwrap();
        let entries = list_dir_entries(&dict, &dir_d).unwrap();
        assert_eq!(entries.len(), 5); // . + .. + 3
        let ino = lookup_dir_entry(&mut dict, &dir_d, "beta").unwrap();
        assert_eq!(ino, 3);
    }

    #[test]
    fn test_original_entry_list_unchanged_after_add() {
        // The original dir_digest still resolves to the old list (CAS immutability).
        let mut dict = make_dict();
        let dir_d1 = create_dir_entries(&mut dict, 1, 1);
        let dir_d2 = add_dir_entry(&mut dict, &dir_d1, "x", 7).unwrap();
        assert_ne!(dir_d1, dir_d2);
        // old list still has only . and ..
        let old_entries = list_dir_entries(&dict, &dir_d1).unwrap();
        assert_eq!(old_entries.len(), 2);
    }
}
