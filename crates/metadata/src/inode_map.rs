//! InodeMap: monotonically-allocated inode numbers mapped to Digest256 values.
//!
//! Provides an in-memory mapping from inode number (u64) to the CAS key (Digest256)
//! of its serialized inode data. The full map is stored as one node in the
//! chunk store, and loaded from it.

use std::collections::BTreeMap;

use slicefs_traits::digest::{Digest256, DIGEST_BYTES};
use slicefs_traits::metadata::MetaError;

/// In-memory inode number → Digest256 key mapping.
///
/// Inode 1 is reserved for the root directory and is never allocated by
/// `allocate_ino`. Allocation starts at 2 and increases monotonically.
pub struct InodeMap {
    map: BTreeMap<u64, Digest256>,
    next_ino: u64,
}

impl InodeMap {
    /// Create an empty InodeMap. `next_ino` starts at 2 (inode 1 = root).
    pub fn new() -> Self {
        Self {
            map: BTreeMap::new(),
            next_ino: 2,
        }
    }

    /// Allocate and return the next available inode number.
    pub fn allocate_ino(&mut self) -> u64 {
        let ino = self.next_ino;
        self.next_ino += 1;
        ino
    }

    /// Record a mapping from `ino` to the given Digest256 key.
    pub fn insert(&mut self, ino: u64, digest: Digest256) {
        self.map.insert(ino, digest);
    }

    /// Look up the Digest256 for `ino`.
    pub fn get(&self, ino: u64) -> Option<&Digest256> {
        self.map.get(&ino)
    }

    /// Remove and return the Digest256 for `ino`.
    pub fn remove(&mut self, ino: u64) -> Option<Digest256> {
        self.map.remove(&ino)
    }

    /// Iterate over all (ino, digest) pairs in sorted order.
    pub fn entries(&self) -> impl Iterator<Item = (&u64, &Digest256)> {
        self.map.iter()
    }

    /// Current `next_ino` value (for testing / serialization).
    pub fn next_ino(&self) -> u64 {
        self.next_ino
    }

    /// Directly set `next_ino` to the given value.
    ///
    /// Used during deserialization to restore the exact counter value from the
    /// root record, which may be higher than `max(keys) + 1` if inodes were
    /// deleted.  The caller must ensure `value >= max(keys) + 1`.
    pub fn set_next_ino(&mut self, value: u64) {
        self.next_ino = value;
    }
}

impl Default for InodeMap {
    fn default() -> Self {
        Self::new()
    }
}

// ─── serialization ───────────────────────────────────────────────────────────
//
// Each entry is a `RECORD`-byte (40) record:
//   [0..8]        ino        u64 LE
//   [8..RECORD]   digest     Digest256 (8 × u32 LE = `DIGEST_BYTES` bytes)
//
// After all entries the `next_ino` is NOT stored: on deserialization it is
// recovered as max(keys) + 1 (or 2 for an empty map).

const RECORD: usize = 8 + DIGEST_BYTES;

/// Serialize `InodeMap` to a flat byte vector of `RECORD`-byte records.
pub fn serialize_inode_map(map: &InodeMap) -> Vec<u8> {
    let mut buf = Vec::with_capacity(map.map.len() * RECORD);
    for (ino, digest) in &map.map {
        buf.extend_from_slice(&ino.to_le_bytes());
        for word in digest {
            buf.extend_from_slice(&word.to_le_bytes());
        }
    }
    buf
}

/// Deserialize an `InodeMap` from `RECORD`-byte records.
///
/// Sets `next_ino` to `max(keys) + 1`, or 2 for an empty map.
/// Returns `MetaError::Corrupted` if the byte length is not a multiple of `RECORD`.
pub fn deserialize_inode_map(bytes: &[u8]) -> Result<InodeMap, MetaError> {
    if bytes.len() % RECORD != 0 {
        return Err(MetaError::Corrupted(format!(
            "inode map: expected a multiple of {} bytes, got {}",
            RECORD, bytes.len()
        )));
    }

    let mut map = BTreeMap::new();
    let mut max_ino: u64 = 1; // root inode is 1

    let mut i = 0;
    while i < bytes.len() {
        let ino = u64::from_le_bytes(bytes[i..i + 8].try_into().unwrap());
        let mut digest: Digest256 = [0u32; 8];
        for (j, word) in digest.iter_mut().enumerate() {
            let offset = i + 8 + j * 4;
            *word = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
        }
        map.insert(ino, digest);
        if ino > max_ino {
            max_ino = ino;
        }
        i += RECORD;
    }

    Ok(InodeMap {
        map,
        next_ino: max_ino + 1,
    })
}

/// Serialize and store an `InodeMap` into any `StorageAdd` backend.
///
/// Returns the `Digest256` key that can later be passed to `load_inode_map`.
pub fn intern_inode_map(storage: &mut impl crate::store_io::NodeWrite, map: &InodeMap) -> Digest256 {
    let bytes = serialize_inode_map(map);
    (storage).put_node(&bytes)
}

/// Retrieve and deserialize an `InodeMap` from file-backed storage.
pub fn load_inode_map(io: &mut impl crate::store_io::NodeRead, key: &Digest256) -> Result<InodeMap, MetaError> {
    let bytes = (io).get_node(key)
        .ok_or_else(|| MetaError::Corrupted(format!("missing inode_map node {:?}", key)))?;
    deserialize_inode_map(&bytes)
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn test_new_starts_at_two() {
        let mut m = InodeMap::new();
        assert_eq!(m.allocate_ino(), 2);
        assert_eq!(m.allocate_ino(), 3);
    }

    #[test]
    fn test_insert_and_get() {
        let mut m = InodeMap::new();
        let digest: Digest256 = [1, 2, 3, 4, 5, 6, 7, 8];
        m.insert(42, digest);
        assert_eq!(m.get(42), Some(&digest));
        assert_eq!(m.get(99), None);
    }

    #[test]
    fn test_remove() {
        let mut m = InodeMap::new();
        let digest: Digest256 = [10, 20, 30, 40, 50, 60, 70, 80];
        m.insert(5, digest);
        assert_eq!(m.remove(5), Some(digest));
        assert_eq!(m.get(5), None);
        assert_eq!(m.remove(5), None);
    }

    #[test]
    fn test_serialize_empty_round_trip() {
        let m = InodeMap::new();
        let bytes = serialize_inode_map(&m);
        assert_eq!(bytes.len(), 0);
        let recovered = deserialize_inode_map(&bytes).unwrap();
        assert_eq!(recovered.next_ino(), 2); // empty → next_ino = max(1)+1 = 2
    }

    #[test]
    fn test_serialize_single_entry() {
        let mut m = InodeMap::new();
        let digest: Digest256 = [1, 2, 3, 4, 5, 6, 7, 8];
        m.insert(10, digest);
        let bytes = serialize_inode_map(&m);
        assert_eq!(bytes.len(), RECORD);
        let recovered = deserialize_inode_map(&bytes).unwrap();
        assert_eq!(recovered.get(10), Some(&digest));
        assert_eq!(recovered.next_ino(), 11);
    }

    #[test]
    fn test_serialize_multiple_entries() {
        let mut m = InodeMap::new();
        let d1: Digest256 = [1, 0, 0, 0, 0, 0, 0, 0];
        let d2: Digest256 = [0, 2, 0, 0, 0, 0, 0, 0];
        let d3: Digest256 = [0, 0, 3, 0, 0, 0, 0, 0];
        m.insert(2, d1);
        m.insert(5, d2);
        m.insert(3, d3);
        let bytes = serialize_inode_map(&m);
        assert_eq!(bytes.len(), 3 * RECORD);
        let recovered = deserialize_inode_map(&bytes).unwrap();
        assert_eq!(recovered.get(2), Some(&d1));
        assert_eq!(recovered.get(5), Some(&d2));
        assert_eq!(recovered.get(3), Some(&d3));
        assert_eq!(recovered.next_ino(), 6); // max(5)+1
    }

    #[test]
    fn test_deserialize_rejects_bad_length() {
        let bad = [0u8; 37];
        assert!(deserialize_inode_map(&bad).is_err());
    }

    #[test]
    fn test_intern_and_load_round_trip() {
        use crate::store_io::StoreIo;
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let mut io = StoreIo::open(dir.path()).unwrap();
        let mut m = InodeMap::new();
        let d: Digest256 = [9, 8, 7, 6, 5, 4, 3, 2];
        m.insert(2, d);
        m.insert(3, [1; 8]);
        let key = {
            let mut fsa = &mut io;
            intern_inode_map(&mut fsa, &m)
        };
        let recovered = load_inode_map(&mut io, &key).unwrap();
        assert_eq!(recovered.get(2), Some(&d));
        assert_eq!(recovered.get(3), Some(&[1u32; 8]));
        assert_eq!(recovered.next_ino(), 4);
    }

    #[test]
    fn test_next_ino_after_deserialize_is_max_plus_one() {
        let mut m = InodeMap::new();
        m.insert(2, [0u32; 8]);
        m.insert(10, [1u32; 8]);
        m.insert(7, [2u32; 8]);
        let bytes = serialize_inode_map(&m);
        let recovered = deserialize_inode_map(&bytes).unwrap();
        assert_eq!(recovered.next_ino(), 11);
    }

    proptest! {
        #[test]
        fn prop_serialize_round_trip(
            entries in prop::collection::vec((any::<u64>(), any::<[u32; 8]>()), 0..20)
        ) {
            let mut m = InodeMap::new();
            for (ino, digest) in &entries {
                m.insert(*ino, *digest);
            }
            let bytes = serialize_inode_map(&m);
            let recovered = deserialize_inode_map(&bytes).unwrap();
            for (ino, digest) in &entries {
                prop_assert_eq!(recovered.get(*ino), Some(digest));
            }
        }
    }
}
