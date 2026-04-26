//! `StoreIo` re-export — delegates to `metadata::store_io`.
//!
//! Kept as a thin re-export module for historical reasons; current code imports
//! directly from `metadata::store_io::*`. Marked `#[allow(unused_imports)]` so
//! the re-exports remain available without tripping clippy.

#[allow(unused_imports)]
pub use metadata::store_io::EmptyArgs;
#[allow(unused_imports)]
pub use metadata::store_io::StoreIo;
