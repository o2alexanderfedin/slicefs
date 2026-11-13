# SliceFS Dedup-Index Benchmark Results

Recorded outcomes for the bench suite per ARCHITECTURE §11 / §14.3.

## Reference Hardware
| Date | Host | OS | NVMe | Notes |
|------|------|-----|------|-------|
| 2026-04-25 | M7 | macOS 25.4.0 | Unitek-B (external) | initial reference |

## Results

### N1 — seed_burst (gate ≥ 100K ins/s)

| Metric | Value | Status |
|--------|-------|--------|
| Seed-mode burst (1M hashes) | 33.1 ms median | **PASS** |
| Throughput | ~30.2 M ins/s | **PASS (300.2x gate)** |
| Sample count | 100 | Stable |

**Interpretation:** Seed-mode BatchWriter achieves 30.2 million inserts/second with unique 28-byte hashes through the redb-backed index, flushed to disk. Performance far exceeds ARCHITECTURE §15.0.1 gate (≥100K ins/s), confirming v2 escalation (sharded redb / log-structured engine) is not immediately required on this reference hardware.

### N2 — lookup (warm p99 ≤ 10 µs, cold p99 ≤ 500 µs)

To run: `cargo bench -p slicefs-dedup --bench lookup -- --quick`. Bench compiles; full p50/p99 capture pending OOB measurement run. Targets per ARCHITECTURE §11.

### N3 — steady_mixed (≥20K ops/s)

To run: `cargo bench -p slicefs-dedup --bench steady_mixed -- --quick`. Bench compiles; throughput capture pending OOB run. Target ≥20K ops/s sustained across 80% inserts + 20% lookups.

### N4 — commit_latency (Default p99 ≤ 5 ms, Paranoid p99 ≤ 12 ms)

To run: `cargo bench -p slicefs-dedup --bench commit_latency -- --quick`. Bench compiles; per-mode p50/p99 capture pending OOB run. Targets per ARCHITECTURE §11 / §13.1.

### N5 — recovery_50m (RTO ≤ 30 s)

To run for the documented MVP-target N=50M: `DEDUP_BENCH_N=50000000 cargo bench -p slicefs-dedup --bench recovery_50m`. Default `--quick` uses N=1M (faster validation that the rebuild path works). Bench compiles; full RTO capture pending OOB run on reference NVMe.
