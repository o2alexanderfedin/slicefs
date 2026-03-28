//! Re-exports of Digest224, Digest256, Branches and related helper functions from blockset.
//!
//! These types form the content-addressable identity of all data in SliceFS.
//! Digest256 is the internal tree node type; Digest224 is the addressable key type
//! stored in the Dictionary (the top 224 bits of a SHA-224 hash digest).
//!
//! Note: blockset's modules are private; types are accessed via blockset's public re-exports.

// Digest types - re-exported from blockset's public API
// blockset re-exports from_digest224, to_digest224 via lib.rs pub use
pub use blockset::from_digest224;
pub use blockset::to_digest224;

// blockset re-exports from_bytes, to_data via lib.rs pub use
pub use blockset::from_bytes as digest256_from_bytes;
pub use blockset::to_data as digest256_to_data;

// The actual Digest types are type aliases - we must re-declare them here
// since they come from private modules in blockset.
// Digest224 = [u32; 7], Digest256 = sha2_compress::Hash<u32> = [u32; 8]
// We re-export them through a newtype-free approach by using the type directly.
// blockset exposes these through the Dictionary type as key/value:
// Dictionary = BTreeMap<Digest224, Branches> — so we can derive the types.

// Re-export the type aliases by referring to the public Dictionary type's key/value types.
// We use `blockset::Dictionary` which is `BTreeMap<[u32;7], [[u32;8];2]>`.
// This gives us the exact types without going through private modules.

/// Content-addressable 224-bit key (top 7 u32 words of a SHA-224 hash).
/// Alias for `[u32; 7]` — the key type in a data-id Dictionary.
pub type Digest224 = [u32; 7];

/// 256-bit digest type used internally by blockset for tree nodes.
/// Alias for `[u32; 8]` (sha2_compress::Hash<u32>).
pub type Digest256 = [u32; 8];

/// Two child digests stored together in a Dictionary entry.
pub type Branches = [Digest256; 2];
