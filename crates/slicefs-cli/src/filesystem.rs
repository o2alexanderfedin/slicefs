//! SliceFsFilesystem — FUSE adapter stub (filled in by Task 2).
//!
//! This file is intentionally minimal for the Task 1 compile check.
//! Task 2 replaces the entire body with the full implementation.

use std::sync::{Arc, Mutex};

use blockset::Dictionary;
use metadata::store::DictMetadataStore;

/// Read-only FUSE filesystem adapter backed by a `DictMetadataStore`.
pub struct SliceFsFilesystem {
    pub(crate) meta: Arc<DictMetadataStore>,
    pub(crate) dict: Arc<Mutex<Dictionary>>,
}

impl SliceFsFilesystem {
    /// Construct a new filesystem from a metadata store and a dictionary clone.
    pub fn new(meta: DictMetadataStore, dict: Dictionary) -> Self {
        Self {
            meta: Arc::new(meta),
            dict: Arc::new(Mutex::new(dict)),
        }
    }

    /// Access the metadata store (used by the mount command after session ends).
    pub fn meta(&self) -> &Arc<DictMetadataStore> {
        &self.meta
    }

    /// Access the content dictionary (used by the mount command for serialization).
    pub fn dict(&self) -> &Arc<Mutex<Dictionary>> {
        &self.dict
    }
}
