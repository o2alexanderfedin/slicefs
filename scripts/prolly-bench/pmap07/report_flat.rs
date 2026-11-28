//! The report module of the PMAP-07 commit benchmark for the flat build (tag
//! `v2.3.0`). `prepare-v230` of `pmap07.sh` copies it to
//! `crates/metadata/tests/support/pmap07_report.rs` of that worktree; it is
//! never compiled in this tree.
//!
//! The flat commit writes the six maps whole, so each object of their values
//! (and of the root record) counts as written work. It reads no node.
use chunk_store::ChunkStore;
use chunk_store::file::for_each_file_object;
use metadata::store::DictMetadataStore;
use metadata::store_io::{get_value, to_id};
use slicefs_traits::key::{ObjectKey, KEY_BYTES};

/// Offsets of the six map values in the 230-byte root record: the inode map,
/// then inode data, directory data, manifest data, xattr data, refcounts.
const MAP_OFFSETS: [usize; 6] = [0, 49, 82, 115, 148, 181];
const ROOT_RECORD_BYTES: usize = 230;

/// (nodes read, nodes written, pre-sync ms) of the commit that returned `root`.
pub fn nodes(meta: &DictMetadataStore, cs: &ChunkStore, root: &ObjectKey) -> (Option<u64>, Option<u64>, Option<f64>) {
    let _ = meta;
    let rec = get_value(cs, root).expect("read the root record");
    assert_eq!(rec.len(), ROOT_RECORD_BYTES, "root record size");
    let mut count = 0u64;
    for_each_file_object(cs, to_id(root), |_| count += 1).expect("walk the root record");
    for off in MAP_OFFSETS {
        let mut key = [0u8; KEY_BYTES];
        key.copy_from_slice(&rec[off..off + KEY_BYTES]);
        for_each_file_object(cs, to_id(&ObjectKey(key)), |_| count += 1).expect("walk a map value");
    }
    (Some(0), Some(count), None)
}
