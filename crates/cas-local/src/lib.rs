//! Stub/test implementations of the dedupfs CAS traits.
//!
//! These implementations exist to prove out the trait interfaces and support
//! unit and integration testing before the owner's production algorithm crates
//! are integrated (in a dedicated adapter phase after Phase 1).
//!
//! # Modules
//!
//! - [`blake3_hasher`] — `Blake3Hasher`: implements `ContentHasher` using BLAKE3
//! - [`fixed_chunker`] — `FixedChunker`: implements `Chunker` with fixed-size blocks
//! - [`mem_block_store`] — `MemBlockStore`: in-memory `HashMap`-backed `BlockStore`
//! - [`disk_block_store`] — `LocalDiskStore`: flat-file `BlockStore` with 2-byte directory sharding
//! - [`mem_dedup_index`] — `MemDedupIndex`: bloom filter + `HashMap` `DedupIndex`

pub mod blake3_hasher;
pub mod disk_block_store;
pub mod fixed_chunker;
pub mod mem_block_store;
pub mod mem_dedup_index;
