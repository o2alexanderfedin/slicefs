//! A depth-first walk over the nodes of a tree with their IDs: the GC mark
//! (which nodes are live) and a rehash read the tree through this.

use crate::cache::Ctx;
use crate::node::{self, Node};
use crate::{Config, Result};
use chunk_store::ObjectId;

/// The readers check nodes against the largest limits.
const READ_CFG: Config = Config::READ;

/// One node seen by `visit`.
#[derive(Clone, Copy, Debug)]
pub struct NodeVisit<'a> {
    /// The ID of the node.
    pub id: ObjectId,
    /// Level of the node (0 is a leaf).
    pub level: u8,
    /// Child entries of the node (0 for a leaf).
    pub children: usize,
    /// The entries of a leaf; empty for an internal node.
    pub entries: &'a [(Vec<u8>, Vec<u8>)],
}

/// Calls `f` once for every node occurrence of the tree `root`, parent
/// before children, children left to right, holding one path of nodes.
/// Each child is checked against its parent entry when it is loaded. Stops
/// as soon as `f` returns false: the result is `Ok(false)`; `Ok(true)`
/// means every node was visited. A node that two parents share is visited
/// once per parent.
pub fn visit(ctx: &Ctx, root: &ObjectId, f: &mut dyn FnMut(&NodeVisit<'_>) -> bool) -> Result<bool> {
    let l = node::load(ctx, &READ_CFG, root, None)?;
    visit_node(ctx, root, &l.node, None, f)
}

fn visit_node(
    ctx: &Ctx,
    id: &ObjectId,
    n: &Node,
    lo: Option<&[u8]>,
    f: &mut dyn FnMut(&NodeVisit<'_>) -> bool,
) -> Result<bool> {
    let v = match n {
        Node::Leaf(entries) => NodeVisit { id: *id, level: 0, children: 0, entries },
        Node::Internal { level, children } => NodeVisit { id: *id, level: *level, children: children.len(), entries: &[] },
    };
    if !f(&v) {
        return Ok(false);
    }
    if let Node::Internal { level, children } = n {
        for (i, c) in children.iter().enumerate() {
            let child_lo = if i > 0 { Some(children[i - 1].last_key.as_slice()) } else { lo };
            let l = node::load_child(ctx, &READ_CFG, level - 1, c, child_lo)?;
            if !visit_node(ctx, &c.id, &l.node, child_lo, f)? {
                return Ok(false);
            }
        }
    }
    Ok(true)
}
