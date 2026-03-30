---
phase: 11
slug: non-sequential-write-handling
status: draft
nyquist_compliant: false
wave_0_complete: false
created: 2026-03-30
---

# Phase 11 — Validation Strategy

> Per-phase validation contract for feedback sampling during execution.

---

## Test Infrastructure

| Property | Value |
|----------|-------|
| **Framework** | Rust `cargo test` + integration tests |
| **Config file** | Cargo.toml (workspace) |
| **Quick run command** | `cargo test -p slicefs-cli` |
| **Full suite command** | `cargo test --workspace` |
| **Estimated runtime** | ~60 seconds |

---

## Sampling Rate

- **After every task commit:** Run `cargo test -p slicefs-cli`
- **After every plan wave:** Run `cargo test --workspace`
- **Before `/gsd:verify-work`:** Full suite must be green
- **Max feedback latency:** 60 seconds

---

## Per-Task Verification Map

| Task ID | Plan | Wave | Requirement | Test Type | Automated Command | File Exists | Status |
|---------|------|------|-------------|-----------|-------------------|-------------|--------|
| 11-01-01 | 01 | 1 | STRM-02 | unit | `cargo test -p slicefs-cli write_mode` | ❌ W0 | ⬜ pending |
| 11-01-02 | 01 | 1 | STRM-02 | unit | `cargo test -p slicefs-cli test_write` | ✅ | ⬜ pending |
| 11-02-01 | 02 | 2 | STRM-02 | integration | `cargo test -p slicefs-cli --test streaming_tests non_sequential` | ❌ W0 | ⬜ pending |
| 11-02-02 | 02 | 2 | STRM-02 | integration | `cargo test -p slicefs-cli --test streaming_tests pwrite` | ❌ W0 | ⬜ pending |

*Status: ⬜ pending · ✅ green · ❌ red · ⚠️ flaky*

---

## Wave 0 Requirements

- [ ] Test stubs for STRM-02 (non-sequential write detection and fallback)
- [ ] Test stubs for pwrite correctness
- [ ] Test stubs for writeback_cache out-of-order delivery
- [ ] Un-ignore existing `#[ignore]` tests in write_path_tests.rs and posix_compliance_tests.rs

*Existing infrastructure covers framework setup — only test stubs needed.*

---

## Manual-Only Verifications

| Behavior | Requirement | Why Manual | Test Instructions |
|----------|-------------|------------|-------------------|
| vim edit-save-quit cycle | STRM-02 | Requires interactive editor | Mount slicefs, `vim /mnt/slicefs/file`, edit, `:wq`, compare with expected |
| sqlite database operations | STRM-02 | Complex multi-file I/O pattern | Mount slicefs, run sqlite3 CREATE/INSERT/SELECT, verify data integrity |

---

## Validation Sign-Off

- [ ] All tasks have `<automated>` verify or Wave 0 dependencies
- [ ] Sampling continuity: no 3 consecutive tasks without automated verify
- [ ] Wave 0 covers all MISSING references
- [ ] No watch-mode flags
- [ ] Feedback latency < 60s
- [ ] `nyquist_compliant: true` set in frontmatter

**Approval:** pending
