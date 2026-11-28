//! Metadata engine for SliceFS — inode table, directory tree, file manifests,
//! xattrs.
//!
//! This crate provides concrete implementations of the metadata traits defined
//! in `slicefs-traits`. The nodes are stored in the chunk store
//! (`store_io`), next to the file content.

pub mod inode;
pub mod inode_alloc;
pub mod directory;
pub mod manifest;
pub mod xattr;
pub mod store;
pub mod store_io;
pub mod segment;
pub mod wal;
pub mod mount_lock;
pub mod gc;
pub mod snapshot;
pub mod rehash;
mod pmap;
