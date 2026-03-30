//! `StoreIo` re-export — delegates to `metadata::store_io`.
//!
//! All CLI code continues importing `StoreIo` from `crate::store_io::StoreIo`.
//! The actual implementation lives in the metadata crate to be shared with
//! `DictMetadataStore`.

pub use metadata::store_io::StoreIo;
pub use metadata::store_io::EmptyArgs;
