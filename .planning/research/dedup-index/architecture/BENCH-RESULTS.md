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
