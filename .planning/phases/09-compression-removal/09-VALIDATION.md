---
phase: 09
slug: compression-removal
status: draft
nyquist_compliant: false
wave_0_complete: false
created: 2026-03-30
---

# Phase 09 — Validation Strategy

> Per-phase validation contract for feedback sampling during execution.

---

## Test Infrastructure

| Property | Value |
|----------|-------|
| **Framework** | Rust `cargo test` |
| **Config file** | Cargo.toml (workspace) |
| **Quick run command** | `cargo test -p slicefs-cli --lib` |
| **Full suite command** | `cargo test --workspace` |
| **Estimated runtime** | ~30 seconds |

---

## Sampling Rate

- **After every task commit:** Run `cargo test -p slicefs-cli --lib`
- **After every plan wave:** Run `cargo test --workspace`
- **Before `/gsd:verify-work`:** Full suite must be green
- **Max feedback latency:** 30 seconds

---

## Per-Task Verification Map

| Task ID | Plan | Wave | Requirement | Test Type | Automated Command | File Exists | Status |
|---------|------|------|-------------|-----------|-------------------|-------------|--------|
| TBD | 01 | 1 | DECOMP-01 | unit | `cargo test -p slicefs-cli --lib` | TBD | ⬜ pending |
| TBD | 01 | 1 | DECOMP-02 | unit | `cargo test -p slicefs-cli --lib` | TBD | ⬜ pending |
| TBD | 01 | 1 | DECOMP-03 | unit | `cargo test -p slicefs-cli --lib` | TBD | ⬜ pending |
| TBD | 01 | 1 | DECOMP-04 | unit | `cargo test -p slicefs-cli --lib` | TBD | ⬜ pending |

*Status: ⬜ pending · ✅ green · ❌ red · ⚠️ flaky*

---

## Wave 0 Requirements

- [ ] `cargo test --workspace` passes before phase begins (baseline)

*Existing infrastructure covers requirements. New v3 tests replace deleted compression tests.*

---

## Manual-Only Verifications

*All phase behaviors have automated verification.*

---

## Validation Sign-Off

- [ ] All tasks have `<automated>` verify or Wave 0 dependencies
- [ ] Sampling continuity: no 3 consecutive tasks without automated verify
- [ ] Wave 0 covers all MISSING references
- [ ] No watch-mode flags
- [ ] Feedback latency < 30s
- [ ] `nyquist_compliant: true` set in frontmatter

**Approval:** pending
