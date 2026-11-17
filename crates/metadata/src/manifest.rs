//! File manifest storage for SliceFS.
//!
//! A manifest here is the list of `ObjectKey`s that the metadata keeps for an
//! inode (`set_manifest` / `get_manifest`), serialized as `KEY_BYTES` (33:
//! tag + digest) per key and stored as one metadata node.
//!
//! This is not the manifest of the file content: the content of a file is a
//! tree of content-defined manifest nodes in the chunk store
//! (`chunk_store::file`, cut by `is_cut`), and a file's metadata keeps only
//! the key of that tree's root. Like every node, the bytes of this list are
//! stored in FastCDC chunks (`store_io::put_value`), so a long list is not
//! one large object.

use slicefs_traits::key::{ObjectKey, KEY_BYTES};
use slicefs_traits::metadata::MetaError;

/// Serialize an ordered list of keys and store it as a node (`NodeWrite`).
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

/// Read and deserialize a manifest node (`NodeRead`).
///
/// Returns `MetaError::Corrupted` if the stored byte length is not a multiple of `KEY_BYTES`.
pub fn load_manifest(io: &mut impl crate::store_io::NodeRead, key: &ObjectKey) -> Result<Vec<ObjectKey>, MetaError> {
    let bytes = (io).get_node(key)
        .map_err(|e| crate::store_io::node_err("manifest node", key, e))?;

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
