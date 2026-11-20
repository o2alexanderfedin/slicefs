//! SliceFS CLI library — exposes filesystem adapter for integration tests.

pub mod backend;
pub mod cli;
pub mod commit_timer;
pub mod content;
pub mod filesystem;
mod fuse_callbacks;
#[doc(hidden)]
pub mod handlers;
pub mod oplog;
pub mod overlay;
pub mod gc;
pub mod gc_pacing;
pub mod online_gc;
#[cfg(target_os = "macos")]
pub mod fuset_watch;
pub mod mount;
pub mod scrub;
pub mod snapshot;
pub mod seed;
pub mod signals;
pub mod stats;
pub(crate) mod util;
pub mod unmount;
