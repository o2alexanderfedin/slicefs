//! Extended attribute (xattr) storage for SliceFS.
//!
//! Xattrs are stored as a flat list of (name, value) pairs serialized as a
//! concatenated byte stream and pushed into the blockset Dictionary via
//! `State::push_all`. The returned `Digest224` acts as the identity key for the
//! entire xattr set of a given inode.
//!
//! Entry wire format (little-endian):
//! ```text
//! for each attribute:
//!   [name_len:  u32 LE]
//!   [name:      name_len bytes (UTF-8)]
//!   [value_len: u32 LE]
//!   [value:     value_len bytes]
//! ```
//! An empty attribute set is represented by an empty byte sequence and is stored
//! in the Dictionary as the canonical empty-content `Digest224`.

use blockset::{Io, State, StorageAdd, Tree, file_storage_get};
use slicefs_traits::digest::Digest224;
use slicefs_traits::metadata::MetaError;

// ─── serialization helpers ────────────────────────────────────────────────────

fn serialize_xattrs(xattrs: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut buf = Vec::new();
    for (name, value) in xattrs {
        let name_bytes = name.as_bytes();
        buf.extend_from_slice(&(name_bytes.len() as u32).to_le_bytes());
        buf.extend_from_slice(name_bytes);
        buf.extend_from_slice(&(value.len() as u32).to_le_bytes());
        buf.extend_from_slice(value);
    }
    buf
}

fn deserialize_xattrs(bytes: &[u8]) -> Result<Vec<(String, Vec<u8>)>, MetaError> {
    let mut result = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        // name_len
        if i + 4 > bytes.len() {
            return Err(MetaError::Corrupted("xattr: truncated at name_len".into()));
        }
        let name_len = u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap()) as usize;
        i += 4;

        // name bytes
        if i + name_len > bytes.len() {
            return Err(MetaError::Corrupted(
                "xattr: truncated at name bytes".into(),
            ));
        }
        let name = String::from_utf8(bytes[i..i + name_len].to_vec())
            .map_err(|_| MetaError::Corrupted("xattr name not utf-8".into()))?;
        i += name_len;

        // value_len
        if i + 4 > bytes.len() {
            return Err(MetaError::Corrupted("xattr: truncated at value_len".into()));
        }
        let value_len = u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap()) as usize;
        i += 4;

        // value bytes
        if i + value_len > bytes.len() {
            return Err(MetaError::Corrupted(
                "xattr: truncated at value bytes".into(),
            ));
        }
        let value = bytes[i..i + value_len].to_vec();
        i += value_len;

        result.push((name, value));
    }

    Ok(result)
}

// ─── public CAS-backed functions ──────────────────────────────────────────────

/// Serialize all xattr pairs and store them in any `StorageAdd` backend.
///
/// Returns the `Digest224` key that can later be passed to `load_xattrs`.
/// An empty list serializes to an empty byte sequence which produces a
/// canonical empty-content `Digest224`.
pub fn intern_xattrs(storage: &mut impl StorageAdd, xattrs: &[(String, Vec<u8>)]) -> Digest224 {
    let bytes = serialize_xattrs(xattrs);
    State::push_all(storage, &bytes)
}

/// Retrieve and deserialize all xattr pairs from file-backed storage.
///
/// Returns `MetaError::Corrupted` if the stored bytes cannot be parsed.
pub fn load_xattrs(io: &mut impl Io, key: &Digest224) -> Result<Vec<(String, Vec<u8>)>, MetaError> {
    let bytes = file_storage_get(io, key)
        .ok_or_else(|| MetaError::Corrupted(format!("missing xattr node {:?}", key)))?;
    deserialize_xattrs(&bytes)
}

// ─── in-memory list helpers ───────────────────────────────────────────────────

/// Insert or replace the value for `name` in an xattr list.
pub fn set_xattr_entry(xattrs: &mut Vec<(String, Vec<u8>)>, name: &str, value: &[u8]) {
    if let Some(entry) = xattrs.iter_mut().find(|(k, _)| k == name) {
        entry.1 = value.to_vec();
    } else {
        xattrs.push((name.to_string(), value.to_vec()));
    }
}

/// Look up the value for `name` in an xattr list.
pub fn get_xattr_entry(xattrs: &[(String, Vec<u8>)], name: &str) -> Option<Vec<u8>> {
    xattrs
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.clone())
}

/// Return all attribute names in an xattr list.
pub fn list_xattr_names(xattrs: &[(String, Vec<u8>)]) -> Vec<String> {
    xattrs.iter().map(|(k, _)| k.clone()).collect()
}

/// Remove the entry for `name` from an xattr list.
///
/// Returns `true` if an entry was removed, `false` if `name` was not present.
pub fn remove_xattr_entry(xattrs: &mut Vec<(String, Vec<u8>)>, name: &str) -> bool {
    let before = xattrs.len();
    xattrs.retain(|(k, _)| k != name);
    xattrs.len() < before
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store_io::StoreIo;
    use blockset::FileStorageAdd;
    use tempfile::TempDir;

    fn make_io() -> (TempDir, StoreIo) {
        let dir = TempDir::new().unwrap();
        let io = StoreIo::new(dir.path());
        (dir, io)
    }

    // ── serialize/deserialize unit tests ─────────────────────────────────────

    #[test]
    fn test_empty_round_trip() {
        let xattrs: Vec<(String, Vec<u8>)> = vec![];
        let bytes = serialize_xattrs(&xattrs);
        assert!(bytes.is_empty());
        let recovered = deserialize_xattrs(&bytes).unwrap();
        assert!(recovered.is_empty());
    }

    #[test]
    fn test_single_attr_round_trip() {
        let xattrs = vec![("user.key".to_string(), b"hello".to_vec())];
        let bytes = serialize_xattrs(&xattrs);
        let recovered = deserialize_xattrs(&bytes).unwrap();
        assert_eq!(recovered, xattrs);
    }

    #[test]
    fn test_multiple_attrs_round_trip() {
        let xattrs = vec![
            ("user.a".to_string(), b"alpha".to_vec()),
            ("user.b".to_string(), b"beta".to_vec()),
            (
                "security.selinux".to_string(),
                b"system_u:object_r:unlabeled_t:s0".to_vec(),
            ),
        ];
        let bytes = serialize_xattrs(&xattrs);
        let recovered = deserialize_xattrs(&bytes).unwrap();
        assert_eq!(recovered, xattrs);
    }

    #[test]
    fn test_binary_value_round_trip() {
        let value: Vec<u8> = (0u8..=255).collect();
        let xattrs = vec![("user.binary".to_string(), value)];
        let bytes = serialize_xattrs(&xattrs);
        let recovered = deserialize_xattrs(&bytes).unwrap();
        assert_eq!(recovered, xattrs);
    }

    #[test]
    fn test_large_value_round_trip() {
        // Value larger than 31 bytes exercises the CAS tree storage path.
        let value: Vec<u8> = vec![0xAB; 100];
        let xattrs = vec![("user.large".to_string(), value)];
        let bytes = serialize_xattrs(&xattrs);
        let recovered = deserialize_xattrs(&bytes).unwrap();
        assert_eq!(recovered, xattrs);
    }

    #[test]
    fn test_empty_value() {
        let xattrs = vec![("user.empty".to_string(), vec![])];
        let bytes = serialize_xattrs(&xattrs);
        let recovered = deserialize_xattrs(&bytes).unwrap();
        assert_eq!(recovered, xattrs);
    }

    // ── CAS-backed intern/load tests ──────────────────────────────────────────

    #[test]
    fn test_intern_load_empty() {
        let (_dir, mut io) = make_io();
        let xattrs: Vec<(String, Vec<u8>)> = vec![];
        let key = {
            let mut fsa = FileStorageAdd::new(&mut io);
            intern_xattrs(&mut fsa, &xattrs)
        };
        let recovered = load_xattrs(&mut io, &key).unwrap();
        assert!(recovered.is_empty());
    }

    #[test]
    fn test_intern_load_single_attr() {
        let (_dir, mut io) = make_io();
        let xattrs = vec![("user.test".to_string(), b"testvalue".to_vec())];
        let key = {
            let mut fsa = FileStorageAdd::new(&mut io);
            intern_xattrs(&mut fsa, &xattrs)
        };
        let recovered = load_xattrs(&mut io, &key).unwrap();
        assert_eq!(recovered, xattrs);
    }

    #[test]
    fn test_intern_load_large_value() {
        // > 31 bytes triggers the CAS tree path in blockset
        let (_dir, mut io) = make_io();
        let value: Vec<u8> = (0..=127u8).collect(); // 128 bytes
        let xattrs = vec![("user.big".to_string(), value)];
        let key = {
            let mut fsa = FileStorageAdd::new(&mut io);
            intern_xattrs(&mut fsa, &xattrs)
        };
        let recovered = load_xattrs(&mut io, &key).unwrap();
        assert_eq!(recovered, xattrs);
    }

    #[test]
    fn test_intern_load_multiple_attrs() {
        let (_dir, mut io) = make_io();
        let xattrs = vec![
            ("user.a".to_string(), b"val-a".to_vec()),
            ("user.b".to_string(), b"val-b".to_vec()),
            ("user.c".to_string(), b"val-c".to_vec()),
        ];
        let key = {
            let mut fsa = FileStorageAdd::new(&mut io);
            intern_xattrs(&mut fsa, &xattrs)
        };
        let recovered = load_xattrs(&mut io, &key).unwrap();
        assert_eq!(recovered, xattrs);
    }

    // ── in-memory list helper tests ───────────────────────────────────────────

    #[test]
    fn test_set_xattr_entry_insert() {
        let mut xattrs = vec![];
        set_xattr_entry(&mut xattrs, "user.x", b"hello");
        assert_eq!(xattrs.len(), 1);
        assert_eq!(xattrs[0], ("user.x".to_string(), b"hello".to_vec()));
    }

    #[test]
    fn test_set_xattr_entry_replace() {
        let mut xattrs = vec![("user.x".to_string(), b"old".to_vec())];
        set_xattr_entry(&mut xattrs, "user.x", b"new");
        assert_eq!(xattrs.len(), 1);
        assert_eq!(xattrs[0].1, b"new".to_vec());
    }

    #[test]
    fn test_get_xattr_entry_found() {
        let xattrs = vec![("user.k".to_string(), b"v".to_vec())];
        let val = get_xattr_entry(&xattrs, "user.k");
        assert_eq!(val, Some(b"v".to_vec()));
    }

    #[test]
    fn test_get_xattr_entry_not_found() {
        let xattrs = vec![("user.k".to_string(), b"v".to_vec())];
        let val = get_xattr_entry(&xattrs, "user.missing");
        assert_eq!(val, None);
    }

    #[test]
    fn test_list_xattr_names() {
        let xattrs = vec![
            ("user.a".to_string(), b"1".to_vec()),
            ("user.b".to_string(), b"2".to_vec()),
        ];
        let names = list_xattr_names(&xattrs);
        assert_eq!(names, vec!["user.a", "user.b"]);
    }

    #[test]
    fn test_list_xattr_names_empty() {
        let xattrs: Vec<(String, Vec<u8>)> = vec![];
        let names = list_xattr_names(&xattrs);
        assert!(names.is_empty());
    }

    #[test]
    fn test_remove_xattr_entry_removes() {
        let mut xattrs = vec![
            ("user.a".to_string(), b"1".to_vec()),
            ("user.b".to_string(), b"2".to_vec()),
        ];
        let removed = remove_xattr_entry(&mut xattrs, "user.a");
        assert!(removed);
        assert_eq!(xattrs.len(), 1);
        assert_eq!(xattrs[0].0, "user.b");
    }

    #[test]
    fn test_remove_xattr_entry_not_found_returns_false() {
        let mut xattrs = vec![("user.a".to_string(), b"1".to_vec())];
        let removed = remove_xattr_entry(&mut xattrs, "user.missing");
        assert!(!removed);
        assert_eq!(xattrs.len(), 1);
    }

    #[test]
    fn test_multiple_attrs_coexist_independently() {
        let (_dir, mut io) = make_io();
        let xattrs1 = vec![("user.x".to_string(), b"foo".to_vec())];
        let xattrs2 = vec![
            ("user.x".to_string(), b"foo".to_vec()),
            ("user.y".to_string(), b"bar".to_vec()),
        ];

        let (key1, key2) = {
            let mut fsa = FileStorageAdd::new(&mut io);
            let k1 = intern_xattrs(&mut fsa, &xattrs1);
            let k2 = intern_xattrs(&mut fsa, &xattrs2);
            (k1, k2)
        };

        // Two different xattr sets produce different keys
        assert_ne!(key1, key2);

        let rec1 = load_xattrs(&mut io, &key1).unwrap();
        let rec2 = load_xattrs(&mut io, &key2).unwrap();
        assert_eq!(rec1.len(), 1);
        assert_eq!(rec2.len(), 2);
    }
}
