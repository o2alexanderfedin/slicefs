//! The per-tree report of the PMAP-07 commit benchmark (`commit_bench.rs`).
//!
//! `nodes` gives the nodes read and written by the last commit and its
//! pre-sync time, from the commit counters of the store
//! (`DictMetadataStore::last_commit_stats`). The flat build of tag `v2.3.0`
//! has its own copy in `scripts/prolly-bench/pmap07/report_flat.rs`.
use chunk_store::ChunkStore;
use metadata::store::DictMetadataStore;
use slicefs_traits::key::ObjectKey;

/// (nodes read, nodes written, pre-sync ms) of the last commit of `meta`.
pub fn nodes(meta: &DictMetadataStore, cs: &ChunkStore, root: &ObjectKey) -> (Option<u64>, Option<u64>, Option<f64>) {
    let _ = (cs, root);
    let s = meta.last_commit_stats();
    (Some(s.nodes_read), Some(s.nodes_written), Some(s.presync_ms))
}
