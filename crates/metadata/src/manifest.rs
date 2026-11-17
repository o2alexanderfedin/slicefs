//! File manifest storage for SliceFS.
//!
//! A file manifest is an ordered list of `Digest256` block hashes that together
//! compose a file's content.  The manifest is stored as a flat byte sequence
//! (`DIGEST_BYTES` = 32 bytes per block hash: 8 × u32 LE) as one node in the chunk store, with
//! a single `Digest256` key for the whole list.

use slicefs_traits::digest::{Digest256, DIGEST_BYTES};
use slicefs_traits::metadata::MetaError;

/// Serialize and store an ordered list of block hashes in any `StorageAdd` backend.
///
/// Returns the `Digest256` key that can later be passed to `load_manifest`.
/// An empty block list is valid and produces a deterministic key for the empty
/// byte sequence.
pub fn intern_manifest(storage: &mut impl crate::store_io::NodeWrite, blocks: &[Digest256]) -> Digest256 {
    let mut bytes = Vec::with_capacity(blocks.len() * DIGEST_BYTES);
    for block in blocks {
        for word in block {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
    }
    (storage).put_node(&bytes)
}

/// Retrieve and deserialize a file manifest from file-backed storage.
///
/// Returns `MetaError::Corrupted` if the stored byte length is not a multiple of `DIGEST_BYTES`.
pub fn load_manifest(io: &mut impl crate::store_io::NodeRead, key: &Digest256) -> Result<Vec<Digest256>, MetaError> {
    let bytes = (io).get_node(key)
        .ok_or_else(|| MetaError::Corrupted(format!("missing manifest node {:?}", key)))?;

    if bytes.len() % DIGEST_BYTES != 0 {
        return Err(MetaError::Corrupted(format!(
            "manifest: expected a multiple of {} bytes, got {}",
            DIGEST_BYTES, bytes.len()
        )));
    }

    let count = bytes.len() / DIGEST_BYTES;
    let mut blocks = Vec::with_capacity(count);
    for i in 0..count {
        let off = i * DIGEST_BYTES;
        let mut digest: Digest256 = [0u32; 8];
        for (j, word) in digest.iter_mut().enumerate() {
            let o = off + j * 4;
            *word = u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
        }
        blocks.push(digest);
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
        let block: Digest256 = [1, 2, 3, 4, 5, 6, 7, 8];
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
        let b1: Digest256 = [0x01; 8];
        let b2: Digest256 = [0x02; 8];
        let b3: Digest256 = [0x03; 8];
        let b4: Digest256 = [0xAB, 0xCD, 0xEF, 0x12, 0x34, 0x56, 0x78, 0x9A];
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
        let blocks: Vec<Digest256> = (0..10).map(|i| [i as u32; 8]).collect();
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
            let k1 = intern_manifest(&mut fsa, &[[1u32; 8]]);
            let k2 = intern_manifest(&mut fsa, &[[2u32; 8]]);
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
            let k_one = intern_manifest(&mut fsa, &[[0u32; 8]]);
            (k_empty, k_one)
        };
        assert_ne!(k_empty, k_one);
    }
}
