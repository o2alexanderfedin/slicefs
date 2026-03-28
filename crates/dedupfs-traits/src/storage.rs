//! Storage traits for CAS-backed Dictionary storage.
//!
//! These traits are defined inline here (mirroring blockset's private storage module)
//! to allow crates in this workspace to implement them independently of blockset.
//!
//! The types here are structurally compatible with blockset's `StorageAdd` and
//! `StorageGet` — any type implementing these traits is automatically a valid
//! blockset Dictionary backend.

use crate::digest::{Digest224, Digest256, Branches};

/// Dictionary write interface: add tree nodes and finalize to a Digest224 key.
///
/// Mirrors `blockset::storage::StorageAdd`. Any type implementing this trait
/// can be used wherever blockset expects a `StorageAdd` implementation.
pub trait StorageAdd {
    /// Combine two Digest256 child nodes, storing the result.
    fn add(&mut self, left: &Digest256, right: &Digest256) -> Digest256;
    /// Finalize a Digest256 tree root to a Digest224 addressable key.
    fn end(&mut self, x: &Digest256) -> Digest224;
}

/// Dictionary read interface: look up two children by their Digest224 key.
///
/// Mirrors `blockset::storage::StorageGet`. Any type implementing this trait
/// can be used wherever blockset expects a `StorageGet` implementation.
pub trait StorageGet {
    /// Return the two child Digest256 nodes stored at `key`, if present.
    fn get(&self, key: &Digest224) -> Option<Branches>;
}
