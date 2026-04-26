use criterion::{black_box, criterion_group, criterion_main, Criterion};
use slicefs_dedup::{DedupIndexConfig, DurabilityMode, RedbDedupIndex};
use slicefs_traits::{ChunkHash, DedupIndex};

fn seed_burst(c: &mut Criterion) {
    c.bench_function("seed_burst_1m", |b| {
        b.iter_custom(|iters| {
            let td = tempfile::tempdir().unwrap();
            let cas = td.path().join("cas");
            std::fs::create_dir_all(&cas).unwrap();
            let cfg = DedupIndexConfig::builder(&cas)
                .mode(DurabilityMode::Seed)
                .build();
            let idx = RedbDedupIndex::create(cfg).unwrap();
            let n = (iters as usize).min(1_000_000);
            let start = std::time::Instant::now();
            for i in 0..n {
                let mut h = [0u8; 28];
                h[..8].copy_from_slice(&(i as u64).to_le_bytes());
                idx.insert(&ChunkHash::from_bytes(h.to_vec())).unwrap();
            }
            idx.flush().unwrap();
            black_box(start.elapsed())
        });
    });
}

criterion_group!(benches, seed_burst);
criterion_main!(benches);
