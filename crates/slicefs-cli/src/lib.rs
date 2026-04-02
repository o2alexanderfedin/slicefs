//! SliceFS CLI library — exposes filesystem adapter for integration tests.

pub mod backend;
pub mod cli;
pub mod filesystem;
mod fuse_callbacks;
pub(crate) mod handlers;
pub mod gc;
pub mod mount;
pub mod snapshot;
pub mod seed;
mod store_io;
mod unmount;
