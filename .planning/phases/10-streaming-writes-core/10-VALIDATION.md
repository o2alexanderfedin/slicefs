---
phase: 10
slug: streaming-writes-core
status: draft
nyquist_compliant: false
wave_0_complete: false
created: 2026-03-30
---

# Phase 10 — Validation Strategy

> Per-phase validation contract for feedback sampling during execution.

---

## Test Infrastructure

| Property | Value |
|----------|-------|
| **Framework** | Rust `cargo test` + integration tests |
| **Config file** | Cargo.toml (workspace) |
| **Quick run command** | `cargo test -p slicefs-fuse` |
| **Full suite command** | `cargo test --workspace` |
| **Estimated runtime** | ~60 seconds |

---

## Sampling Rate

- **After every task commit:** Run `cargo test -p slicefs-fuse`
- **After every plan wave:** Run `cargo test --workspace`
- **Before `/gsd:verify-work`:** Full suite must be green
- **Max feedback latency:** 60 seconds

---

## Per-Task Verification Map

| Task ID | Plan | Wave | Requirement | Test Type | Automated Command | File Exists | Status |
|---------|------|------|-------------|-----------|-------------------|-------------|--------|
| 10-01-01 | 01 | 1 | STRM-01 | unit | `cargo test -p slicefs-fuse open_file_state` | ❌ W0 | ⬜ pending |
| 10-02-01 | 02 | 2 | STRM-03 | integration | `cargo test -p slicefs-fuse flush_buffer` | ❌ W0 | ⬜ pending |
| 10-02-02 | 02 | 2 | STRM-04 | integration | `cargo test -p slicefs-fuse fsync_midstream` | ❌ W0 | ⬜ pending |
| 10-02-03 | 02 | 2 | STRM-05 | integration | `cargo test -p slicefs-fuse read_during_write` | ❌ W0 | ⬜ pending |
| 10-03-01 | 03 | 2 | STRM-03 | integration | `cargo test -p slicefs-fuse truncate_streaming` | ❌ W0 | ⬜ pending |

*Status: ⬜ pending · ✅ green · ❌ red · ⚠️ flaky*

---

## Wave 0 Requirements

- [ ] Test stubs for STRM-01 (streaming write memory bounds)
- [ ] Test stubs for STRM-03 (fsync mid-stream)
- [ ] Test stubs for STRM-04 (truncate on open handle)
- [ ] Test stubs for STRM-05 (read-during-write correctness)

*Existing infrastructure covers framework setup — only test stubs needed.*

---

## Manual-Only Verifications

| Behavior | Requirement | Why Manual | Test Instructions |
|----------|-------------|------------|-------------------|
| 10 GB file write under 512 MB RAM | STRM-01 | Resource-constrained test requires ulimit setup | `ulimit -v 524288 && dd if=/dev/urandom bs=1M count=10240 of=/mnt/slicefs/bigfile` |

---

## Validation Sign-Off

- [ ] All tasks have `<automated>` verify or Wave 0 dependencies
- [ ] Sampling continuity: no 3 consecutive tasks without automated verify
- [ ] Wave 0 covers all MISSING references
- [ ] No watch-mode flags
- [ ] Feedback latency < 60s
- [ ] `nyquist_compliant: true` set in frontmatter

**Approval:** pending
