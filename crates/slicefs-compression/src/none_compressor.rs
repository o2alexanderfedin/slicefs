use slicefs_traits::{AlgorithmId, Compressor, CompressorError};

/// A passthrough compressor that stores data verbatim.
///
/// Use this when compression is disabled (`--compressor none`).
/// `compress` always returns `AlgorithmId::None`; `decompress` accepts
/// `None` and `Raw` without any processing.
#[derive(Debug, Default, Clone)]
pub struct NoneCompressor;

impl NoneCompressor {
    /// Create a new `NoneCompressor`.
    pub fn new() -> Self {
        Self
    }
}

impl Compressor for NoneCompressor {
    fn compress(&self, input: &[u8]) -> Result<(AlgorithmId, Vec<u8>), CompressorError> {
        Ok((AlgorithmId::None, input.to_vec()))
    }

    fn decompress(&self, algorithm: AlgorithmId, input: &[u8]) -> Result<Vec<u8>, CompressorError> {
        match algorithm {
            AlgorithmId::None | AlgorithmId::Raw => Ok(input.to_vec()),
            other => Err(CompressorError::Decompress(format!(
                "NoneCompressor cannot decompress {:?} data",
                other
            ))),
        }
    }

    fn algorithm_id(&self) -> AlgorithmId {
        AlgorithmId::None
    }
}
