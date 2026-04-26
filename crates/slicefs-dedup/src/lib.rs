//! Persistent on-disk DedupIndex for SliceFS, backed by redb 4.x.
//!
//! See `.planning/research/dedup-index/ARCHITECTURE.md` (the binding
//! spec) and the per-spec docs under
//! `.planning/research/dedup-index/architecture/`.
//!
//! # Panic policy
//! No panic ever crosses the [`slicefs_traits::DedupIndex`] trait
//! boundary. Engine panics are caught and surfaced as
//! [`CasError::Index("engine-panic: …")`]. See ARCHITECTURE §5.3.

mod error;
pub use error::DedupIndexError;
