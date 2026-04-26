//! Integration test for K1 — `RedbDedupIndex::rebuild_from_cas`.
//!
//! Plants 200 synthetic CAS blocks under `cas_root/{shard}/...`, runs
//! the rebuild twice (idempotency, I7), opens the resulting index, and
//! verifies every planted hash now reports `DedupResult::Present`.

use slicefs_dedup::{DedupIndexConfig, RedbDedupIndex};
use slicefs_traits::{ChunkHash, DedupIndex, DedupResult};

fn write_cas_block(cas_root: &std::path::Path, hash: &[u8; 28]) {
    let hex: String = hash.iter().map(|b| format!("{:02x}", b)).collect();
    let dir = cas_root.join(&hex[..2]);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(&hex[2..]), b"x").unwrap();
}

#[test]
fn rebuild_from_cas_idempotent() {
    let td = tempfile::tempdir().unwrap();
    let cas = td.path().join("cas");
    std::fs::create_dir_all(&cas).unwrap();
    for i in 0..200u8 {
        let mut h = [0u8; 28];
        h[0] = i;
        write_cas_block(&cas, &h);
    }
    let cfg = DedupIndexConfig::builder(&cas).build();
    RedbDedupIndex::rebuild_from_cas(cfg.clone()).unwrap();
    // Run twice — idempotent.
    RedbDedupIndex::rebuild_from_cas(cfg.clone()).unwrap();

    let idx = RedbDedupIndex::open(cfg).unwrap();
    for i in 0..200u8 {
        let mut h = [0u8; 28];
        h[0] = i;
        let ch = ChunkHash::from_bytes(h.to_vec());
        assert!(matches!(idx.lookup(&ch).unwrap(), DedupResult::Present));
    }
}
