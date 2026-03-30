//! File manifest storage for SliceFS.
//!
//! A file manifest is an ordered list of `Digest224` block hashes that together
//! compose a file's content.  The manifest is stored as a flat byte sequence
//! (28 bytes per block hash: 7 × u32 LE) in the blockset Dictionary via
//! `State::push_all`, which returns a single `Digest224` key for the whole list.

use slicefs_traits::digest::{Digest224, from_digest224};
use slicefs_traits::metadata::MetaError;
use blockset::{State, Tree, GetBytes, GetData, Dictionary, StorageAdd};

/// Serialize and store an ordered list of block hashes in any `StorageAdd` backend.
///
/// Returns the `Digest224` key that can later be passed to `load_manifest`.
/// An empty block list is valid and produces a deterministic key for the empty
/// byte sequence.
pub fn intern_manifest(storage: &mut impl StorageAdd, blocks: &[Digest224]) -> Digest224 {
    let mut bytes = Vec::with_capacity(blocks.len() * 28);
    for block in blocks {
        for word in block {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
    }
    State::push_all(storage, &bytes)
}

/// Retrieve and deserialize a file manifest from the Dictionary.
///
/// Returns `MetaError::Corrupted` if the stored byte length is not a multiple of 28.
pub fn load_manifest(dict: &Dictionary, key: &Digest224) -> Result<Vec<Digest224>, MetaError> {
    let digest256 = from_digest224(key);
    let get_data = GetData::new(dict, &digest256);
    let bytes: Vec<u8> = GetBytes::new(get_data).collect();

    if bytes.len() % 28 != 0 {
        return Err(MetaError::Corrupted(format!(
            "manifest: expected multiple-of-28 bytes, got {}",
            bytes.len()
        )));
    }

    let count = bytes.len() / 28;
    let mut blocks = Vec::with_capacity(count);
    for i in 0..count {
        let off = i * 28;
        let mut digest: Digest224 = [0u32; 7];
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
    use blockset::Dictionary;

    #[test]
    fn test_empty_manifest_round_trip() {
        let mut dict = Dictionary::default();
        let key = intern_manifest(&mut dict, &[]);
        let blocks = load_manifest(&dict, &key).unwrap();
        assert!(blocks.is_empty());
    }

    #[test]
    fn test_single_block_round_trip() {
        let mut dict = Dictionary::default();
        let block: Digest224 = [1, 2, 3, 4, 5, 6, 7];
        let key = intern_manifest(&mut dict, &[block]);
        let blocks = load_manifest(&dict, &key).unwrap();
        assert_eq!(blocks, vec![block]);
    }

    #[test]
    fn test_multiple_blocks_round_trip() {
        let mut dict = Dictionary::default();
        let b1: Digest224 = [0x01; 7];
        let b2: Digest224 = [0x02; 7];
        let b3: Digest224 = [0x03; 7];
        let b4: Digest224 = [0xAB, 0xCD, 0xEF, 0x12, 0x34, 0x56, 0x78];
        let blocks = vec![b1, b2, b3, b4];
        let key = intern_manifest(&mut dict, &blocks);
        let recovered = load_manifest(&dict, &key).unwrap();
        assert_eq!(recovered, blocks);
    }

    #[test]
    fn test_ordering_preserved() {
        let mut dict = Dictionary::default();
        let blocks: Vec<Digest224> = (0..10).map(|i| [i as u32; 7]).collect();
        let key = intern_manifest(&mut dict, &blocks);
        let recovered = load_manifest(&dict, &key).unwrap();
        assert_eq!(recovered, blocks);
    }

    #[test]
    fn test_two_different_manifests_produce_different_keys() {
        let mut dict = Dictionary::default();
        let k1 = intern_manifest(&mut dict, &[[1u32; 7]]);
        let k2 = intern_manifest(&mut dict, &[[2u32; 7]]);
        assert_ne!(k1, k2);
    }

    #[test]
    fn test_empty_and_nonempty_keys_differ() {
        let mut dict = Dictionary::default();
        let k_empty = intern_manifest(&mut dict, &[]);
        let k_one = intern_manifest(&mut dict, &[[0u32; 7]]);
        assert_ne!(k_empty, k_one);
    }
}
