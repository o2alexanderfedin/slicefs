---
phase: 6
slug: compression-and-snapshots
status: draft
nyquist_compliant: false
wave_0_complete: false
created: 2026-03-29
---

# Phase 6 — Validation Strategy

> Per-phase validation contract for feedback sampling during execution.

---

## Test Infrastructure

| Property | Value |
|----------|-------|
| **Framework** | Rust built-in `#[test]` via `cargo test` |
| **Config file** | none (workspace uses `cargo test` directly) |
| **Quick run command** | `cargo test -p slicefs-compression && cargo test -p metadata 2>&1 \| tail -20` |
| **Full suite command** | `cargo test --workspace 2>&1 \| tail -30` |
| **Estimated runtime** | ~30 seconds |

---

## Sampling Rate

- **After every task commit:** Run `cargo test -p slicefs-compression && cargo test -p metadata 2>&1 | tail -20`
- **After every plan wave:** Run `cargo test --workspace 2>&1 | tail -30`
- **Before `/gsd:verify-work`:** Full suite must be green
- **Max feedback latency:** 30 seconds

---

## Per-Task Verification Map

| Task ID | Plan | Wave | Requirement | Test Type | Automated Command | File Exists | Status |
|---------|------|------|-------------|-----------|-------------------|-------------|--------|
| 06-01-01 | 01 | 1 | COMP-01 | unit | `cargo test -p slicefs-compression test_zstd_compressor_reduces_size -x` | Wave 0 | ⬜ pending |
| 06-01-02 | 01 | 1 | COMP-01 | unit | `cargo test -p slicefs-compression test_lz4_compressor_reduces_size -x` | Wave 0 | ⬜ pending |
| 06-01-03 | 01 | 1 | COMP-01 | unit | `cargo test -p slicefs-compression test_none_compressor_passthrough -x` | Wave 0 | ⬜ pending |
| 06-01-04 | 01 | 1 | COMP-01 | unit | `cargo test -p slicefs-compression test_incompressible_stored_raw -x` | Wave 0 | ⬜ pending |
| 06-01-05 | 01 | 1 | COMP-01 | unit | `cargo test -p slicefs-compression test_compress_decompress_roundtrip -x` | Wave 0 | ⬜ pending |
| 06-02-01 | 02 | 1 | COMP-02 | unit | `cargo test -p metadata test_dedup_content_hash_independent_of_compressor -x` | Wave 0 | ⬜ pending |
| 06-02-02 | 02 | 1 | SNAP-01 | unit | `cargo test -p metadata test_create_snapshot_returns_version -x` | Wave 0 | ⬜ pending |
| 06-02-03 | 02 | 1 | SNAP-01 | integration | `cargo test -p slicefs-cli test_snapshot_files_readable -x` | Wave 0 | ⬜ pending |
| 06-02-04 | 02 | 1 | SNAP-02 | unit | `cargo test -p metadata test_list_snapshots_ordered -x` | Wave 0 | ⬜ pending |
| 06-02-05 | 02 | 1 | SNAP-02 | unit | `cargo test -p metadata test_switch_root_updates_live_pointer -x` | Wave 0 | ⬜ pending |
| 06-02-06 | 02 | 1 | SNAP-03 | unit | `cargo test -p metadata test_shared_blocks_not_double_counted -x` | Wave 0 | ⬜ pending |
| 06-02-07 | 02 | 1 | GC-03 | unit | `cargo test -p metadata test_gc_preserves_snapshot_blocks -x` | Wave 0 | ⬜ pending |

*Status: ⬜ pending · ✅ green · ❌ red · ⚠️ flaky*

---

## Wave 0 Requirements

- [ ] `crates/slicefs-compression/src/lib.rs` — new crate, Compressor trait + Zstd/LZ4/None implementations
- [ ] `crates/slicefs-compression/tests/compressor_tests.rs` — COMP-01, COMP-02 test stubs
- [ ] `crates/metadata/tests/snapshot_tests.rs` — SNAP-01, SNAP-02, SNAP-03, GC-03 test stubs

---

## Manual-Only Verifications

| Behavior | Requirement | Why Manual | Test Instructions |
|----------|-------------|------------|-------------------|
| Snapshot mount read-only via FUSE | SNAP-01 | Requires FUSE kernel module | Mount with `--snapshot`, verify `ls`/`cat` work, verify writes return EROFS |
| Version switch with unmount/remount | SNAP-02 | Requires FUSE kernel module | Create snapshot, modify files, switch back, verify content matches snapshot |

---

## Validation Sign-Off

- [ ] All tasks have `<automated>` verify or Wave 0 dependencies
- [ ] Sampling continuity: no 3 consecutive tasks without automated verify
- [ ] Wave 0 covers all MISSING references
- [ ] No watch-mode flags
- [ ] Feedback latency < 30s
- [ ] `nyquist_compliant: true` set in frontmatter

**Approval:** pending
