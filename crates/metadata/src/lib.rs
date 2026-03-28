//! Metadata engine for SliceFS — inode table, directory tree, file manifests,
//! xattrs built on data-id's CAS Dictionary.
//!
//! This crate provides concrete implementations of the metadata traits defined
//! in `dedupfs-traits`. The storage backend is a data-id `Dictionary`.

pub mod inode;
pub mod inode_map;
pub mod directory;
pub mod manifest;
