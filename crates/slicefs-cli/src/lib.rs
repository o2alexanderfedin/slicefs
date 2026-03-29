//! SliceFS CLI library — exposes filesystem adapter for integration tests.

pub mod cli;
pub mod filesystem;
pub mod gc;
pub mod mount;
pub mod snapshot;
mod seed;
mod store_io;
mod unmount;
