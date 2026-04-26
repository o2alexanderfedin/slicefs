//! CAS trait contracts for the SliceFS filesystem.
//!
//! This crate defines the core trait interfaces that every component in the
//! slicefs workspace depends on.
//!
//! # CAS Traits (Phase 1)
//!
//! - [`ContentHasher`] — Pluggable hash function (CAS-01)
//! - [`Chunker`] — Pluggable block-splitting algorithm (CAS-02)
//! - [`BlockStore`] — Pluggable CAS block storage backend (CAS-03, CAS-05)
//! - [`DedupIndex`] — Bounded-memory dedup index with bloom pre-filter (CAS-07)
//!
//! # Metadata Traits (Phase 2)
//!
//! - [`MetadataStore`] — Full inode CRUD, directory, manifest, xattr operations
//! - [`StorageAdd`] / [`StorageGet`] — data-id Dictionary storage interface
//! - [`Digest224`] / [`Digest256`] — Content-addressed identity types from blockset
//!
//! # Supporting Types
//!
//! - [`ChunkHash`] — Opaque content hash newtype (variable-width, hex-displayable)
//! - [`Chunk`] — A single chunk produced by a [`Chunker`]
//! - [`BlockStoreConfig`] — Configuration for a [`BlockStore`] (integrity verification)
//! - [`DedupResult`] — Result of a [`DedupIndex`] lookup
//! - [`IndexStats`] — Trait-level coarse stats from [`DedupIndex::stats`]
//! - [`VerifyReport`] — Result of a [`DedupIndex::verify`] integrity scan
//! - [`CasError`] — Typed error enum covering all CAS failure modes
//! - [`MetaError`] — Typed error enum for metadata operations
//! - [`InodeMeta`] — POSIX inode fields (plain data; serialization in `metadata` crate)
//! - [`InodeId`] — Inode number type alias (`u64`)
//! - [`DirEntry`] — Directory entry (name + inode number)

pub mod block_store;
pub mod chunk;
pub mod compressor;
pub mod dedup_index;
pub mod digest;
pub mod error;
pub mod hash;
pub mod metadata;
pub mod storage;

// Re-export all public types for ergonomic imports:
// `use slicefs_traits::{ContentHasher, ChunkHash, CasError, ...}`

pub use block_store::{BlockStore, BlockStoreConfig};
pub use chunk::{Chunk, Chunker};
pub use compressor::{AlgorithmId, Compressor, CompressorError};
pub use dedup_index::{DedupIndex, DedupResult, IndexStats, VerifyReport};
pub use digest::*;
pub use error::CasError;
pub use hash::{ChunkHash, ContentHasher};
pub use metadata::*;
pub use storage::*;
