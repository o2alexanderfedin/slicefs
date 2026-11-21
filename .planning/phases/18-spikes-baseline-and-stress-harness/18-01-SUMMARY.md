---
phase: 18-spikes-baseline-and-stress-harness
plan: 01
subsystem: testing
tags: [stress-harness, hdrhistogram, lock-stats, chunk-store, slicefs-cli]
requires: []
provides:
  - "In-process stress harness (tests/stress_harness.rs): config, block-version model, serialize mode, commit gate timing, latency report, short_one_file"
  - "chunk_store::lock_stats: feature-gated Site, lock(), Guard, snapshot(), reset(), ENABLED"
  - "lock-stats cargo feature in chunk-store and slicefs-cli"
affects: [18-02, 18-03, 19, 20, 21]
tech-stack:
  added: [hdrhistogram 7.6 (dev-dependency of slicefs-cli; optional in chunk-store), byteorder 1.5 (transitive)]
  patterns:
    - "Feature-gated guard alias: lock_stats::Guard is the std MutexGuard when the feature is off"
    - "Model keeps block versions only; bytes are a pure function of (seed, file, block, version), so no file content is held in RAM"
key-files:
  created:
    - crates/slicefs-cli/tests/stress_harness.rs
    - crates/chunk-store/src/lock_stats.rs
  modified:
    - crates/chunk-store/Cargo.toml
    - crates/chunk-store/src/lib.rs
    - crates/slicefs-cli/Cargo.toml
    - Cargo.lock
key-decisions:
  - "D-02 followed: hdrhistogram is an optional normal dependency of chunk-store behind lock-stats; no hand-written buckets"
  - "Config is Clone, not Copy (WalConfig is not Copy)"
  - "The write read-back check is counted in the read histogram, so op=read n is larger than the number of read-only ops"
requirements-completed: [SPIKE-02]
duration: ~25 min
completed: 2026-11-21
status: complete
actuals:
  tokens: 8400
  tasks: 2
  commits: 2
plan_head_before: b4d148155084fe68680ae8751098ee37d07de447
plan_head_after: 52744c47835981881bef07ab55e3d1534b44d5a5
commits: 2
---

# Phase 18 Plan 01: Stress harness tracer and store-lock timing Summary

In-process 4-thread stress harness on one shared file (serialize on, block-version model, hdrhistogram latency and commit-gate rows), plus a `lock-stats` feature that times the store lock (`ChunkStore.inner`) with one hdrhistogram per site.

## Tasks

| Task | Name | Commit |
|------|------|--------|
| 1 (tracer) | End-to-end harness: 4 threads, one file, write and read, model check, latency report | 55ac854 |
| 2 | Store-lock hold and wait recorder behind lock-stats, shown in the harness report | 52744c4 |

## Results

- `short_one_file` passes in about 0.6 s (budget 5 s), with and without `--features lock-stats`. The seed is printed first and put in every assertion message; `STRESS_SEED` repeats a run.
- Sample report (feature on): `op=write p50 ~100 us, p99 ~2 ms`, `op=gate_hold p99 ~1 ms`, `lock site=store n=2970 wait p50/p99/max = 0/0/0 us, hold p50 1.3 us, p99 36 us, max 196 us`. With serialize on the store lock never waits, as expected.
- Harness can fail: a mutation (write bytes of version v+1 instead of v) made the test fail with `MISMATCH ... bytes match no version`. The mutation was reverted before commit.
- `cargo tree -p slicefs-cli -e normal`: 0 hdrhistogram lines with the feature off, 1 with `--features lock-stats`.
- `cargo test -p chunk-store` passes with the feature off and on. `cargo check --workspace --all-targets` and `cargo check -p slicefs-cli --all-targets --features lock-stats` exit 0.
- 18 store-lock sites use `lock_stats::lock(&self.inner, Site::Store)`; the one `if let Ok` in `impl Drop for ChunkStore` stays untimed (a drop must not panic on poison).

## Deviations from Plan

None - plan executed as written. Small notes:
- `Config` derives `Clone` only, because `WalConfig` is not `Copy`; call sites use `cfg.clone()`.
- The `MetadataStore` trait import was needed for `fs.meta().commit()`.
- Only `short_one_file` exists; the serialize-off ignored target and env-var long run are planned for later plans (03).

## Known Stubs

None.

## Threat Flags

None. The harness prints only seed, counts and times.

## Self-Check: PASSED

- FOUND: crates/slicefs-cli/tests/stress_harness.rs, crates/chunk-store/src/lock_stats.rs
- FOUND commits: 55ac854, 52744c4
