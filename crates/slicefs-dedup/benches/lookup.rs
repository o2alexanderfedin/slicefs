use criterion::{black_box, criterion_group, criterion_main, Criterion};
use slicefs_dedup::{DedupIndexConfig, RedbDedupIndex};
use slicefs_traits::{ChunkHash, DedupIndex};

fn build_idx(n: usize) -> (tempfile::TempDir, RedbDedupIndex, Vec<[u8; 28]>) {
    let td = tempfile::tempdir().unwrap();
    let cas = td.path().join("cas");
    std::fs::create_dir_all(&cas).unwrap();
    let cfg = DedupIndexConfig::builder(&cas).build();
    let idx = RedbDedupIndex::create(cfg).unwrap();
    let mut hashes = Vec::with_capacity(n);
    for i in 0..n {
        let mut h = [0u8; 28];
        h[..8].copy_from_slice(&(i as u64).to_le_bytes());
        idx.insert(&ChunkHash::from_bytes(h.to_vec())).unwrap();
        hashes.push(h);
    }
    idx.flush().unwrap();
    (td, idx, hashes)
}

fn lookup_warm(c: &mut Criterion) {
    let (_td, idx, hashes) = build_idx(100_000);
    c.bench_function("lookup_warm_present", |b| {
        let mut i = 0usize;
        b.iter(|| {
            let h = &hashes[i % hashes.len()];
            i = i.wrapping_add(1);
            let r = idx.lookup(&ChunkHash::from_bytes(h.to_vec())).unwrap();
            black_box(r);
        });
    });

    c.bench_function("lookup_warm_definitely_absent", |b| {
        let mut i = 0u64;
        b.iter(|| {
            let mut h = [0u8; 28];
            // Above the inserted range — bloom miss.
            h[..8].copy_from_slice(&(1_000_000 + i).to_le_bytes());
            i = i.wrapping_add(1);
            let r = idx.lookup(&ChunkHash::from_bytes(h.to_vec())).unwrap();
            black_box(r);
        });
    });
}

fn lookup_cold(c: &mut Criterion) {
    c.bench_function("lookup_cold", |b| {
        b.iter_custom(|iters| {
            let (td, _, hashes) = build_idx(50_000);
            let cas = td.path().join("cas");
            let cfg = DedupIndexConfig::builder(&cas).build();
            let start = std::time::Instant::now();
            for i in 0..iters {
                let idx = RedbDedupIndex::open(cfg.clone()).unwrap();
                let h = &hashes[(i as usize) % hashes.len()];
                let r = idx.lookup(&ChunkHash::from_bytes(h.to_vec())).unwrap();
                black_box(r);
                drop(idx);
            }
            start.elapsed()
        });
    });
}

criterion_group!(benches, lookup_warm, lookup_cold);
criterion_main!(benches);
