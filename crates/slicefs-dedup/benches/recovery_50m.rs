use criterion::{Criterion, black_box, criterion_group, criterion_main};
use slicefs_dedup::{DedupIndexConfig, RedbDedupIndex};
use std::time::Duration;

fn synth_cas_tree(cas: &std::path::Path, n: u64) {
    for shard in 0u8..=255 {
        let dir = cas.join(format!("{:02x}", shard));
        std::fs::create_dir_all(&dir).unwrap();
    }
    let per_shard = n / 256;
    for shard in 0u8..=255 {
        let dir = cas.join(format!("{:02x}", shard));
        for i in 0..per_shard {
            let mut h = [0u8; 28];
            h[0] = shard;
            h[1..9].copy_from_slice(&i.to_le_bytes());
            let hex: String = h[1..].iter().map(|b| format!("{:02x}", b)).collect();
            let p = dir.join(&hex);
            std::fs::write(&p, b"").unwrap();
        }
    }
}

fn recovery_50m(c: &mut Criterion) {
    let mut group = c.benchmark_group("recovery_50m");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(60));
    let n: u64 = std::env::var("DEDUP_BENCH_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1_000_000);
    group.bench_function(format!("recovery_n_{}", n), |b| {
        b.iter_custom(|_iters| {
            let td = tempfile::tempdir().unwrap();
            let cas = td.path().join("cas");
            std::fs::create_dir_all(&cas).unwrap();
            synth_cas_tree(&cas, n);
            let cfg = DedupIndexConfig::builder(&cas).build();
            let start = std::time::Instant::now();
            RedbDedupIndex::rebuild_from_cas(cfg).unwrap();
            let elapsed = start.elapsed();
            black_box(elapsed)
        });
    });
    group.finish();
}

criterion_group!(benches, recovery_50m);
criterion_main!(benches);
