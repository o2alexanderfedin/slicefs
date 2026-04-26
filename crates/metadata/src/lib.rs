//! Metadata engine for SliceFS — inode table, directory tree, file manifests,
//! xattrs built on data-id's CAS Dictionary.
//!
//! This crate provides concrete implementations of the metadata traits defined
//! in `slicefs-traits`. The storage backend is a data-id `Dictionary`.

pub mod directory;
pub mod gc;
pub mod inode;
pub mod inode_map;
pub mod manifest;
pub mod mount_lock;
pub mod segment;
pub mod snapshot;
pub mod store;
pub mod store_io;
pub mod wal;
pub mod xattr;
