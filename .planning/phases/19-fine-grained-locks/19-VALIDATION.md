---
phase: "19"
slug: "fine-grained-locks"
status: draft
nyquist_compliant: false
wave_0_complete: false
created: "2026-11-21"
---

# Phase 19 — Validation Strategy

> Per-phase validation contract for feedback sampling during execution.

---

## Test Infrastructure

| Property | Value |
|----------|-------|
| **Framework** | `cargo test`; integration tests in `crates/*/tests/`; features `lock-stats`, `deadlock-detect` |
| **Config file** | none; features in crate `Cargo.toml` files |
| **Quick run command** | `export PATH="$HOME/.cargo/bin:$PATH"; cargo test -p slicefs-cli --test stress_harness` |
| **Full suite command** | `export PATH="$HOME/.cargo/bin:$PATH"; cargo test --workspace && cargo check --workspace --all-targets && cargo check --workspace --all-targets --features slicefs-cli/lock-stats && cargo check -p slicefs-cli --features deadlock-detect` |
| **Estimated runtime** | quick ~5 s after build; full several minutes |

---

## Sampling Rate

- **After every task commit:** the crate-local quick test of the touched area (`cargo test -p chunk-store --lib sync::`, `cargo test -p metadata --lib`, or the quick run command), plus `cargo check -p slicefs-cli --features lock-stats` after a lock-site change
- **After every plan:** full suite command, then `cargo test -p slicefs-cli --features lock-stats --test lock_stats_report`
- **Before `/gsd-verify-work`:** full suite green, plus `cargo test -p slicefs-cli --features deadlock-detect --test stress_harness serialize_off_target`
- **Max feedback latency:** 60 seconds (warm build)

---

## Per-Task Verification Map

Filled in by the planner (`<automated>` + `<fails_when>` per task). Requirement map: see `19-RESEARCH.md` "Validation Architecture" (LOCK-01, LOCK-03, LOCK-04, LOCK-05 rows).

*Status: ⬜ pending · ✅ green · ❌ red · ⚠️ flaky*

---

## Wave 0 Requirements

- [ ] `crates/slicefs-cli/tests/common/mod.rs` (shared `dump`, `reload_and_replay`)
- [ ] `crates/slicefs-cli/tests/lock_free_io.rs`
- [ ] `crates/slicefs-cli/tests/multi_handle_posix.rs`
- [ ] `crates/slicefs-cli/tests/crash_replay_threads.rs`
- [ ] slow-`PackIo` test wrapper; `sync.rs` rank unit tests; `Overlay` generation property test
- [ ] `lock_stats_report.rs` label list update

---

## Manual-Only Verifications

| Behavior | Requirement | Why Manual | Test Instructions |
|----------|-------------|------------|-------------------|
| Issue #21 body matches the log-order rule | LOCK-04 | GitHub text | `gh issue view 21 --json body -q .body`, check the rule text |

---

## Validation Sign-Off

- [ ] All tasks have `<automated>` verify or Wave 0 dependencies
- [ ] Sampling continuity: no 3 consecutive tasks without automated verify
- [ ] Wave 0 covers all MISSING references
- [ ] No watch-mode flags
- [ ] Feedback latency < 60s
- [ ] `nyquist_compliant: true` set in frontmatter

**Approval:** pending
