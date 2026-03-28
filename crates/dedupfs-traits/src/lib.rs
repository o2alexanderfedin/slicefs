//! CAS trait contracts for the dedupfs filesystem.
//!
//! This crate defines the four core trait interfaces that every component in the
//! dedupfs workspace depends on. It intentionally has minimal dependencies (only
//! `thiserror`) so that external adapter crates (e.g., for the owner's algorithm
//! crates) can implement these traits without pulling in unrelated dependencies.
//!
//! # Traits
//!
//! - [`ContentHasher`] — Pluggable hash function (CAS-01)
//! - [`Chunker`] — Pluggable block-splitting algorithm (CAS-02)
//! - [`BlockStore`] — Pluggable CAS block storage backend (CAS-03, CAS-05)
//! - [`DedupIndex`] — Bounded-memory dedup index with bloom pre-filter (CAS-07)
//!
//! # Supporting Types
//!
//! - [`ChunkHash`] — Opaque content hash newtype (variable-width, hex-displayable)
//! - [`Chunk`] — A single chunk produced by a [`Chunker`]
//! - [`BlockStoreConfig`] — Configuration for a [`BlockStore`] (integrity verification)
//! - [`DedupResult`] — Result of a [`DedupIndex`] lookup
//! - [`CasError`] — Typed error enum covering all CAS failure modes

pub mod block_store;
pub mod chunk;
pub mod dedup_index;
pub mod error;
pub mod hash;

// Re-export all public types for ergonomic imports:
// `use dedupfs_traits::{ContentHasher, ChunkHash, CasError, ...}`

pub use block_store::{BlockStore, BlockStoreConfig};
pub use chunk::{Chunk, Chunker};
pub use dedup_index::{DedupIndex, DedupResult};
pub use error::CasError;
pub use hash::{ChunkHash, ContentHasher};
