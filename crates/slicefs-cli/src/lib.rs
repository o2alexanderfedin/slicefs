//! SliceFS CLI library — exposes filesystem adapter for integration tests.

pub mod backend;
pub mod cli;
pub mod filesystem;
mod fuse_callbacks;
pub mod gc;
pub(crate) mod handlers;
pub mod mount;
pub mod seed;
pub mod snapshot;
mod store_io;
mod unmount;
pub(crate) mod util;
