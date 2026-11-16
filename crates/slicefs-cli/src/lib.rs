//! SliceFS CLI library — exposes filesystem adapter for integration tests.

pub mod backend;
pub mod cli;
pub mod commit_timer;
pub mod filesystem;
mod fuse_callbacks;
#[doc(hidden)]
pub mod handlers;
pub mod oplog;
pub mod gc;
pub mod online_gc;
pub mod mount;
pub mod snapshot;
pub mod seed;
mod store_io;
pub(crate) mod util;
mod unmount;
