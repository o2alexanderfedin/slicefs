//! A prolly-tree map (a sorted map of byte keys to byte values) whose nodes
//! are chunk-store objects: the metadata map format of store format 8
//! (issue #215).
//!
//! # Status
//!
//! The node format, the boundary rule (the integer threshold, its table and
//! the rule version byte) and the constants are part of store format 8. They
//! are defined in `docs/design/store-format.md`, section "Metadata maps
//! (store format 8)". In this build the commit, read and GC paths of
//! SliceFS do not use this crate; issue #215 wires them in.
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
//! The node bytes are part of store format 8 (see "Status"). Header (`NODE_HEADER` = 10 bytes): `NODE_MAGIC` (`SPT1`), rule version
//! `u8` (`RULE_VERSION`), level `u8`, entry count `u32` LE. A node of
//! another rule version gives `Error::RuleVersion` in `get`, the cursor,
//! `walk` and `apply`. A leaf entry (level 0) is klen `u16` LE, vlen
//! `u16` LE, key, value. An internal entry (level 1 and up) is klen `u16`
//! LE, the last key of the child, the child `ObjectId` (33 bytes) and the
//! leaf entry count of the child's subtree `u64` LE. Keys are strictly
//! ascending in every node. A damaged or crafted node gives
//! `Error::Corrupt`, never a panic. A node is also checked against the
//! size limits of a config (node bytes, key and entry sizes): `apply`
//! against its own config, the readers against `Config::READ`.
//!
//! Each child is checked against its parent entry when `get`, the cursor,
//! `walk` or `apply` loads it: its last key is the entry's key, its first
//! key is above the last key of the previous sibling (or the bound that the
//! path carries down), and its leaf entry count is the entry's count. So
//! the keys of a loaded path are in order and its counts are true, and a
//! crafted tree whose nodes are each valid but do not fit together gives
//! `Error::Corrupt` once a bad node is on the path. The check is per path:
//! a `get` whose path does not hold the bad node does not see it.
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
//! - else it ends when `boundary_hash(level, key)` (a 32-bit number) is below
//!   `threshold(cfg, before, after)`, an integer that stands for
//!   `1 - e^-((after^4 - before^4) / L^4)` with `L = WEIBULL_L` (8,192) and
//!   the shape `WEIBULL_K` = 4. That is the conditional Weibull CDF of
//!   Dolt; the mean node is about 0.906 x L, about 7.4 KB.
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
//! # Salt
//!
//! An optional per-store salt (`Config::with_salt`) makes the boundary
//! hash keyed BLAKE3 over the same bytes (`boundary_hash_keyed`). The
//! default is no salt, so node IDs are equal across stores. A reader needs
//! no salt; a writer must use the salt of the store. The salt enters only
//! the hash, never the stored key bytes.
//!
//! # What the boundaries depend on
//!
//! With fixed-width values, the node boundaries are a function of the key
//! set alone. With variable-width values, they depend on the key set and
//! the entry byte lengths, never on the value bytes. An internal entry has
//! a fixed-width child ID and count, so its level cuts only on the last
//! keys of the children and their lengths.
//!
//! # Apply
//!
//! `apply(store, cfg, root, batch)` applies one batch (one commit): puts
//! and deletes in strictly ascending key order. It returns the new root
//! and `ApplyStats`. Each level is rewritten from the leaves up through
//! the same `LevelWriter` and `Splitter` as the bulk builder. A rewrite
//! starts at the first touched node with a fresh splitter (a node start is
//! always a cut) and stops at the first new cut that falls on an old node
//! end before a node with no edits (resync); every other node keeps its
//! ID. The replaced and the new nodes become the edits of the level above.
//! New top levels come from the bulk builder, and the root is collapsed
//! through internal nodes with one child, so the root of a set does not
//! depend on the order of the edits: it equals the `build` root.
//! A shrink of the height may write up to depth nodes that the collapse
//! then skips; they stay unreferenced in the store. The batch is checked
//! (order, key and entry sizes) before any read or write.
//!
//! # Memory
//!
//! Build keeps one node in progress per level, and the cursor, `get` and
//! `walk` hold one path of nodes: O(depth x node_max) bytes, never
//! O(entries). Apply holds one path of nodes per level pass, one old node,
//! one writer buffer and the sorted edits of the next level: O(changes x
//! depth) plus the batch. The optional `NodeCache` (a `Ctx` carries it) has a
//! fixed byte cap and holds decoded nodes by `ObjectId`; Phase 24 D-17
//! forbids a cache that grows with the map, not a fixed one. A node is
//! charged its encoded length, 64 bytes per entry and 128; the oldest node
//! by use goes first. `range` holds one path, `count` and `height` read the
//! root only, and `visit` holds one path.
//!
//! # Concurrency
//!
//! The design is a single writer per map: one `apply` per root at a time.
//! Nodes are immutable content-addressed objects and no node is changed in
//! place, so readers of an existing root (also an old root during an
//! `apply`) are safe at any time, and the only shared state of this crate is
//! the `NodeCache`, behind one lock (`Rank::Store`, held for map operations
//! only, never across a read, a decode or a callback). It holds immutable
//! content-addressed nodes, so a hit is always the node of its ID.
//!
//! # Integer rule
//!
//! The cut test is an integer test: the node ends iff
//! `boundary_hash as u64 < threshold(cfg, before, after)`, with
//! `threshold = 2^32 - e^-delta` in units of 2^-32 and
//! `delta = ((after^4 - before^4) << 32) / L^4` (u128). The `e^-x` part
//! reads a committed table (`rule_table.rs`: 23 integer and 1,025 fraction
//! entries, linear interpolation), made by `scripts/prolly-table/gen_table.py`
//! (`--check` compares the file with the generator). No floating point is
//! used to decide a cut, so the cut points and node IDs are the same on
//! every platform; `golden_root` pins the table hash, thresholds, cut
//! points and roots, and it passes on macOS and on Linux.

mod apply;
mod cache;
mod node;
mod range;
mod rule;
mod rule_table;
mod tree;
mod visit;

#[cfg(test)]
mod tests;

pub use apply::{apply, apply_in, ApplyStats, Edit};
pub use cache::{CacheStats, Ctx, NodeCache};
pub use range::{count, height, range, Range};
pub use rule::{boundary_hash, boundary_hash_keyed, threshold, Splitter};
pub use tree::{build, empty_root, get, get_in, iter, iter_in, walk, BuildStats, Builder, Cursor, NodeInfo};
pub use visit::{visit, NodeVisit};

/// The version of the boundary rule that wrote a node. Every node header
/// carries it; a reader refuses another version. Version 1 is the integer
/// threshold of `rule_table.rs` with the constants of `Config::DEFAULT`
/// (`WEIBULL_L` 8,192, `NODE_MAX` 16,384). No node of an older rule exists
/// in any store, so the number stays 1.
pub const RULE_VERSION: u8 = 1;
/// Bytes of the node header: magic 4, rule version 1, level 1, entry count 4.
pub const NODE_HEADER: usize = 10;
/// No cut below this node size (Dolt `minChunkSize`).
pub const NODE_MIN: usize = 512;
/// Hard bound of a node, header included (Dolt `maxChunkSize`).
pub const NODE_MAX: usize = 16_384;
/// Largest encoded entry, so that `NODE_MAX` is a real bound.
pub const MAX_ENTRY: usize = 2_048;
/// Largest key, so that an internal entry is never above `MAX_ENTRY`.
pub const MAX_KEY: usize = 1_024;
/// Scale of the size CDF; the mean node is about 0.906 x L (Dolt `L`).
pub const WEIBULL_L: u64 = 8_192;
/// Shape of the size CDF; the code writes x*x*x*x, never a power function.
pub const WEIBULL_K: u32 = 4;
/// Magic of a prolly node (`SPT1`), so it is never read as a manifest node (`SFM1`).
pub const NODE_MAGIC: [u8; 4] = *b"SPT1";

/// The largest node any config may write, so the largest node a reader
/// accepts (32 KiB).
pub const MAX_NODE_MAX: usize = 32_768;

/// The size limits, the CDF scale and the optional salt of a tree.
/// `DEFAULT` and `SMALL` are the configs of the store and of the tests;
/// `Config::new` makes the configs of the bench sweep.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    node_min: usize,
    node_max: usize,
    max_entry: usize,
    max_key: usize,
    weibull_l: usize,
    salt: Option<[u8; 32]>,
}

impl Config {
    /// The rule of the crate constants.
    pub const DEFAULT: Config = Config {
        node_min: NODE_MIN,
        node_max: NODE_MAX,
        max_entry: MAX_ENTRY,
        max_key: MAX_KEY,
        weibull_l: WEIBULL_L as usize,
        salt: None,
    };

    /// The limits that readers check every node against: the `DEFAULT`
    /// limits with `node_max` = `MAX_NODE_MAX`, so a reader accepts every
    /// config that `Config::new` makes.
    pub const READ: Config = Config { node_max: MAX_NODE_MAX, ..Config::DEFAULT };

    /// Small nodes (min 32, max 1,024, entry 256, key 128, L 256): deep
    /// trees from a few hundred entries, for tests.
    pub const SMALL: Config =
        Config { node_min: 32, node_max: 1_024, max_entry: 256, max_key: 128, weibull_l: 256, salt: None };

    /// A config of the bench sweep: scale `weibull_l` in 256..=16,384 and
    /// `node_max` in 4,096..=`MAX_NODE_MAX`; the other limits are the crate
    /// constants and there is no salt. `None` outside the ranges. A store
    /// tree uses `Config::DEFAULT`.
    pub fn new(weibull_l: usize, node_max: usize) -> Option<Config> {
        if !(256..=16_384).contains(&weibull_l) || !(4_096..=MAX_NODE_MAX).contains(&node_max) {
            return None;
        }
        Some(Config { node_max, weibull_l, ..Config::DEFAULT })
    }

    /// The same config with a salt (or none). The salt makes the boundary
    /// hash keyed BLAKE3; a reader needs no salt, a writer must use the
    /// salt of the store.
    pub fn with_salt(self, salt: Option<[u8; 32]>) -> Config {
        Config { salt, ..self }
    }

    /// The salt of the boundary hash, if any.
    pub fn salt(&self) -> Option<&[u8; 32]> {
        self.salt.as_ref()
    }

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
    /// A node was written under another boundary rule version.
    RuleVersion { found: u8 },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Store(e) => write!(f, "prolly: chunk store: {e}"),
            Error::Corrupt(m) => write!(f, "prolly: damaged data: {m}"),
            Error::EntryTooLarge { len, max } => write!(f, "prolly: entry of {len} bytes is above the limit {max}"),
            Error::KeyTooLarge { len, max } => write!(f, "prolly: key of {len} bytes is above the limit {max}"),
            Error::Unsorted => write!(f, "prolly: keys are not strictly ascending"),
            Error::RuleVersion { found } => {
                write!(f, "prolly: node of boundary rule version {found}; this build knows version {RULE_VERSION} only")
            }
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
