//! A key range, and the entry count and height of a tree, from the root.

use crate::cache::Ctx;
use crate::node::{self, Node};
use crate::tree::{seek_in, Cursor};
use crate::{Config, Error, Result};
use chunk_store::ObjectId;

/// The readers check nodes against the largest limits.
const READ_CFG: Config = Config::READ;

/// An ordered iterator over the entries with `start <= key < end`. Like
/// `Cursor` it holds one path of nodes (one per level), never the map, and
/// checks each child against its parent entry when it is loaded.
pub struct Range<'s>(Cursor<'s>);

impl Range<'_> {
    /// The most nodes held at one time so far.
    pub fn max_nodes_held(&self) -> usize {
        self.0.max_nodes_held()
    }
}

impl Iterator for Range<'_> {
    type Item = Result<(Vec<u8>, Vec<u8>)>;

    /// The next entry in key order; after an error, `None`.
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next()
    }
}

/// The entries of the tree `root` with `start <= key < end` (`end` of
/// `None`: to the last key), in key order. The descent to `start` holds
/// one path and checks every child it loads.
pub fn range<'s>(ctx: &Ctx<'s>, root: &ObjectId, start: &[u8], end: Option<&[u8]>) -> Result<Range<'s>> {
    seek_in(ctx, root, start, end).map(Range)
}

/// The number of entries of the tree `root`, from the root node only: the
/// entry count of a leaf root, else the checked sum of the counts of the
/// root's children. The counts of the children are checked when a path
/// loads them.
pub fn count(ctx: &Ctx, root: &ObjectId) -> Result<u64> {
    let l = node::load(ctx, &READ_CFG, root, None)?;
    node::node_count(&l.node)
}

/// The levels of the tree `root`: 1 for a leaf root. Reads the root only.
pub fn height(ctx: &Ctx, root: &ObjectId) -> Result<u8> {
    let l = node::load(ctx, &READ_CFG, root, None)?;
    let level = match &*l.node {
        Node::Leaf(_) => 0,
        Node::Internal { level, .. } => *level,
    };
    level.checked_add(1).ok_or_else(|| Error::Corrupt("prolly node: level above 254".into()))
}
