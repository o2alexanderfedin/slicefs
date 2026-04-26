use slicefs_dedup::{DedupIndexConfig, DurabilityMode, RedbDedupIndex};
use slicefs_traits::{ChunkHash, DedupIndex, DedupResult};

fn make_h(seed: u8) -> [u8; 28] {
    let mut h = [0u8; 28];
    h[0] = seed;
    h
}

fn write_cas_block(cas_root: &std::path::Path, hash: &[u8; 28]) -> std::path::PathBuf {
    let hex: String = hash.iter().map(|b| format!("{:02x}", b)).collect();
    let dir = cas_root.join(&hex[..2]);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(&hex[2..]);
    std::fs::write(&path, b"x").unwrap();
    path
}

#[test]
fn t6_default_mode_does_not_verify_so_lookup_remains_present() {
    let td = tempfile::tempdir().unwrap();
    let cas = td.path().join("cas");
    std::fs::create_dir_all(&cas).unwrap();
    let cfg = DedupIndexConfig::builder(&cas)
        .mode(DurabilityMode::Default)
        .build();
    let idx = RedbDedupIndex::create(cfg).unwrap();

    let h = make_h(0x42);
    let cas_path = write_cas_block(&cas, &h);
    idx.insert(&ChunkHash::from_bytes(h.to_vec())).unwrap();
    idx.flush().unwrap();

    // Delete the CAS block on disk but leave the index entry.
    std::fs::remove_file(&cas_path).unwrap();

    // Default mode: verify_on_present=false, so we accept the FP-causing state.
    let r = idx.lookup(&ChunkHash::from_bytes(h.to_vec())).unwrap();
    assert!(
        matches!(r, DedupResult::Present),
        "Default mode trusts the index without stat()"
    );
}

#[test]
fn t6_paranoid_mode_demotes_to_absent_when_cas_missing() {
    let td = tempfile::tempdir().unwrap();
    let cas = td.path().join("cas");
    std::fs::create_dir_all(&cas).unwrap();
    let cfg = DedupIndexConfig::builder(&cas)
        .mode(DurabilityMode::Paranoid)
        .build();
    let idx = RedbDedupIndex::create(cfg).unwrap();

    let h = make_h(0x43);
    let cas_path = write_cas_block(&cas, &h);
    idx.insert(&ChunkHash::from_bytes(h.to_vec())).unwrap();
    idx.flush().unwrap();

    std::fs::remove_file(&cas_path).unwrap();

    // Paranoid mode: verify_on_present=true; missing CAS block demotes to Absent (I8).
    let r = idx.lookup(&ChunkHash::from_bytes(h.to_vec())).unwrap();
    assert!(
        matches!(r, DedupResult::Absent),
        "Paranoid mode must demote: stat() found ENOENT, so I1-violation suspected (I8)"
    );
}
