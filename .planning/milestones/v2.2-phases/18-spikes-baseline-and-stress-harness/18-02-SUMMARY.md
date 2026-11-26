---
phase: 18-spikes-baseline-and-stress-harness
plan: 02
subsystem: testing
tags: [lock-stats, hdrhistogram, metadata, wal, slicefs-cli]
requires:
  - phase: 18-01
    provides: chunk_store::lock_stats recorder (Site, lock, Guard, snapshot, reset)
provides:
  - "Hold and wait timing for all seven lock labels: store, io, wal, wal_writer, wal_pending, open_files, readers"
  - "metadata lock-stats feature, forwarded from slicefs-cli"
  - "tests/lock_stats_report.rs: separate test binary that asserts a non-zero count for every label"
affects: [18-03, 19]
tech-stack:
  added: []
  patterns:
    - "Mechanical lock conversion: x.lock().unwrap() becomes lock_stats::lock(&x, Site::..); feature off inlines to the same call"
key-files:
  created:
    - crates/slicefs-cli/tests/lock_stats_report.rs
  modified:
    - crates/metadata/Cargo.toml
    - crates/metadata/src/store.rs
    - crates/metadata/src/wal/per_op.rs
    - crates/metadata/src/wal/buffered.rs
    - crates/slicefs-cli/Cargo.toml
    - crates/slicefs-cli/src/filesystem.rs
key-decisions:
  - "The report test is its own test binary, so the process-global recorder is not shared with the harness tests"
  - "read_root still holds the readers guard across Reader::read; narrowing it is Phase 19 work and S3 measures it as it is"
  - "Test-module and #[cfg(test)] lock sites stay as plain lock().unwrap()"
requirements-completed: []
duration: ~35 min
completed: 2026-11-21
status: complete
actuals:
  tokens: 6500
  tasks: 2
  commits: 2
plan_head_before: c7cc442a4c1051d435605f7cde559c76a45ab08c
plan_head_after: 94082aafd5420bf921a3bc40bdd488cb02e873f9
commits: 2
---

# Phase 18 Plan 02: Lock timing for io, wal, WAL strategy, open_files and readers Summary

All seven lock labels from CONTEXT now record hold and wait times when built with `--features lock-stats`; a separate report test proves each label gets samples, and the feature-off build is unchanged.

## Tasks

| Task | Name | Commit |
|------|------|--------|
| 1 (tracer) | io and wal labels in metadata, forwarded feature, report test for store, io, wal | aeaeec0 |
| 2 | wal_writer, wal_pending, open_files, readers labels; test asserts all seven | 94082aa |

## Results

- Converted sites: 16 `io` and 4 `wal` in `store.rs`; 6 `writer` in `per_op.rs`; 5 `pending` and 5 `writer` in `buffered.rs`; 36 `open_files` and 2 `readers` in `filesystem.rs`. The remaining `.lock()` sites are the test ones the plan names (2, 3 and 1).
- `slicefs-cli` feature is now `lock-stats = ["chunk-store/lock-stats", "metadata/lock-stats"]`; `metadata` has `lock-stats = ["chunk-store/lock-stats"]`.
- `cargo test -p slicefs-cli --features lock-stats --test lock_stats_report -- --nocapture` passes in about 0.4 s. Sample rows: `store n=72`, `io n=12`, `wal n=13`, `wal_writer n=10`, `wal_pending n=7`, `open_files n=30`, `readers n=1`. The read-back of 64 KiB after release and commit is checked in the same test (it goes through `read_root`, which is what makes `readers` non-zero).
- Without the feature the same test binary builds and prints `running 0 tests`.
- `cargo test --workspace` (feature off) exits 0 with no FAILED line. `cargo check --workspace --all-targets --features slicefs-cli/lock-stats` and `cargo check -p metadata --all-targets --features lock-stats` exit 0. `cargo test -p metadata`: 137 unit tests pass.
- First rows already show what Phase 19 needs: `wal_writer` and `wal_pending` max hold is about 21 ms (the fsync of the log writer, under the lock), while `open_files` max hold is about 25 us.

## Deviations from Plan

None - plan executed as written. Note: the tracer gate re-ran the verify commands (report test, metadata tests, check with feature) before Task 2; all passed.

## Known Stubs

None.

## Threat Flags

None. No new network, auth, file-access or schema surface; the lock conversions take the same lock in the same order (T-18-04, T-18-06), and the recorder is a leaf mutex that records after the site lock is released (T-18-05).

## Self-Check: PASSED

- FOUND: crates/slicefs-cli/tests/lock_stats_report.rs
- FOUND commits: aeaeec0, 94082aa
