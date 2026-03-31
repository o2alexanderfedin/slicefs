---
phase: 12
slug: add-smb-fskit-backend-support-for-fuse-t
status: draft
nyquist_compliant: false
wave_0_complete: false
created: 2026-03-31
---

# Phase 12 — Validation Strategy

> Per-phase validation contract for feedback sampling during execution.

---

## Test Infrastructure

| Property | Value |
|----------|-------|
| **Framework** | Rust's built-in `#[test]` + cargo test |
| **Config file** | Cargo.toml (workspace) |
| **Quick run command** | `cargo test --package slicefs-cli --lib` |
| **Full suite command** | `cargo test --workspace` |
| **Estimated runtime** | ~8 seconds |

---

## Sampling Rate

- **After every task commit:** Run `cargo test --package slicefs-cli --lib`
- **After every plan wave:** Run `cargo test --workspace`
- **Before `/gsd:verify-work`:** Full suite must be green
- **Max feedback latency:** 10 seconds

---

## Per-Task Verification Map

| Task ID | Plan | Wave | Requirement | Test Type | Automated Command | File Exists | Status |
|---------|------|------|-------------|-----------|-------------------|-------------|--------|
| 12-01-01 | 01 | 1 | Backend detection | unit | `cargo test --package slicefs-cli backend` | ❌ W0 | ⬜ pending |
| 12-01-02 | 01 | 1 | Version parsing | unit | `cargo test --package slicefs-cli version` | ❌ W0 | ⬜ pending |
| 12-01-03 | 01 | 1 | FSKit availability | unit | `cargo test --package slicefs-cli fskit` | ❌ W0 | ⬜ pending |
| 12-02-01 | 02 | 2 | CLI --backend flag | unit | `cargo test --package slicefs-cli backend_flag` | ❌ W0 | ⬜ pending |
| 12-02-02 | 02 | 2 | NFS blocked by default | unit | `cargo test --package slicefs-cli nfs_blocked` | ❌ W0 | ⬜ pending |
| 12-03-01 | 03 | 3 | Signal handler | unit | `cargo test --package slicefs-cli signal` | ❌ W0 | ⬜ pending |
| 12-03-02 | 03 | 3 | Enhanced unmount | unit | `cargo test --package slicefs-cli unmount` | ❌ W0 | ⬜ pending |

*Status: ⬜ pending · ✅ green · ❌ red · ⚠️ flaky*

---

## Wave 0 Requirements

- [ ] `crates/slicefs-cli/src/backend.rs` — new module with unit tests for detection logic
- [ ] Test helpers for injectable paths (avoid real FUSE-T dependency in tests)

*Existing infrastructure covers test framework — no new framework needed.*

---

## Manual-Only Verifications

| Behavior | Requirement | Why Manual | Test Instructions |
|----------|-------------|------------|-------------------|
| SMB mount works | Backend switch | Requires live FUSE-T + SMB | Mount with `--backend=smb`, run `echo test > mount/f.txt && cat mount/f.txt` |
| FSKit mount works | Backend switch | Requires macOS 26 + fuse-t.app | Mount with `--backend=fskit`, same write test |
| Stuck mount cleanup | Enhanced unmount | Requires killing mount mid-operation | Kill slicefs, verify `slicefs unmount` cleans up |
| Fallback confirmation | Interactive fallback | Requires TTY | Mount without SMB available, verify prompt appears |

---

## Validation Sign-Off

- [ ] All tasks have `<automated>` verify or Wave 0 dependencies
- [ ] Sampling continuity: no 3 consecutive tasks without automated verify
- [ ] Wave 0 covers all MISSING references
- [ ] No watch-mode flags
- [ ] Feedback latency < 10s
- [ ] `nyquist_compliant: true` set in frontmatter

**Approval:** pending
