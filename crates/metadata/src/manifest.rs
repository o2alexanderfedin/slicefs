//! File manifest storage for SliceFS.
//!
//! A file manifest is an ordered list of `ObjectKey` block hashes that together
//! compose a file's content.  The manifest is stored as a flat byte sequence
//! (`KEY_BYTES` = 33 bytes per block hash: tag + digest) as one node in the chunk store, with
//! a single `ObjectKey` key for the whole list.

use slicefs_traits::key::{ObjectKey, KEY_BYTES};
use slicefs_traits::metadata::MetaError;

/// Serialize and store an ordered list of block hashes in any `StorageAdd` backend.
///
/// Returns the `ObjectKey` key that can later be passed to `load_manifest`.
/// An empty block list is valid and produces a deterministic key for the empty
/// byte sequence.
pub fn intern_manifest(storage: &mut impl crate::store_io::NodeWrite, blocks: &[ObjectKey]) -> ObjectKey {
    let mut bytes = Vec::with_capacity(blocks.len() * KEY_BYTES);
    for block in blocks {
        bytes.extend_from_slice(&block.0);
    }
    (storage).put_node(&bytes)
}

/// Retrieve and deserialize a file manifest from file-backed storage.
///
/// Returns `MetaError::Corrupted` if the stored byte length is not a multiple of `KEY_BYTES`.
pub fn load_manifest(io: &mut impl crate::store_io::NodeRead, key: &ObjectKey) -> Result<Vec<ObjectKey>, MetaError> {
    let bytes = (io).get_node(key)
        .ok_or_else(|| MetaError::Corrupted(format!("missing manifest node {:?}", key)))?;

    if bytes.len() % KEY_BYTES != 0 {
        return Err(MetaError::Corrupted(format!(
            "manifest: expected a multiple of {} bytes, got {}",
            KEY_BYTES, bytes.len()
        )));
    }

    let count = bytes.len() / KEY_BYTES;
    let mut blocks = Vec::with_capacity(count);
    for i in 0..count {
        let off = i * KEY_BYTES;
        blocks.push(ObjectKey::read(&bytes, off));
    }
    Ok(blocks)
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
    fn test_empty_manifest_round_trip() {
        let (_dir, mut io) = make_io();
        let key = {
            let mut fsa = &mut io;
            intern_manifest(&mut fsa, &[])
        };
        let blocks = load_manifest(&mut io, &key).unwrap();
        assert!(blocks.is_empty());
    }

    #[test]
    fn test_single_block_round_trip() {
        let (_dir, mut io) = make_io();
        let block: ObjectKey = ObjectKey::example(1);
        let key = {
            let mut fsa = &mut io;
            intern_manifest(&mut fsa, &[block])
        };
        let blocks = load_manifest(&mut io, &key).unwrap();
        assert_eq!(blocks, vec![block]);
    }

    #[test]
    fn test_multiple_blocks_round_trip() {
        let (_dir, mut io) = make_io();
        let b1: ObjectKey = ObjectKey::example(0x01);
        let b2: ObjectKey = ObjectKey::example(0x02);
        let b3: ObjectKey = ObjectKey::example(0x03);
        let b4: ObjectKey = ObjectKey::example(0xAB);
        let blocks = vec![b1, b2, b3, b4];
        let key = {
            let mut fsa = &mut io;
            intern_manifest(&mut fsa, &blocks)
        };
        let recovered = load_manifest(&mut io, &key).unwrap();
        assert_eq!(recovered, blocks);
    }

    #[test]
    fn test_ordering_preserved() {
        let (_dir, mut io) = make_io();
        let blocks: Vec<ObjectKey> = (0..10).map(|i| ObjectKey::example(i as u32)).collect();
        let key = {
            let mut fsa = &mut io;
            intern_manifest(&mut fsa, &blocks)
        };
        let recovered = load_manifest(&mut io, &key).unwrap();
        assert_eq!(recovered, blocks);
    }

    #[test]
    fn test_two_different_manifests_produce_different_keys() {
        let (_dir, mut io) = make_io();
        let (k1, k2) = {
            let mut fsa = &mut io;
            let k1 = intern_manifest(&mut fsa, &[ObjectKey::example(1)]);
            let k2 = intern_manifest(&mut fsa, &[ObjectKey::example(2)]);
            (k1, k2)
        };
        assert_ne!(k1, k2);
    }

    #[test]
    fn test_empty_and_nonempty_keys_differ() {
        let (_dir, mut io) = make_io();
        let (k_empty, k_one) = {
            let mut fsa = &mut io;
            let k_empty = intern_manifest(&mut fsa, &[]);
            let k_one = intern_manifest(&mut fsa, &[ObjectKey::example(0)]);
            (k_empty, k_one)
        };
        assert_ne!(k_empty, k_one);
    }
}
