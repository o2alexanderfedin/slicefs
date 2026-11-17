//! Digest types of the SliceFS metadata.

/// Content-addressed 224-bit key: an `ObjectId` of the chunk store (BLAKE3,
/// 28 bytes) as 7 little-endian `u32` words.
pub type Digest224 = [u32; 7];
