//! `FixedChunker`: implements `Chunker` with fixed-size blocks.

use slicefs_traits::{chunk::{Chunk, Chunker}, error::CasError};

/// Fixed-size block chunker.
///
/// Splits a buffer into contiguous, equal-size blocks. The last block may be
/// smaller than `block_size` if the input length is not evenly divisible.
///
/// An empty input returns `Ok(vec![])` — this is the defined behaviour for
/// zero-length files; no chunk is emitted.
///
/// The `Default` implementation uses a 4096-byte block size, matching common
/// filesystem page sizes.
pub struct FixedChunker {
    block_size: usize,
}

impl FixedChunker {
    /// Create a new `FixedChunker` with the given block size.
    ///
    /// # Panics
    /// Panics if `block_size` is zero.
    pub fn new(block_size: usize) -> Self {
        assert!(block_size > 0, "block_size must be greater than zero");
        Self { block_size }
    }
}

impl Default for FixedChunker {
    fn default() -> Self {
        Self::new(4096)
    }
}

impl Chunker for FixedChunker {
    fn chunk(&self, data: &[u8]) -> Result<Vec<Chunk>, CasError> {
        if data.is_empty() {
            return Ok(vec![]);
        }

        let mut chunks = Vec::new();
        let mut offset = 0;

        while offset < data.len() {
            let end = (offset + self.block_size).min(data.len());
            chunks.push(Chunk {
                offset,
                data: data[offset..end].to_vec(),
            });
            offset = end;
        }

        Ok(chunks)
    }

    fn strategy_id(&self) -> &'static str {
        // Use a leaked string so we can return `&'static str` from a runtime value.
        // This is acceptable because strategy_id() is called rarely and the string
        // lives for the lifetime of the program.
        //
        // NOTE: This is a design limitation of the trait requiring `&'static str`.
        // For the default 4096-byte chunker the id is always "fixed-4096".
        // Custom sizes fall back to a generic label; in production this would
        // need a different approach (e.g., the trait should return `String`).
        match self.block_size {
            4096 => "fixed-4096",
            8192 => "fixed-8192",
            _ => "fixed-custom",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /// Second chunker: 8192-byte blocks. Used for swap / trait-object test.
    struct LargeBlockChunker(FixedChunker);
    impl LargeBlockChunker {
        fn new() -> Self {
            Self(FixedChunker::new(8192))
        }
    }
    impl Chunker for LargeBlockChunker {
        fn chunk(&self, data: &[u8]) -> Result<Vec<Chunk>, CasError> {
            self.0.chunk(data)
        }
        fn strategy_id(&self) -> &'static str {
            "fixed-8192"
        }
    }

    fn chunk_with(chunker: &dyn Chunker, data: &[u8]) -> Vec<Chunk> {
        chunker.chunk(data).expect("chunk() should not fail")
    }

    fn concat_chunks(chunks: &[Chunk]) -> Vec<u8> {
        let mut out = Vec::new();
        for c in chunks {
            out.extend_from_slice(&c.data);
        }
        out
    }

    // -----------------------------------------------------------------------
    // FixedChunker unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn chunks_into_equal_size_pieces() {
        let chunker = FixedChunker::new(4);
        let data = b"12345678"; // 8 bytes, 4-byte blocks
        let chunks = chunker.chunk(data).unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].data, b"1234");
        assert_eq!(chunks[1].data, b"5678");
    }

    #[test]
    fn last_chunk_is_smaller_if_not_evenly_divisible() {
        let chunker = FixedChunker::new(4);
        let data = b"123456789"; // 9 bytes → [4, 4, 1]
        let chunks = chunker.chunk(data).unwrap();
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[2].data, b"9");
    }

    #[test]
    fn empty_input_returns_empty_vec() {
        let chunker = FixedChunker::default();
        let chunks = chunker.chunk(b"").unwrap();
        assert!(chunks.is_empty(), "empty input must return empty Vec");
    }

    #[test]
    fn chunk_offsets_are_monotonically_increasing_and_contiguous() {
        let chunker = FixedChunker::new(4);
        let data: Vec<u8> = (0..17).collect(); // 17 bytes → [4, 4, 4, 4, 1]
        let chunks = chunker.chunk(&data).unwrap();

        let mut expected_offset = 0usize;
        for c in &chunks {
            assert_eq!(
                c.offset, expected_offset,
                "chunk offset must be contiguous"
            );
            expected_offset += c.data.len();
        }
        assert_eq!(expected_offset, data.len(), "offsets must cover all input bytes");
    }

    #[test]
    fn concatenating_chunks_reproduces_original_input() {
        let chunker = FixedChunker::default();
        let data: Vec<u8> = (0..=255u8).cycle().take(10_000).collect();
        let chunks = chunker.chunk(&data).unwrap();
        let reconstructed = concat_chunks(&chunks);
        assert_eq!(reconstructed, data, "chunk round-trip must reproduce original input");
    }

    #[test]
    fn strategy_id_is_fixed_4096() {
        let chunker = FixedChunker::default();
        assert_eq!(chunker.strategy_id(), "fixed-4096");
    }

    /// Swap test: proves `&dyn Chunker` dispatch works — both FixedChunker
    /// and LargeBlockChunker pass through the same helper without changes to
    /// that helper (CAS-02 pluggability).
    #[test]
    fn swap_test_trait_object_dispatch() {
        let fixed: FixedChunker = FixedChunker::default();
        let large: LargeBlockChunker = LargeBlockChunker::new();

        let data: Vec<u8> = vec![0u8; 10_000];

        // Both go through `chunk_with(&dyn Chunker, ...)` — the concrete type is invisible
        let chunks_fixed = chunk_with(&fixed, &data);
        let chunks_large = chunk_with(&large, &data);

        // 4096-byte blocks: ceil(10000 / 4096) = 3
        assert_eq!(chunks_fixed.len(), 3);
        // 8192-byte blocks: ceil(10000 / 8192) = 2
        assert_eq!(chunks_large.len(), 2);

        // Both round-trip correctly
        assert_eq!(concat_chunks(&chunks_fixed), data);
        assert_eq!(concat_chunks(&chunks_large), data);

        assert_eq!(fixed.strategy_id(), "fixed-4096");
        assert_eq!(large.strategy_id(), "fixed-8192");
    }

    // -----------------------------------------------------------------------
    // Property-based test
    // -----------------------------------------------------------------------

    proptest! {
        /// For any random input, concatenating FixedChunker output reproduces the input.
        #[test]
        fn prop_chunk_round_trip(data in proptest::collection::vec(any::<u8>(), 0..=8192)) {
            let chunker = FixedChunker::default();
            let chunks = chunker.chunk(&data).unwrap();
            let reconstructed = concat_chunks(&chunks);
            prop_assert_eq!(reconstructed, data);
        }

        /// Chunk offsets are always contiguous for any random input.
        #[test]
        fn prop_offsets_contiguous(data in proptest::collection::vec(any::<u8>(), 1..=8192)) {
            let chunker = FixedChunker::default();
            let chunks = chunker.chunk(&data).unwrap();
            let mut expected = 0usize;
            for c in &chunks {
                prop_assert_eq!(c.offset, expected);
                expected += c.data.len();
            }
            prop_assert_eq!(expected, data.len());
        }
    }
}
