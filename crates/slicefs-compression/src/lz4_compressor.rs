use slicefs_traits::{AlgorithmId, Compressor, CompressorError};

/// LZ4 block compressor.
///
/// Uses `lz4_flex::block::compress_prepend_size` which prepends the original
/// length as a 4-byte little-endian header (required by
/// `decompress_size_prepended`).
///
/// When the compressed form is not smaller than the input (incompressible data),
/// `compress` falls back to `AlgorithmId::Raw`.
#[derive(Debug, Default, Clone)]
pub struct Lz4Compressor;

impl Lz4Compressor {
    /// Create a new `Lz4Compressor`.
    pub fn new() -> Self {
        Self
    }
}

impl Compressor for Lz4Compressor {
    fn compress(&self, input: &[u8]) -> Result<(AlgorithmId, Vec<u8>), CompressorError> {
        if input.is_empty() {
            return Ok((AlgorithmId::Raw, vec![]));
        }
        let compressed = lz4_flex::block::compress_prepend_size(input);
        if compressed.len() >= input.len() {
            Ok((AlgorithmId::Raw, input.to_vec()))
        } else {
            Ok((AlgorithmId::Lz4, compressed))
        }
    }

    fn decompress(&self, algorithm: AlgorithmId, input: &[u8]) -> Result<Vec<u8>, CompressorError> {
        match algorithm {
            AlgorithmId::Lz4 => lz4_flex::block::decompress_size_prepended(input)
                .map_err(|e| CompressorError::Decompress(e.to_string())),
            AlgorithmId::Raw | AlgorithmId::None => Ok(input.to_vec()),
            other => Err(CompressorError::Decompress(format!(
                "Lz4Compressor cannot decompress {:?} data",
                other
            ))),
        }
    }

    fn algorithm_id(&self) -> AlgorithmId {
        AlgorithmId::Lz4
    }
}
