---
phase: 4
slug: full-posix-write-path
status: draft
nyquist_compliant: false
wave_0_complete: false
created: 2026-03-28
---

# Phase 4 — Validation Strategy

> Per-phase validation contract for feedback sampling during execution.

---

## Test Infrastructure

| Property | Value |
|----------|-------|
| **Framework** | Rust built-in test harness (`cargo test`) |
| **Config file** | None — standard `#[test]` attributes |
| **Quick run command** | `cargo test -p slicefs-cli -p metadata` |
| **Full suite command** | `cargo test --workspace` |
| **Estimated runtime** | ~15 seconds |

---

## Sampling Rate

- **After every task commit:** Run `cargo test -p slicefs-cli -p metadata`
- **After every plan wave:** Run `cargo test --workspace`
- **Before `/gsd:verify-work`:** Full suite must be green
- **Max feedback latency:** 20 seconds

---

## Per-Task Verification Map

| Task ID | Plan | Wave | Requirement | Test Type | Automated Command | File Exists | Status |
|---------|------|------|-------------|-----------|-------------------|-------------|--------|
| 04-01-01 | 01 | 1 | POSIX-01 | unit | `cargo test -p slicefs-cli write_buffer` | ❌ W0 | ⬜ pending |
| 04-01-02 | 01 | 1 | POSIX-01 | unit | `cargo test -p slicefs-cli flush_pipeline` | ❌ W0 | ⬜ pending |
| 04-02-01 | 02 | 2 | POSIX-02 | unit | `cargo test -p slicefs-cli mkdir_rmdir` | ❌ W0 | ⬜ pending |
| 04-02-02 | 02 | 2 | POSIX-03 | unit | `cargo test -p slicefs-cli rename` | ❌ W0 | ⬜ pending |
| 04-02-03 | 02 | 2 | POSIX-04 | unit | `cargo test -p slicefs-cli symlink` | ❌ W0 | ⬜ pending |
| 04-02-04 | 02 | 2 | POSIX-05 | unit | `cargo test -p slicefs-cli hard_link` | ❌ W0 | ⬜ pending |
| 04-03-01 | 03 | 3 | POSIX-09 | unit | `cargo test -p slicefs-cli truncate` | ❌ W0 | ⬜ pending |
| 04-03-02 | 03 | 3 | CAS-04 | unit | `cargo test -p metadata refcount` | ❌ W0 | ⬜ pending |
| 04-03-03 | 03 | 3 | CAS-06 | unit | `cargo test -p slicefs-cli statfs` | ❌ W0 | ⬜ pending |
| 04-04-01 | 04 | 4 | POSIX-14 | integration | Custom Rust POSIX test suite | ❌ W0 | ⬜ pending |

*Status: ⬜ pending · ✅ green · ❌ red · ⚠️ flaky*

---

## Wave 0 Requirements

- [ ] Write buffer and flush pipeline in slicefs-cli
- [ ] mkdir/rmdir/rename/symlink/link/unlink callbacks
- [ ] Truncate and setattr size handling
- [ ] Refcount tracking in metadata crate
- [ ] statfs with logical/physical reporting
- [ ] Custom POSIX integration test module

---

## Manual-Only Verifications

| Behavior | Requirement | Why Manual | Test Instructions |
|----------|-------------|------------|-------------------|
| pjdfstest >95% on Linux | POSIX-14 | Requires Linux FUSE + pjdfstest | Build on Linux, mount, run `prove -r pjdfstest/tests/` |

---

## Validation Sign-Off

- [ ] All tasks have `<automated>` verify or Wave 0 dependencies
- [ ] Sampling continuity: no 3 consecutive tasks without automated verify
- [ ] Wave 0 covers all MISSING references
- [ ] No watch-mode flags
- [ ] Feedback latency < 20s
- [ ] `nyquist_compliant: true` set in frontmatter

**Approval:** pending
