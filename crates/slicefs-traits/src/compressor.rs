use thiserror::Error;

/// Identifies the compression algorithm used to encode a block.
///
/// Stored as the first byte in the on-disk wire format produced by
/// `compress_block` / `decompress_block` in `slicefs-compression`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AlgorithmId {
    /// No compression — data stored verbatim.
    None = 0x00,
    /// Zstandard compression.
    Zstd = 0x01,
    /// LZ4 block compression (with size prepended).
    Lz4 = 0x02,
    /// Data was incompressible; stored verbatim even though the compressor
    /// normally compresses.  Differs from `None` in that the caller *tried*
    /// to compress.
    Raw = 0x03,
}

impl AlgorithmId {
    /// Try to convert a raw byte to an `AlgorithmId`.
    ///
    /// Returns `None` for unknown discriminants.
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0x00 => Some(Self::None),
            0x01 => Some(Self::Zstd),
            0x02 => Some(Self::Lz4),
            0x03 => Some(Self::Raw),
            _ => None,
        }
    }
}

/// Errors produced by [`Compressor`] implementations.
#[derive(Error, Debug)]
pub enum CompressorError {
    /// The compression operation failed.
    #[error("compress error: {0}")]
    Compress(String),
    /// The decompression operation failed.
    #[error("decompress error: {0}")]
    Decompress(String),
}

/// Pluggable compression interface.
///
/// Implementations must be `Send + Sync` so they can be shared across threads
/// behind an `Arc<dyn Compressor>`.
///
/// # Incompressible data
///
/// When `compress` detects that the compressed form is not smaller than the
/// input, it should return `(AlgorithmId::Raw, input.to_vec())` rather than
/// inflating the block.  `decompress` for `AlgorithmId::Raw` must return the
/// data unchanged.
pub trait Compressor: Send + Sync {
    /// Compress `input`, returning `(algorithm, bytes)`.
    ///
    /// `algorithm` identifies how `bytes` was encoded so that `decompress`
    /// can invert it.  May return `AlgorithmId::Raw` when the data is
    /// incompressible.
    fn compress(&self, input: &[u8]) -> Result<(AlgorithmId, Vec<u8>), CompressorError>;

    /// Decompress `input` that was encoded with `algorithm`.
    fn decompress(
        &self,
        algorithm: AlgorithmId,
        input: &[u8],
    ) -> Result<Vec<u8>, CompressorError>;

    /// The primary algorithm this compressor uses (excluding Raw passthrough).
    fn algorithm_id(&self) -> AlgorithmId;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn algorithm_id_round_trip() {
        let ids = [
            AlgorithmId::None,
            AlgorithmId::Zstd,
            AlgorithmId::Lz4,
            AlgorithmId::Raw,
        ];
        for id in ids {
            assert_eq!(AlgorithmId::from_u8(id as u8), Some(id));
        }
    }

    #[test]
    fn algorithm_id_invalid_returns_none() {
        assert_eq!(AlgorithmId::from_u8(0x04), None);
        assert_eq!(AlgorithmId::from_u8(0xFF), None);
    }

    #[test]
    fn compressor_error_display() {
        let e = CompressorError::Compress("bad input".to_string());
        assert!(e.to_string().contains("bad input"));
        let e2 = CompressorError::Decompress("truncated".to_string());
        assert!(e2.to_string().contains("truncated"));
    }

    /// Verify the trait is object-safe (can be used as `dyn Compressor`).
    #[test]
    fn compressor_is_object_safe() {
        fn _accept(_: &dyn Compressor) {}
        // No concrete type needed — just confirm it compiles.
    }
}
