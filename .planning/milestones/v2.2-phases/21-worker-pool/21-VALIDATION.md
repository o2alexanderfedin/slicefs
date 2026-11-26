---
phase: "21"
slug: "worker-pool"
status: draft
nyquist_compliant: true
wave_0_complete: false
created: "2026-11-23"
---

# Phase 21 — Validation Strategy

> Per-phase validation contract for feedback sampling during execution.

---

## Test Infrastructure

| Property | Value |
|----------|-------|
| **Framework** | `cargo test`; integration tests in `crates/slicefs-cli/tests/` (one binary per file); unit tests in `pool.rs`, `cli.rs`, `fuse_callbacks.rs` (the module is private); features `lock-stats`, `deadlock-detect`; Docker Linux shell checks |
| **Config file** | none; features in `crates/slicefs-cli/Cargo.toml`; Docker image `scripts/docker-linux-test/Dockerfile` |
| **Quick run command** | `export PATH="$HOME/.cargo/bin:$PATH"; cargo test -p slicefs-cli --test pool_tests --test pool_panic_tests --test reply_after_locks --test flusher_tests --test no_take_out` |
| **Full suite command** | `export PATH="$HOME/.cargo/bin:$PATH"; cargo test --workspace && cargo check --workspace --all-targets && cargo check --workspace --all-targets --features slicefs-cli/lock-stats && cargo check -p slicefs-cli --all-targets --features deadlock-detect && cargo test -p slicefs-cli --features deadlock-detect --test stress_harness serialize_off_target && cargo test -p slicefs-cli --features deadlock-detect --test flusher_tests` |
| **Docker runs (long, not in CI)** | tar stream of the committed tree into `slicefs-fusetest`, volume `slicefs-target-p21` (method in the context of 21-01-PLAN.md); `rustc -V` must equal the `RUSTC` line of the 20-01 notes; mount runs (smoke, live checks, pjdfstest, bench) and harness runs (S7) |
| **CI (after a push, by the orchestrator)** | `ci.yml` Linux job with `SLICEFS_WORKERS: 4` (a push or pull request to main or develop); `macos-mounts.yml` on FSKit with the default (a push to `ci/**`); `pool-bench-macos.yml` (a push to `ci-pool/**`); evidence in `runs/ci-evidence.md` and `runs/pool-bench-macos.log` |
| **Estimated runtime** | quick: a few seconds per test binary after a warm build; full: 9 to 20 minutes at load 15-40; Docker smoke or live checks: 5-15 minutes; pjdfstest: up to 90 minutes; S7 sets: 30-90 minutes; the bench: 30-60 minutes |

---

## Sampling Rate

- **After every task commit:** the quick run command, plus the test names in the task's `<verify>`
- **After every plan:** the full suite command (each plan commits only after `cargo test --workspace` exits 0); a Docker smoke with `SLICEFS_WORKERS=4` after any plan that changes a callback
- **Before `/gsd-verify-work`:** the full suite green; the 21-09 gate log (smoke and live checks with 4 workers); the 21-06 and 21-08 records; the orchestrator CI evidence
- **Max feedback latency:** 60 seconds for the quick command (warm build); Docker tasks are long by nature and run in the background with a `timeout`

---

## Per-Task Verification Map

| Task ID | Plan | Wave | Requirement | Threat Ref | Secure Behavior | Test Type | Automated Command | File Exists | Status |
|---------|------|------|-------------|------------|-----------------|-----------|-------------------|-------------|--------|
| 21-01-01 | 01 | 1 | POOL-01, POOL-02 | T-21-01, T-21-02, T-21-04 | getattr on workers, gate in `serve`, reply after it, drain before `shutdown`; `--workers 4` end to end | tracer: Docker smoke | grep of `runs/smoke-21-01.log` (`Workers: 4 (set)`, `pool: workers=4 jobs=[1-9]`, `daemon-exit: ok`, toolchain) | ❌ W0 | ⬜ pending |
| 21-01-02 | 01 | 1 | POOL-01, POOL-02 | T-21-01, T-21-02 | queue bounded in slots and bytes; drain runs queued jobs; drain limit gives Err; [Stats] queue depth and jobs per worker | integration (guard mutations: tests written after the tracer code) | `cargo test -p slicefs-cli --test pool_tests` | ❌ W0 | ⬜ pending |
| 21-01-03 | 01 | 1 | POOL-01, POOL-02 | T-21-03, T-21-05 | 0 refused; default clamp(n/2, 1, 8), macOS 1; inline on the caller; drain before shutdown | unit (guard mutations) | `cargo test -p slicefs-cli --lib -- workers_ default_workers run_without_a_pool destroy_drains_before_shutdown` | ❌ W0 | ⬜ pending |
| 21-02-01 | 02 | 2 | POOL-01, POOL-02 | T-21-06, T-21-07 | read-side callbacks reply after the gate; trace switch | build + unit + count | `cargo test -p slicefs-cli --lib -- fuse_callbacks::`; non-comment `gate_shared(` count 20 | ✅ | ⬜ pending |
| 21-02-02 | 02 | 2 | POOL-02 | T-21-06, T-21-07 | gate only in `serve` and `init`; debug assert at the reply | source scan (red on the half tree) + unit (guard mutation) | `cargo test -p slicefs-cli --test reply_after_locks`; `cargo test -p slicefs-cli --lib -- serve_panics_when_a_lock_is_held_at_the_reply` | ❌ W0 | ⬜ pending |
| 21-02-03 | 02 | 2 | POOL-01, POOL-02 | T-21-06 | smoke with 4 and 1 workers and trace off; live checks on the default pool, at least as many `: ok` as the run on the tree before 21-02 | Docker (baseline run first) | grep of `runs/smoke-21-02.log`, `runs/verify-21-02.log` and `runs/verify-21-02-before.log` | ❌ W0 | ⬜ pending |
| 21-03-01 | 03 | 3 | POOL-03 | T-21-09, T-21-10, T-21-12 | request-thread panic aborts before the unwind; replay gives the state before the half-done update; other threads keep running; full queue aborts | child process (red on a no-op hook) | `cargo test -p slicefs-cli --test pool_panic_tests` | ❌ W0 | ⬜ pending |
| 21-03-02 | 03 | 3 | POOL-03 | T-21-09, T-21-11 | mount aborts (134) on an injected panic with 4 and 1 workers; remount replays; injection only in debug | Docker (red with a no-op hook, then the live checks) + source gate | grep of `runs/panic-replay-red.log` and `runs/verify-21-03.log`; `cfg(debug_assertions)` gate in mount.rs | ❌ W0 | ⬜ pending |
| 21-04-01 | 04 | 4 | POOL-05 | T-21-28 | S7 writer count knob; the cap asserted at 1 to 8 writers | measurement knob | short local S7 at 2 (`cap_checked=true`) and 9 (`cap_checked=false`) writers | ✅ stress_harness.rs | ⬜ pending |
| 21-04-02 | 04 | 4 | POOL-05 | T-21-27 | before-runs at 1, 2, 8 and 16 writers | Docker measurement | counts over `runs/s7-before-limit.log`; toolchain | ❌ W0 (log) | ⬜ pending |
| 21-05-01 | 05 | 5 | POOL-05 | T-21-13, T-21-14, T-21-15, T-21-16 | at most 2 save steps at once; slot waits before the gate, timed, end at stop; skip keeps the hard cap and the stated bound; no-slot flusher does not mark the pass stuck; [Stats] slot waits, steps, bytes freed | integration (red on stubs) | `cargo test -p slicefs-cli --test flusher_tests -- save_steps_stay_at_the_limit a_writer_with_no_slot_writes_without_its_budget_step a_flusher_with_no_slot_does_not_mark_the_pass_stuck a_slot_wait_with_a_lock_held_panics stop_wakes_a_slot_waiter step_stats_count_steps_bytes_and_slot_waits more_writers_than_slots_stay_bounded` | ✅ flusher_tests.rs | ⬜ pending |
| 21-05-02 | 05 | 5 | POOL-05 | T-21-13 | releases wait for a slot before the gate; Must only in cap and finalize steps; `steps:` line at unmount | integration (red on a stub) + source scan (guard mutation) | `cargo test -p slicefs-cli --test flusher_tests releases_stay_at_the_limit`; `cargo test -p slicefs-cli --test no_take_out` | ✅ | ⬜ pending |
| 21-06-01 | 06 | 6 | POOL-05 | T-21-17, T-21-18 | 19 runs: 7 at 4 writers, 3 each at 1, 2, 8 and 16; rule set first | Docker measurement | counts over `runs/s7-step-limit.log`; CAP_MIB still 320 | ❌ W0 (log) | ⬜ pending |
| 21-06-02 | 06 | 6 | POOL-05 | T-21-17 | R1 (hard bar at 1, 2, 4, 8), R2 and R3 (reports), CAP(N) by the stated rule | recompute | awk over the log equals the `R1=`..`R3=` and `CAP(N) = ` lines of the notes | ❌ W0 | ⬜ pending |
| 21-07-01 | 07 | 7 | POOL-04 | T-21-19 | every Linux CI mount uses SLICEFS_WORKERS and fails on a wrong Workers line | script syntax + counts | `bash -n` and counts on verify-phases.sh, pjdfstest.sh, dedup-probe.sh | ✅ | ⬜ pending |
| 21-07-02 | 07 | 7 | POOL-04 | T-21-19, T-21-21 | ci.yml Linux job with 4 workers, a Workers check per step; macOS logs show the default | YAML parse + added-lines gate | Ruby YAML check of ci.yml; `bash -n` of the macOS scripts | ✅ | ⬜ pending |
| 21-07-03 | 07 | 7 | POOL-04 | T-21-19, T-21-20 | red: a wrong expected Workers value gives FAIL; then all, verify and pjdfstest pass with 4 workers; every log has the 20-01 toolchain | Docker | grep of `runs/workers-check-red.log` and the three `runs/docker-*-w4.log` files | ❌ W0 (logs) | ⬜ pending |
| 21-08-01 | 08 | 8 | POOL-05 | T-21-24 | the bench runs end to end (quick form); macOS bench and workflow ready; lock rows in a lock-stats build | Docker quick run + parse checks | `bash -n`; Ruby YAML check; `cargo check -p slicefs-cli --features lock-stats`; grep of `runs/pool-bench-quick.log` | ❌ W0 | ⬜ pending |
| 21-08-02 | 08 | 8 | POOL-05 | T-21-23, T-21-24 | 16 clients at 1/2/4/8/16 workers, 3 rounds, 0 mismatches | Docker bench | counts over `runs/pool-bench.log` | ❌ W0 (log) | ⬜ pending |
| 21-08-03 | 08 | 8 | POOL-05 | T-21-22 | ratios and verdict by the bar set first; memory bar at 1, 2, 4, 8 workers; cause on a miss | recompute | awk medians and the VmHWM rule over the log equal the notes | ❌ W0 | ⬜ pending |
| 21-09-01 | 09 | 9 | POOL-01..05 | T-21-26 | docs match the code and the numbers; no paths or legal-terms words | doc gates | counts of the Phase 21 result, the README part, the deferred list; added-lines gate | ✅ | ⬜ pending |
| 21-09-02 | 09 | 9 | POOL-01..05 | T-21-25 | marks follow the evidence (CI for the final code sha; verdict, mem_bar, R1); final tree mounts with 4 workers | phase gate + Docker | the full suite command; grep of `runs/gate-21-09.log`; mark checks against `ci-evidence.md` (sha), the verdict, `mem_bar` and `R1` | ✅ | ⬜ pending |

*Status: ⬜ pending · ✅ green · ❌ red · ⚠️ flaky*

### Requirement and criterion map

| Criterion | Requirement | Evidence |
|-----------|-------------|----------|
| 1. Mount option; 1 worker inline; macOS default 1 | POOL-01 | cli and default tests, `run_without_a_pool_runs_on_the_caller` (21-01); smoke with 4 and 1 workers (21-01, 21-02) |
| 2. Unmount and SIGTERM drain and join before `destroy()`; replies after all locks | POOL-02 | `destroy_drains_before_shutdown`, drain tests (21-01); `reply_after_locks`, the `serve` assert (21-02); FUSET-05 with 4 workers (21-07, 21-09) |
| 3. Worker panic aborts; replay; no request after a half-done update | POOL-03 | `pool_panic_tests` and the POOL-03 live-check block (21-03) |
| 4. Tests and pjdfstest with 4 workers on Linux CI; macOS FSKit CI with the default | POOL-04 | local Docker logs (21-07) and the orchestrator CI evidence |
| 5. Parallel I/O on different files scales on Linux; recorded | POOL-05 | the bench, its verdict and its memory bar (21-08); the step limit and the S7 memory bar at 1, 2, 4 and 8 writers (21-04, 21-05, 21-06) |

---

## Wave 0 Requirements

- [ ] `crates/slicefs-cli/src/pool.rs` and `crates/slicefs-cli/tests/pool_tests.rs` — Plan 01
- [ ] `SLICEFS_WORKERS` through `run.sh` and `mt-linux.sh` — Plan 01; the other CI-path scripts — Plan 07
- [ ] `crates/slicefs-cli/tests/reply_after_locks.rs` — Plan 02
- [ ] `crates/slicefs-cli/tests/pool_panic_tests.rs` (child-process pattern, new in this repository) and `scripts/docker-linux-test/panic-replay.sh` — Plan 03
- [ ] `S7_THREADS` knob in `stress_harness.rs` — Plan 04
- [ ] Test hooks `test_set_save_steps`, `test_set_slot_wait`, and the slot and [Stats] fields of `FlusherStats` — Plan 05
- [ ] `scripts/docker-linux-test/pool-bench.sh`, `scripts/macos-mount-test/pool-bench.sh`, `.github/workflows/pool-bench-macos.yml` — Plan 08
- [ ] Framework install: none (no new crate)

The per-task `<automated>` commands and `<fails_when>` signals are in each `21-NN-PLAN.md`.

---

## Manual-Only Verifications

| Behavior | Requirement | Why Manual | Test Instructions |
|----------|-------------|------------|-------------------|
| Linux CI with 4 workers and macOS FSKit CI with the default | POOL-04 | CI runs only after a push or a pull request, which the executor does not do | An early check after 21-07; after the last code commit (21-08 Task 1) the orchestrator runs CI for that sha and writes `runs/ci-evidence.md` with `sha = <full sha>` before 21-09 Task 2 |
| macOS bench at 1 and 2 workers | POOL-05 ([Bench]) | macOS mounts run only in CI | The orchestrator pushes `ci-pool/<name>` after 21-08 and saves the log as `runs/pool-bench-macos.log` |
| The follow-up issue on a bench miss | POOL-05 ([Scale bar]) | GitHub changes are for the orchestrator | The orchestrator opens it from the 21-08 notes and gives the number to 21-09 |
| A 16-worker memory result above 320 MiB, or a rise above 64 MiB | POOL-05 | Reports for the owner (orchestrator reading of [Step limit]); they do not block | The 21-06 and 21-08 SUMMARY files list them; the orchestrator tells the owner |

---

## Validation Sign-Off

- [x] All tasks have `<automated>` verify or Wave 0 dependencies
- [x] Sampling continuity: no 3 consecutive tasks without automated verify
- [x] Wave 0 covers all MISSING references
- [x] No watch-mode flags
- [x] Feedback latency < 60s for the quick command (the Docker tasks are long by nature and run in the background)
- [x] `nyquist_compliant: true` set in frontmatter

**Approval:** pending
