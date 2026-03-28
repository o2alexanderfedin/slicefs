//! InodeMap: monotonically-allocated inode numbers mapped to Digest224 values.
//!
//! Provides an in-memory mapping from inode number (u64) to the CAS key (Digest224)
//! of its serialized inode data. Supports serialization and CAS-backed persistence
//! so the full map can be stored in / loaded from a blockset Dictionary.

use std::collections::BTreeMap;

use dedupfs_traits::digest::Digest224;
use dedupfs_traits::metadata::MetaError;
use blockset::{State, Tree, GetBytes, GetData, Dictionary};

/// In-memory inode number → Digest224 key mapping.
///
/// Inode 1 is reserved for the root directory and is never allocated by
/// `allocate_ino`. Allocation starts at 2 and increases monotonically.
pub struct InodeMap {
    map: BTreeMap<u64, Digest224>,
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

    /// Record a mapping from `ino` to the given Digest224 key.
    pub fn insert(&mut self, ino: u64, digest: Digest224) {
        self.map.insert(ino, digest);
    }

    /// Look up the Digest224 for `ino`.
    pub fn get(&self, ino: u64) -> Option<&Digest224> {
        self.map.get(&ino)
    }

    /// Remove and return the Digest224 for `ino`.
    pub fn remove(&mut self, ino: u64) -> Option<Digest224> {
        self.map.remove(&ino)
    }

    /// Iterate over all (ino, digest) pairs in sorted order.
    pub fn entries(&self) -> impl Iterator<Item = (&u64, &Digest224)> {
        self.map.iter()
    }

    /// Current `next_ino` value (for testing / serialization).
    pub fn next_ino(&self) -> u64 {
        self.next_ino
    }
}

impl Default for InodeMap {
    fn default() -> Self {
        Self::new()
    }
}

// ─── serialization ───────────────────────────────────────────────────────────
//
// Each entry is a 36-byte record:
//   [0..8]   ino        u64 LE
//   [8..36]  digest     Digest224 (7 × u32 LE = 28 bytes)
//
// After all entries the `next_ino` is NOT stored: on deserialization it is
// recovered as max(keys) + 1 (or 2 for an empty map).

/// Serialize `InodeMap` to a flat byte vector of 36-byte records.
pub fn serialize_inode_map(map: &InodeMap) -> Vec<u8> {
    let mut buf = Vec::with_capacity(map.map.len() * 36);
    for (ino, digest) in &map.map {
        buf.extend_from_slice(&ino.to_le_bytes());
        for word in digest {
            buf.extend_from_slice(&word.to_le_bytes());
        }
    }
    buf
}

/// Deserialize an `InodeMap` from 36-byte records.
///
/// Sets `next_ino` to `max(keys) + 1`, or 2 for an empty map.
/// Returns `MetaError::Corrupted` if the byte length is not a multiple of 36.
pub fn deserialize_inode_map(bytes: &[u8]) -> Result<InodeMap, MetaError> {
    if bytes.len() % 36 != 0 {
        return Err(MetaError::Corrupted(format!(
            "inode map: expected multiple-of-36 bytes, got {}",
            bytes.len()
        )));
    }

    let mut map = BTreeMap::new();
    let mut max_ino: u64 = 1; // root inode is 1

    let mut i = 0;
    while i < bytes.len() {
        let ino = u64::from_le_bytes(bytes[i..i + 8].try_into().unwrap());
        let mut digest: Digest224 = [0u32; 7];
        for (j, word) in digest.iter_mut().enumerate() {
            let offset = i + 8 + j * 4;
            *word = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
        }
        map.insert(ino, digest);
        if ino > max_ino {
            max_ino = ino;
        }
        i += 36;
    }

    Ok(InodeMap {
        map,
        next_ino: max_ino + 1,
    })
}

/// Serialize and store an `InodeMap` into a blockset Dictionary.
///
/// Returns the `Digest224` key that can later be passed to `load_inode_map`.
pub fn intern_inode_map(dict: &mut Dictionary, map: &InodeMap) -> Digest224 {
    let bytes = serialize_inode_map(map);
    State::push_all(dict, &bytes)
}

/// Retrieve and deserialize an `InodeMap` from a blockset Dictionary.
pub fn load_inode_map(dict: &Dictionary, key: &Digest224) -> Result<InodeMap, MetaError> {
    use dedupfs_traits::digest::from_digest224;
    let digest256 = from_digest224(key);
    let get_data = GetData::new(dict, &digest256);
    let bytes: Vec<u8> = GetBytes::new(get_data).collect();
    deserialize_inode_map(&bytes)
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use blockset::Dictionary;
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
        let digest: Digest224 = [1, 2, 3, 4, 5, 6, 7];
        m.insert(42, digest);
        assert_eq!(m.get(42), Some(&digest));
        assert_eq!(m.get(99), None);
    }

    #[test]
    fn test_remove() {
        let mut m = InodeMap::new();
        let digest: Digest224 = [10, 20, 30, 40, 50, 60, 70];
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
        let digest: Digest224 = [1, 2, 3, 4, 5, 6, 7];
        m.insert(10, digest);
        let bytes = serialize_inode_map(&m);
        assert_eq!(bytes.len(), 36);
        let recovered = deserialize_inode_map(&bytes).unwrap();
        assert_eq!(recovered.get(10), Some(&digest));
        assert_eq!(recovered.next_ino(), 11);
    }

    #[test]
    fn test_serialize_multiple_entries() {
        let mut m = InodeMap::new();
        let d1: Digest224 = [1, 0, 0, 0, 0, 0, 0];
        let d2: Digest224 = [0, 2, 0, 0, 0, 0, 0];
        let d3: Digest224 = [0, 0, 3, 0, 0, 0, 0];
        m.insert(2, d1);
        m.insert(5, d2);
        m.insert(3, d3);
        let bytes = serialize_inode_map(&m);
        assert_eq!(bytes.len(), 108); // 3 × 36
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
        let mut dict = Dictionary::default();
        let mut m = InodeMap::new();
        let d: Digest224 = [9, 8, 7, 6, 5, 4, 3];
        m.insert(2, d);
        m.insert(3, [1; 7]);
        let key = intern_inode_map(&mut dict, &m);
        let recovered = load_inode_map(&dict, &key).unwrap();
        assert_eq!(recovered.get(2), Some(&d));
        assert_eq!(recovered.get(3), Some(&[1u32; 7]));
        assert_eq!(recovered.next_ino(), 4);
    }

    #[test]
    fn test_next_ino_after_deserialize_is_max_plus_one() {
        let mut m = InodeMap::new();
        m.insert(2, [0u32; 7]);
        m.insert(10, [1u32; 7]);
        m.insert(7, [2u32; 7]);
        let bytes = serialize_inode_map(&m);
        let recovered = deserialize_inode_map(&bytes).unwrap();
        assert_eq!(recovered.next_ino(), 11);
    }

    proptest! {
        #[test]
        fn prop_serialize_round_trip(
            entries in prop::collection::vec((any::<u64>(), any::<[u32; 7]>()), 0..20)
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
