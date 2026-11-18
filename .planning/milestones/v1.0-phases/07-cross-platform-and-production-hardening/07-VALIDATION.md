---
phase: 7
slug: cross-platform-and-production-hardening
status: draft
nyquist_compliant: false
wave_0_complete: false
created: 2026-03-29
---

# Phase 7 — Validation Strategy

> Per-phase validation contract for feedback sampling during execution.

---

## Test Infrastructure

| Property | Value |
|----------|-------|
| **Framework** | Rust built-in `#[test]` via `cargo test` |
| **Config file** | none (workspace uses `cargo test` directly) |
| **Quick run command** | `cargo test -p slicefs-cli 2>&1 \| tail -20` |
| **Full suite command** | `cargo test --workspace 2>&1 \| tail -30` |
| **Estimated runtime** | ~30 seconds |

---

## Sampling Rate

- **After every task commit:** Run `cargo test -p slicefs-cli 2>&1 | tail -20`
- **After every plan wave:** Run `cargo test --workspace 2>&1 | tail -30`
- **Before `/gsd:verify-work`:** Full suite must be green
- **Max feedback latency:** 30 seconds

---

## Per-Task Verification Map

| Task ID | Plan | Wave | Requirement | Test Type | Automated Command | File Exists | Status |
|---------|------|------|-------------|-----------|-------------------|-------------|--------|
| 07-01-01 | 01 | 1 | PLAT-01 | smoke | `PKG_CONFIG_PATH=.pkgconfig cargo build -p slicefs-cli` | existing | pending |
| 07-01-02 | 01 | 1 | PLAT-01 | unit | `cargo test -p slicefs-cli --test posix_compliance_tests` | existing | pending |
| 07-01-03 | 01 | 1 | PLAT-01 | manual | mount + write + read on macOS | manual | pending |
| 07-02-01 | 02 | 1 | CLI-03 | unit | `cargo test -p slicefs-cli -- test_stats` | inline `#[cfg(test)]` in stats.rs | pending |
| 07-02-02 | 02 | 1 | CLI-04 | unit | `cargo test -p slicefs-cli -- test_scrub` | inline `#[cfg(test)]` in scrub.rs | pending |
| 07-02-03 | 02 | 1 | CLI-05 | unit | `cargo test -p slicefs-cli -- test_json_flag` | inline `#[cfg(test)]` in cli.rs | pending |
| 07-03-01 | 03 | 2 | PLAT-04 | smoke | GitHub Actions CI on push | Wave 0 | pending |
| 07-03-02 | 03 | 2 | PLAT-04 | integration | pjdfstest on Linux CI (>95% compliance gate) | Wave 0 | pending |

*Status: pending / green / red / flaky*

---

## Wave 0 Requirements

- [ ] `crates/slicefs-cli/build.rs` — macOS rpath automation (PLAT-01)
- [ ] `crates/slicefs-cli/src/stats.rs` — stats command with inline `#[cfg(test)]` module (CLI-03)
- [ ] `crates/slicefs-cli/src/scrub.rs` — scrub command with inline `#[cfg(test)]` module (CLI-04)
- [ ] `crates/slicefs-cli/src/cli.rs` — JSON flag tests in inline `#[cfg(test)]` module (CLI-05)
- [ ] `.github/workflows/ci.yml` — Linux (with pjdfstest) + macOS CI (PLAT-04)

---

## Manual-Only Verifications

| Behavior | Requirement | Why Manual | Test Instructions |
|----------|-------------|------------|-------------------|
| FUSE-T write path works | PLAT-01 | Requires live FUSE-T mount on macOS | Mount with direct_io, write file, read back, verify content matches |
| Benchmark baselines | N/A | Hardware-dependent | Run fio suite on target NVMe, document in BENCHMARKS.md |

---

## Validation Sign-Off

- [ ] All tasks have `<automated>` verify or Wave 0 dependencies
- [ ] Sampling continuity: no 3 consecutive tasks without automated verify
- [ ] Wave 0 covers all MISSING references
- [ ] No watch-mode flags
- [ ] Feedback latency < 30s
- [ ] `nyquist_compliant: true` set in frontmatter

**Approval:** pending
