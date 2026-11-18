---
phase: 3
slug: read-only-fuse
status: draft
nyquist_compliant: false
wave_0_complete: false
created: 2026-03-28
---

# Phase 3 — Validation Strategy

> Per-phase validation contract for feedback sampling during execution.

---

## Test Infrastructure

| Property | Value |
|----------|-------|
| **Framework** | Rust built-in `#[test]` / `cargo test` |
| **Config file** | None (standard cargo test) |
| **Quick run command** | `cargo test -p slicefs-cli` |
| **Full suite command** | `cargo test --workspace` |
| **Estimated runtime** | ~10 seconds |

---

## Sampling Rate

- **After every task commit:** Run `cargo test -p slicefs-cli`
- **After every plan wave:** Run `cargo test --workspace`
- **Before `/gsd:verify-work`:** Full suite must be green
- **Max feedback latency:** 15 seconds

---

## Per-Task Verification Map

| Task ID | Plan | Wave | Requirement | Test Type | Automated Command | File Exists | Status |
|---------|------|------|-------------|-----------|-------------------|-------------|--------|
| 03-01-01 | 01 | 1 | CLI-01, CLI-02, CLI-06 | unit | `cargo test -p slicefs-cli test_cli` | ❌ W0 | ⬜ pending |
| 03-01-02 | 01 | 1 | POSIX-13 | unit | `cargo test -p slicefs-cli test_write_ops_return_erofs` | ❌ W0 | ⬜ pending |
| 03-02-01 | 02 | 2 | POSIX-15 | unit | `cargo test -p slicefs-cli test_getattr` | ❌ W0 | ⬜ pending |
| 03-02-02 | 02 | 2 | POSIX-15 | unit | `cargo test -p slicefs-cli test_readdir` | ❌ W0 | ⬜ pending |
| 03-02-03 | 02 | 2 | POSIX-15 | unit | `cargo test -p slicefs-cli test_read_at_offset` | ❌ W0 | ⬜ pending |
| 03-03-01 | 03 | 3 | META-02 | unit | `cargo test -p slicefs-cli test_destroy_commits` | ❌ W0 | ⬜ pending |
| 03-03-02 | 03 | 3 | PLAT-02 | integration | Manual: mount + ls + cat + unmount | ❌ W0 | ⬜ pending |

*Status: ⬜ pending · ✅ green · ❌ red · ⚠️ flaky*

---

## Wave 0 Requirements

- [ ] `crates/slicefs-cli/` — entire crate does not exist yet
- [ ] `crates/slicefs-cli/Cargo.toml` — bin crate with fuser, clap, libc, metadata, blockset deps
- [ ] `crates/slicefs-cli/src/main.rs` — entry point with clap derive
- [ ] `crates/slicefs-cli/src/filesystem.rs` — SliceFsFilesystem with unit tests
- [ ] Add `crates/slicefs-cli` to workspace members in root Cargo.toml
- [ ] Add `clap` to workspace `[workspace.dependencies]` with `features = ["derive"]`

---

## Manual-Only Verifications

| Behavior | Requirement | Why Manual | Test Instructions |
|----------|-------------|------------|-------------------|
| Real FUSE mount on Linux | PLAT-02 | Requires FUSE kernel device and privileges | `slicefs seed /tmp/store /some/dir && slicefs mount /tmp/mnt --store /tmp/store` then verify `ls`, `cat`, `stat` |

---

## Validation Sign-Off

- [ ] All tasks have `<automated>` verify or Wave 0 dependencies
- [ ] Sampling continuity: no 3 consecutive tasks without automated verify
- [ ] Wave 0 covers all MISSING references
- [ ] No watch-mode flags
- [ ] Feedback latency < 15s
- [ ] `nyquist_compliant: true` set in frontmatter

**Approval:** pending
