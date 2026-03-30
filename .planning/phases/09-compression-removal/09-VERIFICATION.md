---
phase: 09-compression-removal
verified: 2026-03-29T00:00:00Z
status: passed
score: 8/8 must-haves verified
re_verification: false
---

# Phase 9: Compression Removal Verification Report

**Phase Goal:** The write path pushes raw bytes directly into the Merkle tree with no compression header; clean break — no v1/v2 backward compatibility; remove compress_block/decompress_block and slicefs-compression dependency from write/read path.
**Verified:** 2026-03-29
**Status:** passed
**Re-verification:** No — initial verification

---

## Goal Achievement

### Observable Truths

Plan 01 must-haves:

| # | Truth | Status | Evidence |
|---|-------|--------|----------|
| 1 | Write path pushes raw bytes to CAS with no compression header | VERIFIED | `flush_buffer_to_cas` passes `&buf` directly to `State::push_all`; zero grep hits for `to_wire_bytes`, `compress_block`, or `AlgorithmId` in filesystem.rs |
| 2 | Read path returns stored bytes unchanged — no decompress call | VERIFIED | Zero grep hits for `from_wire_bytes`, `decompress_block`, or `store_version` in filesystem.rs; `file_storage_get` bytes used directly |
| 3 | SliceFsFilesystem has no compressor field and no store_version field | VERIFIED | Struct definition confirmed (lines 67-74): fields are `meta`, `io`, `open_files`, `next_fh`, `store_path`, `auto_snapshot` — no compressor, no store_version |
| 4 | CLI has no --compressor or --compressor-level flags | VERIFIED | `Cmd::Mount` in cli.rs has no `compressor` or `compressor_level` fields; `main.rs` `run_mount` call passes 8 params with no compressor args |
| 5 | Stats reports compressor as 'none (v3 raw)' | VERIFIED | `stats.rs` line 161: `let compressor = "none (v3 raw)".to_string();` |

Plan 02 must-haves:

| # | Truth | Status | Evidence |
|---|-------|--------|----------|
| 6 | All existing test suites pass with updated SliceFsFilesystem::new(meta, io, store_path) constructor | VERIFIED | `cargo test --workspace` — all test results `ok`, 0 failures across all crates |
| 7 | Compression tests are deleted — no tests for removed behavior | VERIFIED | `crates/slicefs-cli/tests/compression_tests.rs` does not exist; confirmed by `ls tests/` output |
| 8 | New v3 tests prove raw bytes round-trip, dedup works on raw content, no compression header present | VERIFIED | `v3_store_tests.rs` (144 lines, 3 tests) — all 3 pass: `test_write_raw_no_compression`, `test_read_returns_raw_bytes`, `test_raw_content_dedup` |

**Score:** 8/8 truths verified

---

### Required Artifacts

| Artifact | Expected | Status | Details |
|----------|----------|--------|---------|
| `crates/slicefs-cli/src/filesystem.rs` | No compressor/store_version fields; 3-param constructor; raw bytes to State::push_all | VERIFIED | Constructor signature: `pub fn new(meta, io, store_path)` — 3 params confirmed; `State::push_all(&buf)` at lines 345, 385, 466, 850, 1625, 1657 |
| `crates/slicefs-cli/src/mount.rs` | run_mount without compressor_name/compressor_level params | VERIFIED | `run_mount` has 8 params: `store_path, mountpoint, noatime, allow_other, _cache_size, wal_strategy, snapshot_ref, auto_snapshot` — no compression params |
| `crates/slicefs-cli/src/cli.rs` | Cmd::Mount without compressor/compressor_level fields | VERIFIED | `Cmd::Mount` variant confirmed; zero grep hits for compress in cli.rs |
| `crates/slicefs-cli/Cargo.toml` | No slicefs-compression dependency | VERIFIED | grep for `slicefs-compression` in Cargo.toml returns no match (exit 1) |
| `crates/slicefs-cli/tests/v3_store_tests.rs` | V3 store invariant tests: raw write, raw read, raw dedup; min 40 lines | VERIFIED | File is 144 lines with 3 substantive tests that directly use `file_storage_get` for low-level verification |
| `crates/slicefs-cli/tests/write_path_tests.rs` | Updated fresh_fs() with 3-param constructor | VERIFIED | `SliceFsFilesystem::new(meta, io, None)` confirmed at line 23 |
| `crates/slicefs-cli/tests/compression_tests.rs` | DELETED — must not exist | VERIFIED | File absent from `crates/slicefs-cli/tests/` directory listing |

---

### Key Link Verification

| From | To | Via | Status | Details |
|------|----|-----|--------|---------|
| `filesystem.rs` | `blockset::State::push_all` | direct `&buf` pass in `flush_buffer_to_cas` | WIRED | Pattern `State::push_all.*&buf` found at line 345 (and 5 other call sites) |
| `main.rs` | `mount.rs` | `run_mount` call without compressor args | WIRED | `run_mount` call at line 25 with 8 args; no compress-related args in destructure or call |
| `v3_store_tests.rs` | `filesystem.rs` | `SliceFsFilesystem::new(meta, io, None)` | WIRED | 3-param constructor call confirmed at line 23 |
| `v3_store_tests.rs` | `blockset::file_storage_get` | read-back verification of raw bytes | WIRED | `file_storage_get` imported and used at lines 9, 57 |

---

### Requirements Coverage

| Requirement | Source Plan | Description | Status | Evidence |
|-------------|------------|-------------|--------|----------|
| DECOMP-01 | 09-01, 09-02 | Write path pushes raw (uncompressed) bytes to Merkle tree — no compress_block call | SATISFIED | `State::push_all(&buf)` in `flush_buffer_to_cas`; `test_write_raw_no_compression` asserts stored bytes == raw input |
| DECOMP-02 | 09-01, 09-02 | Store format version bumped to v3 (raw blocks, no compression header) | SATISFIED | `store_version` field removed; struct doc comment states "v3 store format — no compression header"; test asserts `stored.len() == content.len()` (no header byte) |
| DECOMP-03 | 09-01, 09-02 | Read path v3 only — clean break, no v1/v2 backward compatibility (per RESEARCH.md clarification) | SATISFIED | `from_wire_bytes` and all decompress logic removed; `file_storage_get` bytes used directly; `test_read_returns_raw_bytes` passes |
| DECOMP-04 | 09-01, 09-02 | Digest224 identity is computed on raw content — cross-file dedup works | SATISFIED | Raw bytes flow into `State::push_all` unchanged; `test_raw_content_dedup` asserts `manifest1[0] == manifest2[0]` for identical content |

**Note on DECOMP-03 description mismatch:** REQUIREMENTS.md describes DECOMP-03 as "Read path handles all three store versions: v1 (legacy raw), v2 (compressed header), v3 (new raw)." The RESEARCH.md (the authoritative phase contract, per user constraints) explicitly redefines this as "CLEAN BREAK — No v1/v2 backward compatibility — no production stores exist." The implementation matches the RESEARCH.md definition. The REQUIREMENTS.md wording is a documentation artifact that should be updated to match the actual decision. This is noted but does not constitute a gap — the decision was intentional and recorded.

**Orphaned requirements check:** REQUIREMENTS.md traceability table maps DECOMP-01 through DECOMP-04 to Phase 9. All four are claimed by both plans and verified above. No orphaned requirements.

---

### Anti-Patterns Found

| File | Line | Pattern | Severity | Impact |
|------|------|---------|----------|--------|
| `crates/slicefs-cli/src/mount.rs` | 199 | `_cache_size: usize` unused parameter | Info | Intentional placeholder for future cache implementation; not a stub |
| Multiple source files | various | 14 compiler warnings (unused functions/variables) | Info | Pre-existing warnings unrelated to phase goal; none are stubs |

No blocker or warning-level anti-patterns found. The `_cache_size` prefix signals intentional deferral, not a stub.

---

### Human Verification Required

None. All phase behaviors have automated test coverage that passes.

---

### Gaps Summary

No gaps. All 8 must-haves are verified against the actual codebase:

- `SliceFsFilesystem::new` takes exactly 3 params (meta, io, store_path)
- All 7 compression reference types (`compress_block`, `decompress_block`, `to_wire_bytes`, `from_wire_bytes`, `store_version`, `compressor` field, `slicefs_compression` import) are absent from production source
- `slicefs-compression` is absent from `slicefs-cli/Cargo.toml` dependencies
- `cargo build -p slicefs-cli --lib` compiles cleanly
- All test files use the 3-param constructor; zero `NoneCompressor` references remain
- `compression_tests.rs` is deleted
- `v3_store_tests.rs` exists at 144 lines with 3 passing tests
- `cargo test --workspace` is fully green (0 failures across all crates)

The phase goal is achieved: raw bytes flow directly into the Merkle tree with no compression header, the slicefs-compression dependency is removed from the write/read path, and the clean break is validated by automated tests.

---

_Verified: 2026-03-29_
_Verifier: Claude (gsd-verifier)_
