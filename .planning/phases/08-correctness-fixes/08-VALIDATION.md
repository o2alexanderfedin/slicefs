---
phase: 08
slug: correctness-fixes
status: draft
nyquist_compliant: false
wave_0_complete: false
created: 2026-03-30
---

# Phase 08 — Validation Strategy

> Per-phase validation contract for feedback sampling during execution.

---

## Test Infrastructure

| Property | Value |
|----------|-------|
| **Framework** | Rust `cargo test` + integration tests |
| **Config file** | Cargo.toml (workspace) |
| **Quick run command** | `cargo test -p metadata -p slicefs-cli --lib` |
| **Full suite command** | `cargo test --workspace` |
| **Estimated runtime** | ~30 seconds |

---

## Sampling Rate

- **After every task commit:** Run `cargo test -p metadata -p slicefs-cli --lib`
- **After every plan wave:** Run `cargo test --workspace`
- **Before `/gsd:verify-work`:** Full suite must be green
- **Max feedback latency:** 30 seconds

---

## Per-Task Verification Map

| Task ID | Plan | Wave | Requirement | Test Type | Automated Command | File Exists | Status |
|---------|------|------|-------------|-----------|-------------------|-------------|--------|
| TBD | 01 | 1 | FIX-01 | unit | `cargo test -p metadata --lib refcount` | TBD | ⬜ pending |
| TBD | 01 | 1 | FIX-02 | unit+integration | `cargo test -p slicefs-cli statfs` | TBD | ⬜ pending |
| TBD | 01 | 1 | FIX-03 | verification | `cargo test -p metadata --lib snapshot` | ✅ | ⬜ pending |
| TBD | 01 | 1 | FIX-04 | verification | `cargo test -p metadata --lib snapshot` | ✅ | ⬜ pending |

*Status: ⬜ pending · ✅ green · ❌ red · ⚠️ flaky*

---

## Wave 0 Requirements

- [ ] Existing test infrastructure covers all phase requirements
- [ ] `cargo test --workspace` passes before phase begins (baseline)

*Existing infrastructure covers most requirements. New tests added per-task via TDD.*

---

## Manual-Only Verifications

| Behavior | Requirement | Why Manual | Test Instructions |
|----------|-------------|------------|-------------------|
| `df` shows real inode count on mounted volume | FIX-02 | Requires real FUSE mount | Mount store, run `df`, verify files != 1000000 |

---

## Validation Sign-Off

- [ ] All tasks have `<automated>` verify or Wave 0 dependencies
- [ ] Sampling continuity: no 3 consecutive tasks without automated verify
- [ ] Wave 0 covers all MISSING references
- [ ] No watch-mode flags
- [ ] Feedback latency < 30s
- [ ] `nyquist_compliant: true` set in frontmatter

**Approval:** pending
