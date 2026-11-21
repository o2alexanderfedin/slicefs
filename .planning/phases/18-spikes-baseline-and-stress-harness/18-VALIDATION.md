---
phase: "18"
slug: "spikes-baseline-and-stress-harness"
status: draft
nyquist_compliant: false
wave_0_complete: false
created: "2026-11-20"
---

# Phase 18 — Validation Strategy

> Per-phase validation contract for feedback sampling during execution.

---

## Test Infrastructure

| Property | Value |
|----------|-------|
| **Framework** | `cargo test`; integration tests in `crates/slicefs-cli/tests/` |
| **Config file** | none; `[features] lock-stats` and dev-dependency `hdrhistogram` in Cargo.toml files |
| **Quick run command** | `export PATH="$HOME/.cargo/bin:$PATH"; cargo test -p slicefs-cli --test stress_harness` |
| **Full suite command** | `export PATH="$HOME/.cargo/bin:$PATH"; cargo test --workspace && cargo check --workspace --all-targets` |
| **Estimated runtime** | quick ~10 s after build; full several minutes |

---

## Sampling Rate

- **After every task commit:** quick run command (and `cargo check -p slicefs-cli --features lock-stats` after a lock-site change)
- **After every plan wave:** full suite command, plus `cargo test -p slicefs-cli --features lock-stats --test stress_harness`
- **Before `/gsd-verify-work`:** full suite green; doc greps pass
- **Max feedback latency:** 60 seconds (quick run, warm build)

---

## Per-Task Verification Map

Filled in by the planner per task (`<automated>` + `<fails_when>`). Requirement map from RESEARCH.md:

| Requirement | Behavior | Test Type | Automated Command | File Exists | Status |
|-------------|----------|-----------|-------------------|-------------|--------|
| SPIKE-02 | Mixed ops, one file and many files, data equals model | integration | `cargo test -p slicefs-cli --test stress_harness short_` | ❌ W0 | ⬜ pending |
| SPIKE-02 | Model detects a planted divergence | integration | `cargo test -p slicefs-cli --test stress_harness model_detects_planted_bug` | ❌ W0 | ⬜ pending |
| SPIKE-02 | Histograms and lock hold/wait report (all seven labels) | integration | `cargo test -p slicefs-cli --features lock-stats --test lock_stats_report` (own test binary, so the process-global recorder is not shared) | ❌ W0 | ⬜ pending |
| SPIKE-02 | hdrhistogram not in the product build with the feature off | build | `cargo tree -p slicefs-cli -e normal \| grep -c hdrhistogram` prints 0 | ❌ W0 | ⬜ pending |
| SPIKE-02 | Product build unchanged with the feature off | build | `cargo check --workspace --all-targets` | ✅ | ⬜ pending |
| SPIKE-01 | S1-S7 result sections, each with `**Decides:** step N` | doc check | grep on `docs/research/multithreaded-request-handling.md` | ❌ W0 | ⬜ pending |
| SPIKE-03 | S1 probe builds | build | `cargo build --release -p slicefs-cli --example s1_probe` | ❌ W0 | ⬜ pending |

Per-task map (each `<automated>` has a `<fails_when>` in its PLAN.md):

| Task | Requirement | Automated command (short) |
|------|-------------|---------------------------|
| 18-01 T1 | SPIKE-02 | `cargo test -p slicefs-cli --test stress_harness short_one_file -- --nocapture` |
| 18-01 T2 | SPIKE-02 | same with `--features lock-stats` (row `lock site=store`); `cargo tree ... -e normal` gates |
| 18-02 T1, T2 | SPIKE-02 | `cargo test -p slicefs-cli --features lock-stats --test lock_stats_report`; `cargo test --workspace` |
| 18-03 T1-T3 | SPIKE-02 | `cargo test -p slicefs-cli --test stress_harness short_`; `model_detects_planted_bug --exact`; `-- --list --ignored` |
| 18-04 T1, T2 | SPIKE-03 | `cargo build --release -p slicefs-cli --example s1_probe`; grep `S1 RESULT` in `runs/s1-*.log` |
| 18-05 T1-T3 | SPIKE-01 | grep result lines in `runs/s6-smallfsync-linux.log`, `runs/s4-waiting-linux.log`, `runs/s2-bigwrite-linux.log` |
| 18-06 T1-T3 | SPIKE-01 | grep result lines in `runs/s3-*.log`, `runs/s5-*.log`, `runs/s6-inproc-macos.log`, `runs/s7-budget-linux.log`; suite still 3 passed, 6 ignored |
| 18-07 T1, T2 | SPIKE-01, SPIKE-03 | doc greps: 7 `### S<n> result`, 7 `**Decides:** step`, >= 5 `SUPERSEDED`, the macOS default sentence |

*Status: ⬜ pending · ✅ green · ❌ red · ⚠️ flaky*

---

## Wave 0 Requirements

- [ ] `crates/slicefs-cli/tests/stress_harness.rs`
- [ ] lock-stats recorder and feature wiring
- [ ] `hdrhistogram` dev-dependency
- [ ] `crates/slicefs-cli/examples/s1_probe.rs`, S1 script and workflow
- [ ] S4/S6 Docker scripts

---

## Manual-Only Verifications

| Behavior | Requirement | Why Manual | Test Instructions |
|----------|-------------|------------|-------------------|
| S2/S4/S6 numbers on Linux mount | SPIKE-01 | Needs Docker minutes; test-time budget | `scripts/docker-linux-test/run.sh` modes, 3 runs each |
| S1 on FSKit | SPIKE-03 | Needs a real FSKit mount; local macOS mounts not allowed | `gh workflow run s1-overlap.yml`, read the `S1 RESULT` line |

---

## Validation Sign-Off

- [ ] All tasks have `<automated>` verify or Wave 0 dependencies
- [ ] Sampling continuity: no 3 consecutive tasks without automated verify
- [ ] Wave 0 covers all MISSING references
- [ ] No watch-mode flags
- [ ] Feedback latency < 60s
- [ ] `nyquist_compliant: true` set in frontmatter

**Approval:** pending
