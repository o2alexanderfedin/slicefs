//! Digest types of the SliceFS metadata.

/// Size in bytes of a [`Digest256`]: the full BLAKE3 output.
pub const DIGEST_BYTES: usize = 32;

/// Content-addressed 256-bit key: an `ObjectId` of the chunk store (full
/// BLAKE3 output, 32 bytes) as 8 little-endian `u32` words.
pub type Digest256 = [u32; DIGEST_BYTES / 4];
