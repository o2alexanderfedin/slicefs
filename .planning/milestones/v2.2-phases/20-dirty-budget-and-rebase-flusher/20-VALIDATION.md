---
phase: "20"
slug: "dirty-budget-and-rebase-flusher"
status: draft
nyquist_compliant: true
wave_0_complete: false
created: "2026-11-23"
---

# Phase 20 — Validation Strategy

> Per-phase validation contract for feedback sampling during execution.

---

## Test Infrastructure

| Property | Value |
|----------|-------|
| **Framework** | `cargo test`; integration tests in `crates/slicefs-cli/tests/` (one binary per file); features `lock-stats`, `deadlock-detect` |
| **Config file** | none; features in `crates/slicefs-cli/Cargo.toml` |
| **Quick run command** | `export PATH="$HOME/.cargo/bin:$PATH"; cargo test -p slicefs-cli --test flusher_tests --test no_take_out` |
| **Full suite command** | `export PATH="$HOME/.cargo/bin:$PATH"; cargo test --workspace && cargo check --workspace --all-targets && cargo check --workspace --all-targets --features slicefs-cli/lock-stats && cargo check -p slicefs-cli --all-targets --features deadlock-detect && cargo test -p slicefs-cli --features deadlock-detect --test stress_harness serialize_off_target && cargo test -p slicefs-cli --features deadlock-detect --test flusher_tests` |
| **Docker runs (long, not in CI)** | tar stream of the committed tree into `slicefs-fusetest`, volume `slicefs-target-p20`: S7 release runs (plans 01 and 07) and the smoke mount (plans 03 and 08). Plan 01 builds the image once and records `IMAGE_ID` and `RUSTC` in `runs/s7-wave0-NOTES.md`; later plans check the ID and do not rebuild |
| **Estimated runtime** | quick: a few seconds per test after a warm build; full: 9 to 20 minutes at load 15-40; each S7 Docker set: up to 90 minutes |

---

## Sampling Rate

- **After every task commit:** the quick run command, plus the test binaries named in the task's `<verify>` (for example `dirty_budget_tests`, `rebase_step_tests`, `lock_free_io`); `stress_harness` must stay under 10 s warm
- **After every plan:** the full suite command (each plan commits only after `cargo test --workspace` exits 0)
- **Before `/gsd-verify-work`:** the full suite green, the plan 20-07 P1 runs under CAP_MIB, and the plan 20-08 smoke mount passed
- **Max feedback latency:** 60 seconds for the quick command (warm build); the Docker measurement tasks (20-01-03, 20-07-03) and the smoke mounts are long by nature and run in the background with a `timeout`

---

## Per-Task Verification Map

| Task ID | Plan | Wave | Requirement | Threat Ref | Secure Behavior | Test Type | Automated Command | File Exists | Status |
|---------|------|------|-------------|------------|-----------------|-----------|-------------------|-------------|--------|
| 20-01-01 | 01 | 1 | LOCK-06 | T-20-01 | S7 prints VmHWM; serialize can be off | measurement knob | `S7_FILE_MIB=4 S7_WRITE_MIB=2 S7_COMMIT_SECS=1 S7_SERIALIZE=0 cargo test -p slicefs-cli --test stress_harness -- --ignored --exact --nocapture s7_budget_baseline` | ✅ stress_harness.rs | ⬜ pending |
| 20-01-02 | 01 | 1 | LOCK-02 | T-20-03 | No cell removal outside 4 functions; budget step own file only | source test (red on the old name; guard mutation: a fifth site) | `cargo test -p slicefs-cli --test no_take_out` | ❌ W0 | ⬜ pending |
| 20-01-03 | 01 | 1 | LOCK-06 | T-20-01, T-20-02 | Cap fixed by the stated rule from P_old and P0 only | Docker measurement + recompute | awk over `runs/s7-wave0.log` gives the CAP_MIB in `stress_harness.rs` | ❌ W0 | ⬜ pending |
| 20-02-01 | 02 | 2 | LOCK-06 | T-20-04, T-20-05, T-20-07 | Writer over budget waits with no lock; flusher steps one file per gate hold; pass cannot spin | integration (red on stubs) | `cargo test -p slicefs-cli --test flusher_tests` | ❌ W0 | ⬜ pending |
| 20-02-02 | 02 | 2 | LOCK-06, LOCK-02 | T-20-04 | After the wait limit the writer pays; kick at half the budget; no cell removal in flusher.rs | integration + source test | `cargo test -p slicefs-cli --test flusher_tests --test no_take_out` | ❌ W0 | ⬜ pending |
| 20-03-01 | 03 | 3 | LOCK-06 | T-20-09 | Stop and a flusher panic wake every throttled writer; no throttle or stop with a lock held; hooks show a queued commit and a stop request | integration (red on the 20-02 tree) | `cargo test -p slicefs-cli --test flusher_tests` | ✅ after 20-02 | ⬜ pending |
| 20-03-02 | 03 | 3 | LOCK-06 | T-20-08, T-20-10, T-20-11 | Flusher with the mount; throttle before the gate in FUSE `write`; shutdown order with a parked step and a queued commit; daemon exits after unmount | unit + integration + Docker smoke | `cargo test -p slicefs-cli --lib flusher:: && cargo test -p slicefs-cli --test flusher_tests`; grep of `runs/smoke-20-03.log` | ❌ W0 (unit tests, smoke log) | ⬜ pending |
| 20-04-01 | 04 | 4 | LOCK-06 | T-20-12, T-20-14 | All writers throttled while a commit waits ends (flusher paused; flusher in a step) | integration + 60 s watchdog (guard mutations) | `cargo test -p slicefs-cli --test flusher_tests all_writers_throttled_while_a_commit_waits`; also `--features deadlock-detect` | ✅ after 20-02 | ⬜ pending |
| 20-04-02 | 04 | 4 | LOCK-06 | T-20-13 | A flusher pass never waits for another thread's step | integration (guard mutation) | `cargo test -p slicefs-cli --test flusher_tests the_flusher_skips_a_cell_with_a_step_in_flight` | ✅ after 20-02 | ⬜ pending |
| 20-05-01 | 05 | 5 | LOCK-06 | T-20-16, T-20-17 | Idle and old dirty data is stored | integration on a test clock (red on stubs) | `cargo test -p slicefs-cli --test flusher_tests` | ✅ after 20-02 | ⬜ pending |
| 20-05-02 | 05 | 5 | LOCK-06 | T-20-15 | Hot rule below the budget; largest quiet first; per-file limit trigger | integration on a test clock (red on the Task 1 tree; guard mutation) | `cargo test -p slicefs-cli --test flusher_tests` | ✅ after 20-02 | ⬜ pending |
| 20-06-01 | 06 | 6 | LOCK-06 | T-20-18, T-20-19 | Byte options take bytes or K/M/G (1024-based), refuse values below 4 MiB after scaling, bad suffixes and overflow; limit above budget refused before any side effect | unit | `cargo test -p slicefs-cli --lib -- cli:: mount:: content:: filesystem::` | ✅ (cli.rs, mount.rs tests) | ⬜ pending |
| 20-06-02 | 06 | 6 | LOCK-06 | T-20-18 | Time options refuse 0; the flusher uses the configured times | unit + integration | `cargo test -p slicefs-cli --lib -- cli:: && cargo test -p slicefs-cli --test flusher_tests a_configured_idle_time_is_used` | ✅ | ⬜ pending |
| 20-07-01 | 07 | 7 | LOCK-06 | T-20-21 | Shared total bounded with 4 parallel writers | integration (guard mutation) | `cargo test -p slicefs-cli --test flusher_tests dirty_total_stays_bounded_with_parallel_writers` | ✅ after 20-02 | ⬜ pending |
| 20-07-02 | 07 | 7 | LOCK-06 | T-20-22 | S7 flusher form asserts VmHWM under the fixed cap | measurement knob | `S7_FILE_MIB=8 S7_WRITE_MIB=4 S7_COMMIT_SECS=1 S7_FLUSHER=1 cargo test -p slicefs-cli --test stress_harness -- --ignored --exact --nocapture s7_budget_baseline` | ✅ stress_harness.rs | ⬜ pending |
| 20-07-03 | 07 | 7 | LOCK-06 | T-20-21, T-20-22, T-20-23 | Three Docker runs, 4 writers on 1 GiB files, VmHWM under CAP_MIB, same image and rustc as 20-01, cap unchanged | Docker measurement | grep of `runs/s7-flusher.log` against the CAP_MIB constant; image ID and `rustc -V` against the 20-01 notes; the 20-01 awk on `runs/s7-wave0.log` and an empty `git diff` of the 20-01 log and notes since the 20-01 Task 3 commit | ❌ W0 (log) | ⬜ pending |
| 20-08-01 | 08 | 8 | LOCK-02, LOCK-06 | T-20-24, T-20-26 | Public docs match the code; no paths or licensing words in added text | doc gates | grep of the `## Phase 20 result` section and of the added lines | ✅ | ⬜ pending |
| 20-08-02 | 08 | 8 | LOCK-02, LOCK-06 | T-20-25 | Marks follow the evidence; final tree mounts and unmounts; cap unchanged | phase gate + Docker smoke | the full suite command, `lock_stats_report`, `two_cells_panic_even_in_key_order`; grep of `runs/smoke-20-08.log`; the cap check of 20-07-03 | ✅ | ⬜ pending |

*Status: ⬜ pending · ✅ green · ❌ red · ⚠️ flaky*

### Requirement and criterion map

| Criterion | Requirement | Evidence |
|-----------|-------------|----------|
| 1. No take-out site, `keep_budget` included | LOCK-02 | `no_take_out` (20-01, 20-02); `two_cells_panic_even_in_key_order`; the flusher thread with `require_gate` in every debug flusher test |
| 2. Flusher lowers the budget; writer sleeps before any lock; every wait has a time limit | LOCK-06 | `flusher_tests` of 20-02, 20-03, 20-05, 20-06; the shutdown test; the smoke mounts |
| 3. All writers throttled while a commit waits, no deadlock | LOCK-06 | `all_writers_throttled_while_a_commit_waits_flusher_paused`, `..._flusher_in_a_step`, `the_flusher_skips_a_cell_with_a_step_in_flight` (20-04) |
| 4. VmHWM under one fixed cap in Docker | LOCK-06 | CAP_MIB fixed in 20-01 from P_old and P0; `dirty_total_stays_bounded_with_parallel_writers` and the three P1 runs (20-07) |

---

## Wave 0 Requirements

- [ ] `crates/slicefs-cli/tests/no_take_out.rs` — Plan 01 (extended in Plan 02)
- [ ] S7 knob `S7_SERIALIZE` and the VmHWM reader in `stress_harness.rs` — Plan 01
- [ ] Baseline P_old (3 serialized runs) and probe P0 (3 serialize-off runs, A11) in Docker, and CAP_MIB fixed by the stated rule — Plan 01
- [ ] `crates/slicefs-cli/tests/flusher_tests.rs` with its helpers (block-version model, `throttled_write`, `wait_until`, watchdog, stop guard) — Plan 02
- [ ] Test hooks on `SliceFsFilesystem`: `test_pause_flusher`, `test_set_throttle_wait`, `test_set_flush_pass`, `test_throttled_now`, `test_flusher_running`, `test_flusher_stats` (Plan 02); `test_advance_clock`, `test_set_dirty_idle`, `test_set_dirty_expire`, `test_set_hot_window` (Plan 05)
- [ ] Unit tests in `crates/slicefs-cli/src/flusher.rs` for the spawned thread — Plan 03

The per-task `<automated>` commands and `<fails_when>` signals are in each `20-NN-PLAN.md`.

---

## Manual-Only Verifications

| Behavior | Requirement | Why Manual | Test Instructions |
|----------|-------------|------------|-------------------|
| Hot-rule effect under the S7 load (flusher drops next to steps) | LOCK-06 | A judgement on measured counters, not a pass or fail | Read `flusher_steps` and `flusher_drops` in `runs/s7-flusher.log`; a drop count near the step count means research Pitfall 1 holds under that load; note it in the 20-07 SUMMARY |
| Slowest write with the throttle in a one-thread daemon | LOCK-06 | Phase 21 changes the dispatch; no threshold is set now | Read `slowest_write_ms` and `throttle_wait_max_ms` of the P1 runs (the first is timed after the throttle, so a write's delay is the sum) and write both into the Phase 20 result |

---

## Validation Sign-Off

- [x] All tasks have `<automated>` verify or Wave 0 dependencies
- [x] Sampling continuity: no 3 consecutive tasks without automated verify
- [x] Wave 0 covers all MISSING references
- [x] No watch-mode flags
- [x] Feedback latency < 60s for the quick command (the Docker measurement tasks are long by nature and run in the background)
- [x] `nyquist_compliant: true` set in frontmatter

**Approval:** pending
