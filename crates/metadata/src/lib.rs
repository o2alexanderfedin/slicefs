//! Metadata engine for SliceFS — inode table, directory tree, file manifests,
//! xattrs built on data-id's CAS Dictionary.
//!
//! This crate provides concrete implementations of the metadata traits defined
//! in `slicefs-traits`. The storage backend is a data-id `Dictionary`.

pub mod inode;
pub mod inode_map;
pub mod directory;
pub mod manifest;
pub mod xattr;
pub mod store;
pub mod segment;
pub mod wal;
pub mod mount_lock;
pub mod gc;
