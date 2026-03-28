use std::fmt;

/// Opaque content hash. Wraps raw bytes; size is implementation-defined.
///
/// Using a `Vec<u8>` newtype rather than a fixed-size array means this type
/// accommodates any hash output width — critical for owner algorithm adapters
/// that may use different hash sizes.
///
/// Leading zeros are preserved in the `Display` impl via `{:02x}` formatting.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChunkHash(Vec<u8>);

impl ChunkHash {
    /// Construct a `ChunkHash` from raw hash bytes.
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    /// Return the underlying raw bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Display for ChunkHash {
    /// Hex-encode the hash bytes with leading-zero preservation (`{:02x}` per byte).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in &self.0 {
            write!(f, "{:02x}", b)?;
        }
        Ok(())
    }
}

/// Pluggable hash function interface (CAS-01).
///
/// Implementations must be `Send + Sync` for use in fuser's thread-per-request model.
/// All methods take `&self` to allow safe use behind `RwLock`/`Arc`.
///
/// # Design notes
/// - Buffered I/O: caller passes the complete block as a `&[u8]` slice.
/// - No streaming: owner's existing algorithms and fuser both operate on complete buffers.
/// - The `algorithm_id` is stored alongside the block so integrity verification can select
///   the correct hasher on read without out-of-band configuration.
pub trait ContentHasher: Send + Sync {
    /// Hash a complete block (buffered I/O: caller owns the slice).
    fn hash(&self, data: &[u8]) -> ChunkHash;

    /// Human-readable algorithm identifier (e.g., `"blake3"`, `"sha256"`).
    ///
    /// This identifier is stored in block metadata so the correct hasher can be
    /// selected when verifying integrity on read (CAS-05).
    fn algorithm_id(&self) -> &'static str;
}
