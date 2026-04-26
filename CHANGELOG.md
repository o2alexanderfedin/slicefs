# Changelog

## [v2.0.0-dedup-index.1] — 2026-04-26

> SliceFS v2.0 milestone alpha — first ship of the persistent on-disk
> `DedupIndex`. Per-crate versions are `0.2.0`; this tag locates the
> milestone in the v2.0 line. Subsequent v2.0 milestones use the same
> pattern (e.g. `v2.0.0-streaming-writes.1`).

### Added
- New `slicefs-dedup` crate: persistent on-disk `DedupIndex` backed by redb 4.x.
  - `RedbDedupIndex` with bloom-front + redb authoritative + optional verify-on-present.
  - Single-writer batcher with per-mode group-commit windows (Seed / Default / Paranoid).
  - Atomic bloom snapshots (xxh3-128 + CRC32C).
  - Atomic JSON manifest sidecar (CRC32C).
  - Idempotent `rebuild_from_cas`.
  - `Drop` writes clean-shutdown manifest; subsequent mounts probe for Healthy/Suspect/Rebuilding.
- `DedupIndex` trait extended with `flush()` / `verify()` / `stats()` (default-impl no-ops; `MemDedupIndex` compiles unchanged).
- CLI: `slicefs reindex --offline`, `slicefs dedup-recover`, extended `slicefs stats` `[Index]` block.
- Failure-injection test suite: kill-9 mid-commit (FI-1, FI-2), 100x concurrent stress (FI-10),
  caller-cached-Ok across crash (FI-13), verify-on-present demote (FI-6), Linux power-fail
  scaffold (FI-9), macOS F_FULLFSYNC shim canary (FI-11).
- Property tests: parity vs `MemDedupIndex`, open-close-open preserves the set.
- Criterion benches: seed_burst (gate ≥ 100K ins/s), lookup warm/cold, steady_mixed,
  per-mode commit_latency, recovery_50m.
- git-flow workflow: `develop` branch + `.githooks/pre-push` enforcing merge-commit-only on `main`.

### Changed
- redb 3.1 → 4.1 across the workspace.
- Workspace Cargo.toml gained: xxhash-rust (xxh3-128), crc32c, metrics, parking_lot,
  crossbeam-channel.
- Default durability mode maps `Durability::Eventual` → `Durability::Immediate` (redb 4.x
  removed `Eventual`); the batcher's coalesce window provides equivalent group-commit
  amortization with stronger durability (zero loss-window).
- `Cargo.toml` excludes `crates/data-id/blockset` from the workspace (upstream submodule).

### Internal
- Workspace-wide `cargo fmt`, `cargo clippy --all-targets -- -D warnings` clean across
  all SliceFS-owned crates (slicefs-traits, cas-local, metadata, slicefs-cli,
  slicefs-compression, slicefs-dedup, slicefs-dedup-fi-shim).
- 1312 tests passing (1 ignored — FI-10 stress test, opt-in via --ignored).

### See also
- `.planning/research/dedup-index/ARCHITECTURE.md` — binding spec.
- `docs/superpowers/plans/2026-04-25-dedup-index.md` — implementation plan.
