use crate::error::CasError;

/// A single chunk produced by a [`Chunker`].
#[derive(Debug, Clone)]
pub struct Chunk {
    /// Byte offset of this chunk within the original buffer.
    pub offset: usize,

    /// The chunk data.
    pub data: Vec<u8>,
}

/// Pluggable chunking/block-splitting interface (CAS-02).
///
/// Takes a complete buffer and returns an ordered list of chunks. The owner's
/// content-defined chunking (CDC) algorithm will implement this trait; Phase 1
/// provides a simple fixed-size stub for testing.
///
/// # Design notes
/// - Buffered I/O: operates on `&[u8]` rather than streaming I/O — matches fuser's
///   write callback model where the complete write payload is already in memory.
/// - Sync: owner's existing algorithms are synchronous; fuser uses a sync callback model.
/// - `&self` (not `&mut self`): allows sharing across threads via `Arc<dyn Chunker>`.
///
/// # Contract
/// Implementations MUST be deterministic: identical input always produces identical
/// chunks. Non-determinism would silently defeat deduplication.
pub trait Chunker: Send + Sync {
    /// Split `data` into an ordered list of chunks.
    ///
    /// Returns the chunks in ascending offset order. The combined data of all chunks
    /// must equal the input `data` slice.
    fn chunk(&self, data: &[u8]) -> Result<Vec<Chunk>, CasError>;

    /// Human-readable identifier for this chunking strategy (e.g., `"fixed-4096"`, `"cdc-fastcdc"`).
    ///
    /// Used in diagnostics and to tag stored block metadata with the chunking algorithm.
    fn strategy_id(&self) -> &'static str;
}
