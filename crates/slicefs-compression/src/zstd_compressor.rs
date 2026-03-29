use slicefs_traits::{AlgorithmId, Compressor, CompressorError};
use std::io::Read as _;

/// Zstandard compressor with configurable compression level.
///
/// When the compressed form is not smaller than the input (incompressible data),
/// `compress` falls back to `AlgorithmId::Raw` so the block is not inflated.
#[derive(Debug, Clone)]
pub struct ZstdCompressor {
    /// Compression level in the range 1..=22.  The zstd default is 3.
    level: i32,
}

impl ZstdCompressor {
    /// Create a `ZstdCompressor` with the given compression level (1–22).
    pub fn new(level: i32) -> Self {
        Self { level }
    }
}

impl Default for ZstdCompressor {
    fn default() -> Self {
        Self::new(3)
    }
}

impl Compressor for ZstdCompressor {
    fn compress(&self, input: &[u8]) -> Result<(AlgorithmId, Vec<u8>), CompressorError> {
        if input.is_empty() {
            // zstd can encode empty slices; let it do so, but we treat
            // empty-in as empty-out with Raw so decompress is trivial.
            return Ok((AlgorithmId::Raw, vec![]));
        }
        let compressed = zstd::encode_all(input, self.level)
            .map_err(|e| CompressorError::Compress(e.to_string()))?;
        if compressed.len() >= input.len() {
            Ok((AlgorithmId::Raw, input.to_vec()))
        } else {
            Ok((AlgorithmId::Zstd, compressed))
        }
    }

    fn decompress(
        &self,
        algorithm: AlgorithmId,
        input: &[u8],
    ) -> Result<Vec<u8>, CompressorError> {
        match algorithm {
            AlgorithmId::Zstd => {
                let mut buf = Vec::new();
                zstd::Decoder::new(input)
                    .map_err(|e| CompressorError::Decompress(e.to_string()))?
                    .read_to_end(&mut buf)
                    .map_err(|e| CompressorError::Decompress(e.to_string()))?;
                Ok(buf)
            }
            AlgorithmId::Raw | AlgorithmId::None => Ok(input.to_vec()),
            other => Err(CompressorError::Decompress(format!(
                "ZstdCompressor cannot decompress {:?} data",
                other
            ))),
        }
    }

    fn algorithm_id(&self) -> AlgorithmId {
        AlgorithmId::Zstd
    }
}
