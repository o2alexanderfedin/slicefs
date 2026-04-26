use criterion::{black_box, criterion_group, criterion_main, Criterion};
use slicefs_dedup::{DedupIndexConfig, RedbDedupIndex};
use slicefs_traits::{ChunkHash, DedupIndex};

fn steady_mixed(c: &mut Criterion) {
    c.bench_function("steady_mixed_80i_20l", |b| {
        b.iter_custom(|iters| {
            let td = tempfile::tempdir().unwrap();
            let cas = td.path().join("cas");
            std::fs::create_dir_all(&cas).unwrap();
            let cfg = DedupIndexConfig::builder(&cas).build();
            let idx = RedbDedupIndex::create(cfg).unwrap();
            for i in 0u64..10_000 {
                let mut h = [0u8; 28];
                h[..8].copy_from_slice(&i.to_le_bytes());
                idx.insert(&ChunkHash::from_bytes(h.to_vec())).unwrap();
            }
            idx.flush().unwrap();

            let n = iters;
            let start = std::time::Instant::now();
            let mut next_seed = 1_000_000u64;
            for i in 0..n {
                if i % 5 == 4 {
                    let target = if i % 10 == 4 { i % 10_000 } else { 1_500_000 + i };
                    let mut h = [0u8; 28];
                    h[..8].copy_from_slice(&target.to_le_bytes());
                    let r = idx.lookup(&ChunkHash::from_bytes(h.to_vec())).unwrap();
                    black_box(r);
                } else {
                    let mut h = [0u8; 28];
                    h[..8].copy_from_slice(&next_seed.to_le_bytes());
                    next_seed += 1;
                    idx.insert(&ChunkHash::from_bytes(h.to_vec())).unwrap();
                }
            }
            start.elapsed()
        });
    });
}

criterion_group!(benches, steady_mixed);
criterion_main!(benches);
