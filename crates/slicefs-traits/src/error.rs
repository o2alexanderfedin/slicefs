use crate::hash::ChunkHash;
use thiserror::Error;

/// Typed error enum covering all CAS failure modes.
#[derive(Error, Debug)]
pub enum CasError {
    /// A requested block was not found in the store.
    #[error("block not found: {0}")]
    NotFound(ChunkHash),

    /// A block's content hash did not match on read — data corruption detected.
    #[error("integrity failure: expected {expected}, got {actual}")]
    IntegrityFailure {
        expected: ChunkHash,
        actual: ChunkHash,
    },

    /// Two different blocks produced the same hash — hash collision detected.
    /// This is a catastrophic error; the block store cannot safely store both.
    #[error("hash collision detected for hash {hash}")]
    HashCollision { hash: ChunkHash },

    /// An underlying I/O error occurred (file read, write, stat, etc.).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// A chunker-specific error (e.g., invalid input, configuration error).
    #[error("chunker error: {0}")]
    Chunker(String),

    /// A dedup index error (e.g., serialization failure, database error).
    #[error("index error: {0}")]
    Index(String),
}
