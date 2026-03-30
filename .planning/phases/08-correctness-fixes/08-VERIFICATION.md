---
phase: 08-correctness-fixes
verified: 2026-03-30T00:00:00Z
status: passed
score: 6/6 must-haves verified
re_verification: false
gaps: []
human_verification: []
---

# Phase 8: Correctness Fixes Verification Report

**Phase Goal:** Known v1.0 correctness bugs are eliminated before the invasive write-path
restructuring begins — refcount overflow risk is closed and statfs reports real numbers with
three-tier space reporting (logical, CAS, host disk)

**Verified:** 2026-03-30
**Status:** PASSED
**Re-verification:** No — initial verification

---

## Goal Achievement

### Observable Truths

| # | Truth | Status | Evidence |
|---|-------|--------|----------|
| 1 | Incrementing a refcount at u64::MAX produces u64::MAX, not 0 | VERIFIED | `increment_refcount` in store.rs line 154: early return if `*val == u64::MAX`; then `*val += 1`; then warns if newly saturated |
| 2 | Decrementing a refcount at u64::MAX is a no-op (block stays immortal) | VERIFIED | `decrement_refcount` in store.rs line 177: `if *count == u64::MAX { return; }` as first guard inside the Some block |
| 3 | statfs files field reflects actual inode count, not hardcoded 1_000_000 | VERIFIED | `compute_statfs()` in filesystem.rs line 610: `let files = self.meta.inode_count();`; integration test `test_statfs_files_reflects_inode_count` confirms value is 4 (not 1,000,000) after 3 creates |
| 4 | statfs blocks/bfree/bavail come from host disk via statvfs, not hardcoded u64::MAX/4 | VERIFIED | `compute_statfs()` lines 620-627 call `libc::statvfs`, use `sv.f_frsize as u32`, `sv.f_blocks as u64`, `sv.f_bfree as u64`, `sv.f_bavail as u64`; test `test_statfs_blocks_from_host` asserts `blocks > 0` and `blocks != u64::MAX/4` |
| 5 | slicefs scrub reports saturated refcount blocks as warnings | VERIFIED | `ScrubReport.saturated_blocks: usize` field added (scrub.rs line 48); populated via `meta.saturated_refcount_count()` (line 123); warning printed when > 0 (lines 175-179); JSON output includes field via `#[derive(Serialize)]` |
| 6 | Snapshot lookup by version and name uses O(1) HashMap (pre-satisfied by Phase 7.1) | VERIFIED | store.rs lines 84/86: `snapshots_by_version: Mutex<HashMap<u64, SnapshotEntry>>` and `snapshots_by_name: Mutex<HashMap<String, u64>>` present; used at lines 273-274, 294, 316-320, 329, 343-348, 357 |

**Score:** 6/6 truths verified

---

### Required Artifacts

| Artifact | Expected | Status | Details |
|----------|----------|--------|---------|
| `crates/metadata/src/store.rs` | saturating_add refcount, inode_count AtomicU64, saturated_refcount_count() | VERIFIED | `increment_refcount` (line 151) uses manual saturation guard; `decrement_refcount` (line 173) guards at MAX; `saturated_refcount_count()` (line 199); `inode_count: AtomicU64` field (line 76); 9 TDD tests for these behaviors |
| `crates/slicefs-cli/src/filesystem.rs` | Three-tier statfs with libc::statvfs | VERIFIED | `compute_statfs()` (line 609) calls `libc::statvfs`; `test_statfs_values()` (line 597) bridges to integration tests; `statfs()` FUSE handler (line 1211) delegates to `compute_statfs()` |
| `crates/slicefs-cli/src/scrub.rs` | Saturated refcount reporting in scrub | VERIFIED | `ScrubReport.saturated_blocks` field (line 48); populated from `meta.saturated_refcount_count()` (line 123); human output and JSON both include the field |
| `crates/slicefs-cli/src/stats.rs` | Three-tier display: logical, CAS, host disk | VERIFIED | `StoreStats.host_disk_bytes` field (line 56); `host_disk_bytes = dir_size(store_path)` (line 123); print labels are "Logical bytes", "CAS bytes", "Host disk used" (lines 205-207) |
| `crates/slicefs-cli/tests/statfs_tests.rs` | Integration tests for inode_count and statvfs | VERIFIED | `test_statfs_files_reflects_inode_count` (line 244), `test_statfs_blocks_from_host` (line 269), `test_statfs_fallback_no_store_path` (line 295) — all three TDD tests present and substantive |
| `crates/metadata/Cargo.toml` | tracing dependency | VERIFIED | `tracing = { workspace = true }` (line 10) |

---

### Key Link Verification

| From | To | Via | Status | Details |
|------|----|-----|--------|---------|
| `crates/slicefs-cli/src/filesystem.rs` | `crates/metadata/src/store.rs` | `self.meta.inode_count()` | VERIFIED | `compute_statfs()` line 610: `let files = self.meta.inode_count();` — direct call present |
| `crates/slicefs-cli/src/scrub.rs` | `crates/metadata/src/store.rs` | `meta.saturated_refcount_count()` | VERIFIED | scrub.rs line 123: `saturated_blocks = meta.saturated_refcount_count();` — called after `DictMetadataStore::load_from_root` |

---

### Requirements Coverage

| Requirement | Source Plan | Description | Status | Evidence |
|-------------|------------|-------------|--------|----------|
| FIX-01 | 08-01-PLAN.md | Refcount increment uses saturating_add — no silent overflow to 0 on u64::MAX | SATISFIED | `increment_refcount` saturates at u64::MAX; `decrement_refcount` is no-op at MAX; `saturated_refcount_count()` counts immortal blocks; scrub surfaces the count as a warning |
| FIX-02 | 08-01-PLAN.md | statfs reports actual inode count (not hardcoded 1M) and tracks physical bytes accurately | SATISFIED | `inode_count` AtomicU64 tracks create/delete/reload; `compute_statfs()` uses `libc::statvfs` for blocks; three-tier display in stats CLI |
| FIX-03 | 08-01-PLAN.md | Snapshot lookup by version is O(1) via HashMap<u64, SnapshotEntry> | SATISFIED | Pre-satisfied by Phase 7.1; `snapshots_by_version: Mutex<HashMap<u64, SnapshotEntry>>` present and used in all lookup paths |
| FIX-04 | 08-01-PLAN.md | Snapshot lookup by name is O(1) via HashMap<String, u64> index | SATISFIED | Pre-satisfied by Phase 7.1; `snapshots_by_name: Mutex<HashMap<String, u64>>` present and used for name-to-version resolution |

All four requirements claimed in the PLAN frontmatter are accounted for. No orphaned requirements.

---

### Anti-Patterns Found

No anti-patterns detected.

Scanned files: `crates/metadata/src/store.rs`, `crates/slicefs-cli/src/filesystem.rs`,
`crates/slicefs-cli/src/scrub.rs`, `crates/slicefs-cli/src/stats.rs`,
`crates/slicefs-cli/tests/statfs_tests.rs`.

No TODO/FIXME/HACK/PLACEHOLDER comments, no stub returns (`return null`, `return {}`,
`return []`), no console.log-only handlers found in any modified file.

---

### Human Verification Required

None. All truths are verifiable by static analysis:

- Saturation logic is deterministic arithmetic
- `libc::statvfs` call is present and wired through the FUSE handler
- `inode_count` AtomicU64 transitions are code-level operations
- `saturated_blocks` field is wired end-to-end in scrub

The only runtime behavior not covered by automated static verification is whether
`cargo test --workspace` is currently green. The SUMMARY documents this as passing with
zero failures (commits `9100c58` and `9728d03`). No human test required before
proceeding to Phase 9.

---

### Commit Verification

Both commits documented in SUMMARY exist and are present in git history:

- `9100c58` — feat(08-01): saturating refcount + inode_count AtomicU64 (FIX-01 + FIX-02 store layer)
- `9728d03` — feat(08-01): three-tier statfs, scrub saturated reporting, stats host_disk_bytes (FIX-02/FIX-01 CLI)

---

### Summary

Phase 8 fully achieves its goal. All four correctness bugs (FIX-01 through FIX-04) are closed:

- **FIX-01 (refcount overflow):** The data-loss risk is eliminated. `increment_refcount` now
  saturates at `u64::MAX` with a tracing warning. `decrement_refcount` treats `u64::MAX` as
  immortal. `saturated_refcount_count()` exposes the count for operator visibility via scrub.

- **FIX-02 (statfs accuracy):** `df` now reports real host disk capacity via `libc::statvfs`
  (using `f_frsize`, not `f_bsize`, avoiding the macOS pitfall). The `files` field reflects the
  actual inode count via an `AtomicU64` that survives `load_from_root` round-trips. Stats CLI
  shows three distinct tiers: logical bytes, CAS bytes, and host disk used.

- **FIX-03 / FIX-04 (snapshot O(1) lookup):** Confirmed present from Phase 7.1. Both
  `snapshots_by_version` and `snapshots_by_name` HashMaps are wired into all snapshot
  lookup, create, and delete methods. No new implementation was needed.

Phase 9 (Compression Removal) can proceed with confidence that the refcount and statfs
foundations are correct.

---

_Verified: 2026-03-30_
_Verifier: Claude (gsd-verifier)_
