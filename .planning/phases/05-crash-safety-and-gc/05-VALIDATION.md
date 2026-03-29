---
phase: 5
slug: crash-safety-and-gc
status: draft
nyquist_compliant: false
wave_0_complete: false
created: 2026-03-28
---

# Phase 5 — Validation Strategy

> Per-phase validation contract for feedback sampling during execution.

---

## Test Infrastructure

| Property | Value |
|----------|-------|
| **Framework** | Rust built-in `#[test]` via `cargo test` |
| **Config file** | None — standard Cargo.toml dev-dependencies |
| **Quick run command** | `cargo test --workspace -q` |
| **Full suite command** | `cargo test --workspace` |
| **Estimated runtime** | ~20 seconds |

---

## Sampling Rate

- **After every task commit:** Run `cargo test --workspace -q`
- **After every plan wave:** Run `cargo test --workspace`
- **Before `/gsd:verify-work`:** Full suite must be green
- **Max feedback latency:** 25 seconds

---

## Per-Task Verification Map

| Task ID | Plan | Wave | Requirement | Test Type | Automated Command | File Exists | Status |
|---------|------|------|-------------|-----------|-------------------|-------------|--------|
| 05-01-01 | 01 | 1 | META-01 | unit | `cargo test -p metadata wal` | ❌ W0 | ⬜ pending |
| 05-01-02 | 01 | 1 | META-01 | unit | `cargo test -p metadata segment` | ❌ W0 | ⬜ pending |
| 05-02-01 | 02 | 2 | META-01, POSIX-11 | unit | `cargo test -p slicefs-cli fsync` | ❌ W0 | ⬜ pending |
| 05-02-02 | 02 | 2 | META-01 | integration | `cargo test -p metadata wal_replay` | ❌ W0 | ⬜ pending |
| 05-03-01 | 03 | 3 | GC-01, GC-02 | unit | `cargo test -p metadata gc` | ❌ W0 | ⬜ pending |
| 05-03-02 | 03 | 3 | GC-03 | unit | `cargo test -p metadata gc_snapshot` | ❌ W0 | ⬜ pending |

*Status: ⬜ pending · ✅ green · ❌ red · ⚠️ flaky*

---

## Wave 0 Requirements

- [ ] `crates/metadata/src/wal/` — WAL module with strategy trait
- [ ] `crates/metadata/src/segment/` — Segment storage module
- [ ] `crates/metadata/src/gc/` — GC mark-and-sweep module
- [ ] `crates/slicefs-cli/src/gc.rs` — offline GC command
- [ ] Test files for WAL, segment, and GC modules

---

## Manual-Only Verifications

| Behavior | Requirement | Why Manual | Test Instructions |
|----------|-------------|------------|-------------------|
| kill -9 during write + remount produces consistent FS | META-01 | Requires real FUSE mount + signal | Mount, write file, kill -9, remount, verify consistency |

---

## Validation Sign-Off

- [ ] All tasks have `<automated>` verify or Wave 0 dependencies
- [ ] Sampling continuity: no 3 consecutive tasks without automated verify
- [ ] Wave 0 covers all MISSING references
- [ ] No watch-mode flags
- [ ] Feedback latency < 25s
- [ ] `nyquist_compliant: true` set in frontmatter

**Approval:** pending
