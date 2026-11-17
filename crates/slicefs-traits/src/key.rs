//! Keys of the SliceFS metadata.

/// Bytes of the hash tag at the start of a key.
pub const TAG_BYTES: usize = 1;

/// Bytes of a digest (every hash gives this many).
pub const DIGEST_BYTES: usize = 32;

/// Size in bytes of an [`ObjectKey`]: the tag, then the digest.
pub const KEY_BYTES: usize = TAG_BYTES + DIGEST_BYTES;

/// Content-addressed key of a metadata node or of file content: the same
/// bytes as an `ObjectId` of the chunk store, so it names its hash (a tag
/// byte) and holds the full digest. A peer can verify the content that a key
/// refers to with the hash that the tag names.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct ObjectKey(pub [u8; KEY_BYTES]);

impl ObjectKey {
    /// All zero bytes: no object (tag 0 is no hash).
    pub const ZERO: ObjectKey = ObjectKey([0; KEY_BYTES]);

    /// The key with this tag and digest.
    pub const fn new(tag: u8, digest: [u8; DIGEST_BYTES]) -> Self {
        let mut b = [0u8; KEY_BYTES];
        b[0] = tag;
        let mut i = 0;
        while i < DIGEST_BYTES {
            b[TAG_BYTES + i] = digest[i];
            i += 1;
        }
        ObjectKey(b)
    }

    /// A key made from a number, with tag 0 (no hash): for tests and
    /// examples only; no object has it. `example(0)` is `ZERO`.
    pub fn example(n: u32) -> Self {
        let mut b = [0u8; KEY_BYTES];
        b[TAG_BYTES..TAG_BYTES + 4].copy_from_slice(&n.to_le_bytes());
        ObjectKey(b)
    }

    /// The key in `bytes[at..at + KEY_BYTES]`. Panics if the slice is short.
    pub fn read(bytes: &[u8], at: usize) -> Self {
        ObjectKey(bytes[at..at + KEY_BYTES].try_into().unwrap())
    }

    /// The tag of the hash.
    pub fn tag(&self) -> u8 {
        self.0[0]
    }

    /// The digest (without the tag).
    pub fn digest(&self) -> &[u8; DIGEST_BYTES] {
        self.0[TAG_BYTES..].try_into().unwrap()
    }

    /// Lowercase hex of all bytes (tag first).
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

impl Default for ObjectKey {
    fn default() -> Self {
        ObjectKey::ZERO
    }
}
