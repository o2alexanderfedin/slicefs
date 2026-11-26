//! A prototype prolly-tree map (a sorted map of byte keys to byte values)
//! whose nodes are chunk-store objects, for issue #19.
//!
//! Task 3 completes this doc.

mod node;
mod rule;
mod tree;

#[cfg(test)]
mod tests;

pub use rule::{boundary_hash, cut_probability, Splitter};
pub use tree::{build, empty_root, get, BuildStats, Builder};

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
