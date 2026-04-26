use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use slicefs_dedup::{DedupIndexConfig, DurabilityMode, RedbDedupIndex};
use slicefs_traits::{ChunkHash, DedupIndex};

fn commit_latency(c: &mut Criterion) {
    let mut group = c.benchmark_group("commit_latency");
    for mode in [DurabilityMode::Default, DurabilityMode::Paranoid] {
        let label = match mode {
            DurabilityMode::Default => "default",
            DurabilityMode::Paranoid => "paranoid",
            _ => "seed",
        };
        group.bench_with_input(BenchmarkId::from_parameter(label), &mode, |b, &mode| {
            b.iter_custom(|iters| {
                let td = tempfile::tempdir().unwrap();
                let cas = td.path().join("cas");
                std::fs::create_dir_all(&cas).unwrap();
                let cfg = DedupIndexConfig::builder(&cas).mode(mode).build();
                let idx = RedbDedupIndex::create(cfg).unwrap();
                let start = std::time::Instant::now();
                for i in 0..iters {
                    let mut h = [0u8; 28];
                    h[..8].copy_from_slice(&i.to_le_bytes());
                    idx.insert(&ChunkHash::from_bytes(h.to_vec())).unwrap();
                }
                idx.flush().unwrap();
                let elapsed = start.elapsed();
                black_box(elapsed)
            });
        });
    }
    group.finish();
}

criterion_group!(benches, commit_latency);
criterion_main!(benches);
