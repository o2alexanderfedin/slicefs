---
phase: "23"
slug: "hashes-and-rehash"
# status lifecycle: draft (seeded by plan-phase) → validated (set by validate-phase §6)
status: draft
nyquist_compliant: false
wave_0_complete: false
created: "2026-11-26"
---

# Phase 23 — Validation Strategy

> Per-phase validation contract for feedback sampling during execution. Source: 23-RESEARCH.md "Validation Architecture".

---

## Test Infrastructure

| Property | Value |
|----------|-------|
| **Framework** | Rust `cargo test` (libtest); feature `test-hasher` for `TestKeyedBlake3` |
| **Config file** | none |
| **Quick run command** | `timeout 600 cargo test -p chunk-store --lib hash::` (or the targeted command of the task) |
| **Full suite command** | `timeout 3600 cargo test --workspace -- --skip a_panic_on_another_thread_does_not_abort --skip a_reader_thread_panic_aborts_and_the_remount_replays --skip a_worker_panic_aborts_and_the_remount_replays --skip a_full_queue_that_stays_full_aborts` |
| **Estimated runtime** | targeted < 30 s; full suite minutes |

---

## Sampling Rate

- **After every task commit:** the targeted command of the task (under 30 s)
- **After every plan wave:** `timeout 900 cargo test -p chunk-store -p metadata -p slicefs-cli` and `scripts/safety/unsafe-count.sh`
- **Before `/gsd-verify-work`:** full suite green, plus `scripts/safety/unsafe-count.sh`, `scripts/safety/clippy.sh`, `KANI_HARNESS=name_bytes_total scripts/safety/kani.sh`
- **Max feedback latency:** 30 seconds per task

---

## Per-Task Verification Map

| Req | Behavior | Test Type | Automated Command | File Exists | Status |
|-----|----------|-----------|-------------------|-------------|--------|
| HASH-01 | `Sha256` golden vectors, tag 0x12, name `sha256`, in `hashers()` | unit | `cargo test -p chunk-store --lib hash::` | ❌ W0 | ⬜ pending |
| HASH-01 | `--hash sha256` on seed and mount; flag before any SHA-256 object | integration | `cargo test -p slicefs-cli --test hash_change_tests` | partly | ⬜ pending |
| HASH-01 | header pairing rule for `hash sha256` and `hash-sha256` flag | unit | `cargo test -p slicefs-cli --lib header::` | partly | ⬜ pending |
| HASH-02 | rehashed file key equals a fresh write (default and large profile, sparse, spliced) | integration | `cargo test -p slicefs-cli --test rehash_tests` | ❌ W0 | ⬜ pending |
| HASH-02 | engine on `MemIo`: nodes, refcount rows, same tree | unit | `cargo test -p metadata --lib rehash::` | ❌ W0 | ⬜ pending |
| HASH-03 | refusals: lock exclusive/shared, log records, `mount.lock`, unknown target | integration | `cargo test -p slicefs-cli --test rehash_tests refuse` | ❌ W0 | ⬜ pending |
| HASH-04 | switch segment crash states; last record per version; GC frees old objects | unit + integration | `cargo test -p metadata --lib segment::` and `--test rehash_tests crash` | ❌ W0 | ⬜ pending |
| HASH-04 | corrupted old object stops before the switch | integration | `cargo test -p slicefs-cli --test rehash_tests verify` | ❌ W0 | ⬜ pending |
| HASH-05 | report fields; `--dry-run` changes nothing; map cache bounded | integration + unit | `cargo test -p slicefs-cli --test rehash_tests dry_run`; `cargo test -p metadata --lib rehash::map` | ❌ W0 | ⬜ pending |
| HASH-06 | round trips both ways, mixed store ends with one tag, files and snapshots equal, scrub passes | integration | `cargo test -p slicefs-cli --test rehash_tests round_trip` | ❌ W0 | ⬜ pending |

*Status: ⬜ pending · ✅ green · ❌ red · ⚠️ flaky*

---

## Wave 0 Requirements

- [ ] `crates/chunk-store/src/hash.rs` tests (golden vectors, lookup, ignored speed test)
- [ ] `crates/slicefs-cli/src/header.rs` tests (pairing rule, flag kept)
- [ ] `crates/metadata/src/segment/` tests (last record per version, switch segment, `.seg.tmp` ignored)
- [ ] `crates/metadata/src/rehash/` tests (engine, map)
- [ ] `crates/slicefs-cli/tests/rehash_tests.rs`

---

## Manual-Only Verifications

| Behavior | Requirement | Why Manual | Test Instructions |
|----------|-------------|------------|-------------------|
| Hardware SHA-256 path runs | HASH-01 | the dev CPU has no SHA extensions; `sha2` does not expose the backend | on a machine with SHA-NI or ARMv8 SHA2: `cargo test --release -p chunk-store -- --ignored sha256_speed`; over 1,000 MB/s means a hardware path |

---

## Validation Sign-Off

- [ ] All tasks have `<automated>` verify or Wave 0 dependencies
- [ ] Sampling continuity: no 3 consecutive tasks without automated verify
- [ ] Wave 0 covers all MISSING references
- [ ] No watch-mode flags
- [ ] Feedback latency < 30s
- [ ] `nyquist_compliant: true` set in frontmatter

**Approval:** pending
