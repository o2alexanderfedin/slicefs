//! Metadata trait definitions and types for SliceFS.
//!
//! Defines MetadataStore trait, InodeMeta struct, InodeId type alias,
//! DirEntry, and MetaError. These are the foundational data types used
//! throughout the metadata engine (Phase 2+).

use crate::digest::Digest224;
use thiserror::Error;

/// Inode number type. Inode 1 is the FUSE root directory.
pub type InodeId = u64;

/// Typed error enum for metadata operations.
#[derive(Error, Debug)]
pub enum MetaError {
    /// No inode found with the given number.
    #[error("inode not found: {0}")]
    NotFound(InodeId),

    /// An inode with the given number already exists.
    #[error("inode already exists: {0}")]
    AlreadyExists(InodeId),

    /// The specified inode is not a directory.
    #[error("not a directory: {0}")]
    NotADirectory(InodeId),

    /// The specified inode is a directory (where a non-directory was expected).
    #[error("is a directory: {0}")]
    IsADirectory(InodeId),

    /// The directory is not empty (e.g., on rmdir).
    #[error("directory not empty: {0}")]
    NotEmpty(InodeId),

    /// A directory entry name is invalid (e.g., contains NUL, too long).
    #[error("invalid name: {0}")]
    InvalidName(String),

    /// On-disk data is corrupted or cannot be deserialized.
    #[error("corrupted data: {0}")]
    Corrupted(String),

    /// An underlying I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// A single directory entry returned by `list_directory`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// Entry name (not a path — just the filename component).
    pub name: String,
    /// Inode number for this entry.
    pub ino: InodeId,
}

/// Inode metadata. Represents the POSIX inode fields stored per file or directory.
///
/// This struct is intentionally kept as plain data — no I/O, no storage logic.
/// Serialization lives in `crates/metadata/src/inode.rs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InodeMeta {
    /// Inode number.
    pub ino: u64,
    /// File mode (type bits + permission bits, as in `stat(2)`).
    pub mode: u32,
    /// Owner user ID.
    pub uid: u32,
    /// Owner group ID.
    pub gid: u32,
    /// Hard link count.
    pub nlinks: u32,
    /// File size in bytes.
    pub size: u64,
    /// Last modification time — seconds since Unix epoch.
    pub mtime_sec: i64,
    /// Last modification time — nanosecond fraction.
    pub mtime_nsec: u32,
    /// Last metadata change time — seconds since Unix epoch.
    pub ctime_sec: i64,
    /// Last metadata change time — nanosecond fraction.
    pub ctime_nsec: u32,
}

impl InodeMeta {
    /// Create a new directory inode with sensible defaults.
    ///
    /// Sets `nlinks = 2` (the `.` and `..` hard-link convention for directories),
    /// `size = 0`, and all timestamps to 0.
    pub fn new_directory(ino: InodeId, uid: u32, gid: u32, mode: u32) -> Self {
        Self {
            ino,
            mode,
            uid,
            gid,
            nlinks: 2,
            size: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            ctime_sec: 0,
            ctime_nsec: 0,
        }
    }

    /// Create a new regular file inode with sensible defaults.
    ///
    /// Sets `nlinks = 1`, `size = 0`, and all timestamps to 0.
    pub fn new_file(ino: InodeId, uid: u32, gid: u32, mode: u32) -> Self {
        Self {
            ino,
            mode,
            uid,
            gid,
            nlinks: 1,
            size: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            ctime_sec: 0,
            ctime_nsec: 0,
        }
    }
}

/// Core metadata operations for the SliceFS inode store.
///
/// All methods take `&self` (not `&mut self`) so implementations can use
/// interior mutability (`Mutex`, `RwLock`) and be shared behind `Arc<dyn MetadataStore>`.
///
/// The store is backed by a data-id Dictionary. `commit()` flushes in-memory state
/// and returns the root Digest224 that uniquely identifies the filesystem snapshot.
pub trait MetadataStore: Send + Sync {
    /// Assign a new inode number and store `meta`. Returns the assigned `InodeId`.
    fn create_inode(&self, meta: &InodeMeta) -> Result<InodeId, MetaError>;

    /// Retrieve the metadata for an existing inode.
    fn get_inode(&self, ino: InodeId) -> Result<InodeMeta, MetaError>;

    /// Update the metadata of an existing inode (must have the same `ino`).
    fn update_inode(&self, meta: &InodeMeta) -> Result<(), MetaError>;

    /// Delete an inode record. Idempotent: deleting a non-existent inode is an error.
    fn delete_inode(&self, ino: InodeId) -> Result<(), MetaError>;

    /// Create a directory inode under `parent_ino` with the given `name`.
    ///
    /// Stores `meta` as the new directory's inode, adds `.` (pointing to itself)
    /// and `..` (pointing to `parent_ino`) entries.
    fn create_directory(
        &self,
        parent_ino: InodeId,
        name: &str,
        meta: &InodeMeta,
    ) -> Result<InodeId, MetaError>;

    /// Return all entries in the directory identified by `ino`.
    fn list_directory(&self, ino: InodeId) -> Result<Vec<DirEntry>, MetaError>;

    /// Look up a directory entry by name in directory `parent_ino`.
    fn lookup(&self, parent_ino: InodeId, name: &str) -> Result<InodeId, MetaError>;

    /// Add a hard link entry `name -> ino` in directory `parent_ino`.
    fn link(&self, parent_ino: InodeId, name: &str, ino: InodeId) -> Result<(), MetaError>;

    /// Remove the directory entry named `name` from directory `parent_ino`.
    fn unlink(&self, parent_ino: InodeId, name: &str) -> Result<(), MetaError>;

    /// Store the ordered list of block keys (manifest) for file inode `ino`.
    fn set_manifest(&self, ino: InodeId, blocks: &[Digest224]) -> Result<(), MetaError>;

    /// Retrieve the manifest (ordered block keys) for file inode `ino`.
    fn get_manifest(&self, ino: InodeId) -> Result<Vec<Digest224>, MetaError>;

    /// Set an extended attribute `name` to `value` on inode `ino`.
    fn set_xattr(&self, ino: InodeId, name: &str, value: &[u8]) -> Result<(), MetaError>;

    /// Get the value of extended attribute `name` on inode `ino`.
    fn get_xattr(&self, ino: InodeId, name: &str) -> Result<Vec<u8>, MetaError>;

    /// List the names of all extended attributes on inode `ino`.
    fn list_xattrs(&self, ino: InodeId) -> Result<Vec<String>, MetaError>;

    /// Remove extended attribute `name` from inode `ino`.
    fn remove_xattr(&self, ino: InodeId, name: &str) -> Result<(), MetaError>;

    /// Return the root inode number (always 1 for FUSE root).
    fn root_ino(&self) -> InodeId;

    /// Persist the current in-memory state to the Dictionary and return the root digest.
    ///
    /// Callers use the returned `Digest224` as the filesystem snapshot identifier.
    fn commit(&self) -> Result<Digest224, MetaError>;
}
