//! A prototype prolly-tree map (a sorted map of byte keys to byte values)
//! whose nodes are chunk-store objects, for issue #19.
//!
//! # Status
//!
//! A prototype for the evaluation of #19. SliceFS does not use it: it is
//! not wired into the root record, the commit path or a mount, and no
//! product crate depends on it.
//!
//! # Storage
//!
//! Each node is one chunk-store object (`ChunkStore::put`, read back with
//! the hash-verified `ChunkStore::read`), so it is deduplicated, compressed
//! and packed like a file chunk. Nodes are not cut again by FastCDC. A
//! child pointer is the child's `ObjectId`, and the root of a map is one
//! `ObjectId`. The empty map is the empty leaf object (`empty_root`).
//! Nothing else is written: no root record, segment record or header line.
//!
//! # Node format
//!
//! The node bytes are private to this crate; this is not a store format.
//! Header (`NODE_HEADER` = 9 bytes): `NODE_MAGIC` (`SPT1`), level `u8`,
//! entry count `u32` LE. A leaf entry (level 0) is klen `u16` LE, vlen
//! `u16` LE, key, value. An internal entry (level 1 and up) is klen `u16`
//! LE, the last key of the child, the child `ObjectId` (33 bytes) and the
//! leaf entry count of the child's subtree `u64` LE. Keys are strictly
//! ascending in every node. A damaged or crafted node gives
//! `Error::Corrupt`, never a panic.
//!
//! # Boundary rule
//!
//! This is Dolt's size-aware key splitter (`go/store/prolly/tree/node_splitter.go`
//! and `chunker.go` of `dolthub/dolt`, read at commit d5a833f8df56e35c8fffccb48cfa5d790726aa57).
//! One `Splitter` decides every cut at every level. After an entry that
//! grows the node from `before` to `after` bytes:
//!
//! - an internal node never ends after its first entry;
//! - the node ends when `after > NODE_MAX - MAX_ENTRY` (the forced cut);
//! - else it does not end when `after < NODE_MIN` (512 B);
//! - else it ends when `boundary_hash(level, key) / (2^32 - 1)` is below
//!   `1 - e^-((after^4 - before^4) / L^4)` with `L = WEIBULL_L` (4,096) and
//!   the shape `WEIBULL_K` = 4. That is the conditional Weibull CDF of
//!   Dolt; the mean node is about 0.906 x L, about 3.7 KB.
//!
//! `boundary_hash` is the first 4 bytes (LE) of BLAKE3 over the byte
//! `level + 1` and the key: over the key only, with a salt per level.
//! Two changes to Dolt: (1) the forced cut at `NODE_MAX - MAX_ENTRY` with
//! the limits `MAX_ENTRY` and `MAX_KEY`, so no node is above `NODE_MAX`
//! (Dolt cuts after the entry that passes its maximum); (2) the size
//! counted is the encoded node size, header and length fields included
//! (Dolt counts key and value bytes only), so `NODE_MAX` bounds the object.
//! The decision reads only the entries since the last cut and the current
//! key, with no look-ahead.
//!
//! # What the boundaries depend on
//!
//! With fixed-width values, the node boundaries are a function of the key
//! set alone. With variable-width values, they depend on the key set and
//! the entry byte lengths, never on the value bytes. An internal entry has
//! a fixed-width child ID and count, so its level cuts only on the last
//! keys of the children and their lengths.
//!
//! # Memory
//!
//! Build keeps one node in progress per level, and the cursor, `get` and
//! `walk` hold one path of nodes: O(depth x node_max) bytes, never
//! O(entries). There is no node cache.
//!
//! # Concurrency
//!
//! The design is a single writer per map. Nodes are immutable
//! content-addressed objects, so readers of an existing root are safe at
//! any time, and no type of this crate holds shared mutable state.
//!
//! # Floats
//!
//! The cut test uses only the four basic float operations (an exact u64
//! difference and an own `e^-x`), so node IDs are the same on every
//! platform; `golden_root` pins them. Whether this rule and its constants
//! would become part of a frozen store format is owner question Q-3 of
//! Phase 24 (a one-way door, not decided here).

mod node;
mod rule;
mod tree;

#[cfg(test)]
mod tests;

pub use rule::{boundary_hash, cut_probability, Splitter};
pub use tree::{build, empty_root, get, iter, walk, BuildStats, Builder, Cursor, NodeInfo};

/// Bytes of the node header: magic 4, level 1, entry count 4.
pub const NODE_HEADER: usize = 9;
/// No cut below this node size (Dolt `minChunkSize`).
pub const NODE_MIN: usize = 512;
/// Hard bound of a node, header included (Dolt `maxChunkSize`).
pub const NODE_MAX: usize = 16_384;
/// Largest encoded entry, so that `NODE_MAX` is a real bound.
pub const MAX_ENTRY: usize = 2_048;
/// Largest key, so that an internal entry is never above `MAX_ENTRY`.
pub const MAX_KEY: usize = 1_024;
/// Scale of the size CDF; the mean node is about 0.906 x L (Dolt `L`).
pub const WEIBULL_L: u64 = 4_096;
/// Shape of the size CDF; the code writes x*x*x*x, never a power function.
pub const WEIBULL_K: u32 = 4;
/// Magic of a prolly node (`SPT1`), so it is never read as a manifest node (`SFM1`).
pub const NODE_MAGIC: [u8; 4] = *b"SPT1";

/// The size limits and the CDF scale of a tree. Only the two constants
/// exist; no other config can be made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    node_min: usize,
    node_max: usize,
    max_entry: usize,
    max_key: usize,
    weibull_l: usize,
}

impl Config {
    /// The rule of the crate constants.
    pub const DEFAULT: Config = Config {
        node_min: NODE_MIN,
        node_max: NODE_MAX,
        max_entry: MAX_ENTRY,
        max_key: MAX_KEY,
        weibull_l: WEIBULL_L as usize,
    };

    /// Small nodes (min 32, max 1,024, entry 256, key 128, L 256): deep
    /// trees from a few hundred entries, for tests.
    pub const SMALL: Config = Config { node_min: 32, node_max: 1_024, max_entry: 256, max_key: 128, weibull_l: 256 };

    /// No cut below this node size.
    pub fn node_min(&self) -> usize {
        self.node_min
    }

    /// No node above this size.
    pub fn node_max(&self) -> usize {
        self.node_max
    }

    /// Largest encoded entry.
    pub fn max_entry(&self) -> usize {
        self.max_entry
    }

    /// Largest key.
    pub fn max_key(&self) -> usize {
        self.max_key
    }

    /// Scale of the size CDF.
    pub fn weibull_l(&self) -> usize {
        self.weibull_l
    }

    /// A node ends after any entry that leaves it above this size.
    pub fn force_at(&self) -> usize {
        self.node_max - self.max_entry
    }
}

/// Errors of the prolly map.
#[derive(Debug)]
pub enum Error {
    /// The chunk store failed (for example a missing node).
    Store(chunk_store::Error),
    /// A node is damaged or crafted.
    Corrupt(String),
    /// An encoded entry is above the config's `max_entry`.
    EntryTooLarge { len: usize, max: usize },
    /// A key is above the config's `max_key`.
    KeyTooLarge { len: usize, max: usize },
    /// Keys are not strictly ascending.
    Unsorted,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Store(e) => write!(f, "prolly: chunk store: {e}"),
            Error::Corrupt(m) => write!(f, "prolly: damaged data: {m}"),
            Error::EntryTooLarge { len, max } => write!(f, "prolly: entry of {len} bytes is above the limit {max}"),
            Error::KeyTooLarge { len, max } => write!(f, "prolly: key of {len} bytes is above the limit {max}"),
            Error::Unsorted => write!(f, "prolly: keys are not strictly ascending"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Store(e) => Some(e),
            _ => None,
        }
    }
}

impl From<chunk_store::Error> for Error {
    fn from(e: chunk_store::Error) -> Self {
        Error::Store(e)
    }
}

/// Result of the prolly map.
pub type Result<T> = std::result::Result<T, Error>;
