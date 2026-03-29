//! Pluggable compression implementations for SliceFS.
//!
//! # Implementations
//!
//! - [`ZstdCompressor`] — Zstandard with configurable level (default 3)
//! - [`Lz4Compressor`]  — LZ4 block compression
//! - [`NoneCompressor`] — Passthrough (no compression)
//!
//! # Wire Format Helpers
//!
//! [`compress_block`] and [`decompress_block`] wrap a `Compressor` with a
//! simple 1-byte header so blocks are self-describing:
//!
//! ```text
//! ┌──────────────┬─────────────────────────────────┐
//! │ AlgorithmId  │  payload (compressed or raw)    │
//! │   (1 byte)   │  (variable length)              │
//! └──────────────┴─────────────────────────────────┘
//! ```

pub mod lz4_compressor;
pub mod none_compressor;
pub mod zstd_compressor;

pub use lz4_compressor::Lz4Compressor;
pub use none_compressor::NoneCompressor;
pub use zstd_compressor::ZstdCompressor;

use slicefs_traits::{AlgorithmId, Compressor, CompressorError};

/// Compress `raw` bytes with `compressor` and prepend a 1-byte [`AlgorithmId`]
/// header.
///
/// If the compression algorithm detects incompressible data it will return
/// `AlgorithmId::Raw` and the original bytes are stored verbatim (plus the
/// header byte).  If compression itself fails the function falls back silently
/// to `AlgorithmId::Raw`.
pub fn compress_block(compressor: &dyn Compressor, raw: &[u8]) -> Vec<u8> {
    let (algo, payload) = compressor
        .compress(raw)
        .unwrap_or_else(|_| (AlgorithmId::Raw, raw.to_vec()));
    let mut out = Vec::with_capacity(1 + payload.len());
    out.push(algo as u8);
    out.extend_from_slice(&payload);
    out
}

/// Decompress a wire-format block produced by [`compress_block`].
///
/// The first byte must be a valid [`AlgorithmId`]; the remaining bytes are
/// the payload.  An empty `wire` slice returns an empty `Vec`.
pub fn decompress_block(
    compressor: &dyn Compressor,
    wire: &[u8],
) -> Result<Vec<u8>, CompressorError> {
    if wire.is_empty() {
        return Ok(vec![]);
    }
    let algo = AlgorithmId::from_u8(wire[0]).ok_or_else(|| {
        CompressorError::Decompress(format!("unknown algorithm id: 0x{:02x}", wire[0]))
    })?;
    compressor.decompress(algo, &wire[1..])
}

/// Factory: parse a compressor name and optional level into a `Box<dyn Compressor>`.
///
/// Recognised names: `"zstd"`, `"lz4"`, `"none"` (case-sensitive).
///
/// `level` is only honoured for Zstd; it is silently ignored for LZ4 and None.
/// Defaults: Zstd level 3.
///
/// # Panics
///
/// Panics for unrecognised names — this is intentionally strict so CLI argument
/// validation catches typos before doing any I/O.
pub fn parse_compressor(name: &str, level: Option<i32>) -> Box<dyn Compressor> {
    match name {
        "zstd" => Box::new(ZstdCompressor::new(level.unwrap_or(3))),
        "lz4" => Box::new(Lz4Compressor::new()),
        "none" => Box::new(NoneCompressor::new()),
        other => panic!("unknown compressor: {other:?}. Valid values: zstd, lz4, none"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- helpers -------------------------------------------------------

    fn compressible() -> Vec<u8> {
        b"a".repeat(1000)
    }

    fn random_bytes() -> Vec<u8> {
        // A simple pseudo-random sequence that is effectively incompressible.
        let mut v = Vec::with_capacity(1000);
        let mut x: u64 = 0xdeadbeef_cafebabe;
        for _ in 0..1000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            v.push(x as u8);
        }
        v
    }

    // ---- ZstdCompressor tests -----------------------------------------

    #[test]
    fn zstd_compress_compressible() {
        let c = ZstdCompressor::new(3);
        let (algo, out) = c.compress(&compressible()).unwrap();
        assert_eq!(algo, AlgorithmId::Zstd);
        assert!(out.len() < 1000, "compressed should be smaller: {}", out.len());
    }

    #[test]
    fn zstd_incompressible_returns_raw() {
        let c = ZstdCompressor::new(3);
        let data = random_bytes();
        let (algo, out) = c.compress(&data).unwrap();
        assert_eq!(algo, AlgorithmId::Raw);
        assert_eq!(out, data);
    }

    #[test]
    fn zstd_round_trip_compressible() {
        let c = ZstdCompressor::new(3);
        let original = compressible();
        let (algo, compressed) = c.compress(&original).unwrap();
        let recovered = c.decompress(algo, &compressed).unwrap();
        assert_eq!(recovered, original);
    }

    #[test]
    fn zstd_round_trip_incompressible() {
        let c = ZstdCompressor::new(3);
        let original = random_bytes();
        let (algo, payload) = c.compress(&original).unwrap();
        let recovered = c.decompress(algo, &payload).unwrap();
        assert_eq!(recovered, original);
    }

    #[test]
    fn zstd_round_trip_empty() {
        let c = ZstdCompressor::new(3);
        let (algo, payload) = c.compress(&[]).unwrap();
        let recovered = c.decompress(algo, &payload).unwrap();
        assert_eq!(recovered, Vec::<u8>::new());
    }

    #[test]
    fn zstd_default_level_is_3() {
        let c = ZstdCompressor::default();
        let (algo, out) = c.compress(&compressible()).unwrap();
        assert_eq!(algo, AlgorithmId::Zstd);
        let c2 = ZstdCompressor::new(3);
        let (_, out2) = c2.compress(&compressible()).unwrap();
        assert_eq!(out, out2);
    }

    #[test]
    fn zstd_algorithm_id() {
        assert_eq!(ZstdCompressor::default().algorithm_id(), AlgorithmId::Zstd);
    }

    // ---- Lz4Compressor tests ------------------------------------------

    #[test]
    fn lz4_compress_compressible() {
        let c = Lz4Compressor::new();
        let (algo, out) = c.compress(&compressible()).unwrap();
        assert_eq!(algo, AlgorithmId::Lz4);
        assert!(out.len() < 1000, "compressed should be smaller: {}", out.len());
    }

    #[test]
    fn lz4_incompressible_returns_raw() {
        let c = Lz4Compressor::new();
        let data = random_bytes();
        let (algo, out) = c.compress(&data).unwrap();
        assert_eq!(algo, AlgorithmId::Raw);
        assert_eq!(out, data);
    }

    #[test]
    fn lz4_round_trip_compressible() {
        let c = Lz4Compressor::new();
        let original = compressible();
        let (algo, compressed) = c.compress(&original).unwrap();
        let recovered = c.decompress(algo, &compressed).unwrap();
        assert_eq!(recovered, original);
    }

    #[test]
    fn lz4_round_trip_incompressible() {
        let c = Lz4Compressor::new();
        let original = random_bytes();
        let (algo, payload) = c.compress(&original).unwrap();
        let recovered = c.decompress(algo, &payload).unwrap();
        assert_eq!(recovered, original);
    }

    #[test]
    fn lz4_round_trip_empty() {
        let c = Lz4Compressor::new();
        let (algo, payload) = c.compress(&[]).unwrap();
        let recovered = c.decompress(algo, &payload).unwrap();
        assert_eq!(recovered, Vec::<u8>::new());
    }

    #[test]
    fn lz4_algorithm_id() {
        assert_eq!(Lz4Compressor::new().algorithm_id(), AlgorithmId::Lz4);
    }

    // ---- NoneCompressor tests -----------------------------------------

    #[test]
    fn none_always_returns_none_id() {
        let c = NoneCompressor::new();
        let data = b"hello world";
        let (algo, out) = c.compress(data).unwrap();
        assert_eq!(algo, AlgorithmId::None);
        assert_eq!(out, data);
    }

    #[test]
    fn none_decompress_none_passthrough() {
        let c = NoneCompressor::new();
        let data = b"hello";
        let out = c.decompress(AlgorithmId::None, data).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn none_decompress_raw_passthrough() {
        let c = NoneCompressor::new();
        let data = b"raw data";
        let out = c.decompress(AlgorithmId::Raw, data).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn none_decompress_zstd_errors() {
        let c = NoneCompressor::new();
        assert!(c.decompress(AlgorithmId::Zstd, b"anything").is_err());
    }

    #[test]
    fn none_decompress_lz4_errors() {
        let c = NoneCompressor::new();
        assert!(c.decompress(AlgorithmId::Lz4, b"anything").is_err());
    }

    #[test]
    fn none_algorithm_id() {
        assert_eq!(NoneCompressor::new().algorithm_id(), AlgorithmId::None);
    }

    // ---- compress_block / decompress_block ---------------------------

    #[test]
    fn wire_format_has_1_byte_header() {
        let c = ZstdCompressor::new(3);
        let wire = compress_block(&c, &compressible());
        // First byte must be a valid AlgorithmId
        assert!(AlgorithmId::from_u8(wire[0]).is_some());
    }

    #[test]
    fn wire_format_round_trip_zstd() {
        let c = ZstdCompressor::new(3);
        let original = compressible();
        let wire = compress_block(&c, &original);
        let recovered = decompress_block(&c, &wire).unwrap();
        assert_eq!(recovered, original);
    }

    #[test]
    fn wire_format_round_trip_lz4() {
        let c = Lz4Compressor::new();
        let original = compressible();
        let wire = compress_block(&c, &original);
        let recovered = decompress_block(&c, &wire).unwrap();
        assert_eq!(recovered, original);
    }

    #[test]
    fn wire_format_round_trip_none() {
        let c = NoneCompressor::new();
        let original = b"test data".to_vec();
        let wire = compress_block(&c, &original);
        let recovered = decompress_block(&c, &wire).unwrap();
        assert_eq!(recovered, original);
    }

    #[test]
    fn wire_format_empty_round_trip_zstd() {
        let c = ZstdCompressor::new(3);
        let wire = compress_block(&c, &[]);
        let recovered = decompress_block(&c, &wire).unwrap();
        assert_eq!(recovered, Vec::<u8>::new());
    }

    #[test]
    fn wire_format_empty_round_trip_lz4() {
        let c = Lz4Compressor::new();
        let wire = compress_block(&c, &[]);
        let recovered = decompress_block(&c, &wire).unwrap();
        assert_eq!(recovered, Vec::<u8>::new());
    }

    #[test]
    fn wire_format_empty_round_trip_none() {
        let c = NoneCompressor::new();
        let wire = compress_block(&c, &[]);
        let recovered = decompress_block(&c, &wire).unwrap();
        assert_eq!(recovered, Vec::<u8>::new());
    }

    #[test]
    fn decompress_block_empty_wire_returns_empty() {
        let c = ZstdCompressor::new(3);
        let out = decompress_block(&c, &[]).unwrap();
        assert_eq!(out, Vec::<u8>::new());
    }

    // ---- parse_compressor factory ------------------------------------

    #[test]
    fn parse_compressor_zstd() {
        let c = parse_compressor("zstd", None);
        assert_eq!(c.algorithm_id(), AlgorithmId::Zstd);
    }

    #[test]
    fn parse_compressor_zstd_with_level() {
        let c = parse_compressor("zstd", Some(9));
        assert_eq!(c.algorithm_id(), AlgorithmId::Zstd);
    }

    #[test]
    fn parse_compressor_lz4() {
        let c = parse_compressor("lz4", None);
        assert_eq!(c.algorithm_id(), AlgorithmId::Lz4);
    }

    #[test]
    fn parse_compressor_none() {
        let c = parse_compressor("none", None);
        assert_eq!(c.algorithm_id(), AlgorithmId::None);
    }

    #[test]
    #[should_panic(expected = "unknown compressor")]
    fn parse_compressor_unknown_panics() {
        parse_compressor("snappy", None);
    }
}
