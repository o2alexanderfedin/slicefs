---
phase: 18-spikes-baseline-and-stress-harness
verified: 2026-11-21T00:00:00Z
status: passed
score: 4/4 must-haves verified
covered_files:
  - ".github/workflows/s1-overlap.yml"
  - ".planning/phases/18-spikes-baseline-and-stress-harness/18-01-PLAN.md"
  - ".planning/phases/18-spikes-baseline-and-stress-harness/18-01-SUMMARY.md"
  - ".planning/phases/18-spikes-baseline-and-stress-harness/18-02-PLAN.md"
  - ".planning/phases/18-spikes-baseline-and-stress-harness/18-02-SUMMARY.md"
  - ".planning/phases/18-spikes-baseline-and-stress-harness/18-03-PLAN.md"
  - ".planning/phases/18-spikes-baseline-and-stress-harness/18-03-SUMMARY.md"
  - ".planning/phases/18-spikes-baseline-and-stress-harness/18-04-PLAN.md"
  - ".planning/phases/18-spikes-baseline-and-stress-harness/18-04-SUMMARY.md"
  - ".planning/phases/18-spikes-baseline-and-stress-harness/18-05-PLAN.md"
  - ".planning/phases/18-spikes-baseline-and-stress-harness/18-05-SUMMARY.md"
  - ".planning/phases/18-spikes-baseline-and-stress-harness/18-06-PLAN.md"
  - ".planning/phases/18-spikes-baseline-and-stress-harness/18-06-SUMMARY.md"
  - ".planning/phases/18-spikes-baseline-and-stress-harness/18-07-PLAN.md"
  - ".planning/phases/18-spikes-baseline-and-stress-harness/18-07-SUMMARY.md"
  - "crates/chunk-store/src/lock_stats.rs"
  - "crates/slicefs-cli/examples/s1_probe.rs"
  - "crates/slicefs-cli/tests/lock_stats_report.rs"
  - "crates/slicefs-cli/tests/stress_harness.rs"
  - "docs/research/multithreaded-request-handling.md"
  - "scripts/docker-linux-test/run.sh"
  - "scripts/docker-linux-test/s4-waiting.sh"
  - "scripts/docker-linux-test/s6-smallfsync.sh"
  - "scripts/macos-mount-test/s1-overlap.sh"
covered_digest: "v4:sha256:e0d17b1b5b3b4d94da00b2116970f708175a7c19ecd98cebf0c6572fe2c009ea"
behavior_unverified: 0
overrides_applied: 0
---

# Phase 18: Spikes, baseline and stress harness - Verification Report

**Phase Goal:** The migration steps for multithreaded requests are decided by measurements on the current code, and an in-process stress harness exists that the lock phases must pass (#127)
**Verified:** 2026-11-21
**Status:** passed
**Re-verification:** No - initial verification

## Goal Achievement

### Observable Truths (ROADMAP success criteria)

| # | Truth | Status | Evidence |
|---|-------|--------|----------|
| 1 | The research doc has a result for each of S1-S7, each names the migration step it decides; S2 and S5 measured again on current code; old baselines marked superseded | VERIFIED | `docs/research/multithreaded-request-handling.md` has `### S1 result` ... `### S7 result` (lines 107-258) and 7 `**Decides:**` lines, each naming a step (S1 step 6, S2 step 1, S3 step 6, S4 step 2, S5 step 7, S6 step 7, S7 step 5). 6 `[SUPERSEDED ...]` markers on old baselines (1124/1772 ms, inline fsync, 1.3 s gate stall, step 1, S2 row, `flush_wal`). S2 log has 3 runs per size on f31ef2e (post-#131/#133 code); S5 logs for Linux and macOS on d24da49. I spot-checked doc numbers against the saved logs in `runs/` (S1, S2, S3 N=4/8, S4 histograms, S5, S6, S7): all match. |
| 2 | Harness runs N threads of mixed ops (read, write, fsync, create, rename, unlink) on one file and on many files, checks data against a model, reports latency histograms and lock hold/wait times | VERIFIED | `crates/slicefs-cli/tests/stress_harness.rs` (1812 lines). I ran `short_one_file` and `short_many_files` with `--features lock-stats --nocapture`: both pass and print per-op rows (p50/p99/p99.9/max from hdrhistogram), gate wait/hold, commit times, and `lock site=` rows for store, io, wal, wal_writer, open_files, readers with hold and wait p50/p99/max. Many-files mode runs all six op kinds (read 102, write 76, fsync 6, create 33, rename 13, unlink 6). One-file mode runs read, write, fsync, open, release (create/rename/unlink n=0 there, which is by design: a single shared file has no namespace ops; the namespace ops are covered in many-files mode). `model_detects_planted_bug` plants a write and an unlink behind the model and the harness reports both. |
| 3 | Short run for the test suite and a longer run started by hand | VERIFIED | `cargo test -p slicefs-cli --test stress_harness`: 3 passed (short_one_file, short_many_files, model_detects_planted_bug) in 3.85 s; 6 `#[ignore]` tests by hand: `long_run` (env STRESS_THREADS/SECS/...), `serialize_off_target` (Phases 19-21), `s3_store_lock`, `s5_gate_fairness`, `s6_small_fsync`, `s7_budget_baseline`. Short runs assert `max_inflight == 1` under serialize. |
| 4 | S1 run on a macOS FSKit mount with max in flight and reply order recorded; doc states whether the macOS default can be more than one worker | VERIFIED | `runs/s1-macos-fskit.log`: `S1 RESULT arrived=548 max_inflight=2 reply_order_inversions=5`, macOS 26.7.2, FUSE-T 1.2.9. CI run 44882009650 exists and is green (`gh run view`: `ci-s1/phase-18 S1 FUSE-T overlap`, job succeeded). Linux control log shows max_inflight=3, so the probe can see overlap. Commit hashes cited in the doc resolve in git. Doc S1 section states "The macOS default can be more than one worker" with limits (small sample, Phase 21 must show throughput gain). |

**Score:** 4/4 truths verified.

### Plan-level must-haves

| Item | Status | Evidence |
|------|--------|----------|
| `lock-stats` feature: all 7 labels (store, io, wal, wal_writer, wal_pending, open_files, readers) | VERIFIED | `cargo test -p slicefs-cli --features lock-stats --test lock_stats_report` passes (1 passed). Feature chain in `crates/chunk-store/Cargo.toml` (`lock-stats = ["dep:hdrhistogram"]`), metadata and slicefs-cli forward it. |
| hdrhistogram not in normal dependency tree | VERIFIED | `cargo tree -p slicefs-cli -e normal` has 0 hits; chunk-store dependency is `optional = true`; slicefs-cli has it under `[dev-dependencies]` only. |
| Whole workspace builds | VERIFIED | `cargo check --workspace --all-targets` exit 0. |
| S1 probe, client script, CI workflow exist | VERIFIED | `crates/slicefs-cli/examples/s1_probe.rs`, `scripts/macos-mount-test/s1-overlap.sh`, `.github/workflows/s1-overlap.yml` present; no TBD/FIXME/XXX in any file changed by the phase. |
| Run logs for S1-S7 saved | VERIFIED | 11 logs in `runs/`. |

### Requirements Coverage

| Requirement | Source Plans | Status | Evidence |
|-------------|--------------|--------|----------|
| SPIKE-01 | 18-04, 18-05, 18-06, 18-07 | SATISFIED | S1-S7 results in doc, each with a Decides line; S2 and S5 re-measured; superseded baselines marked. |
| SPIKE-02 | 18-01, 18-02, 18-03, 18-06 | SATISFIED | Harness with model, histograms, lock hold/wait; short and long runs. |
| SPIKE-03 | 18-04, 18-07 | SATISFIED | S1 on macOS FSKit CI; doc states the macOS default decision. |

All three IDs in REQUIREMENTS.md map to Phase 18 and are claimed by at least one plan. No orphaned requirements.

### Review fixes (18-REVIEW-FIX.md)

The six fixes (WR-01 to WR-06) are in the code (commits 93cf284, 5e737b0, 3efc359, cdf70cc, b6e281a, ba75077). The spikes were not re-run after the fixes. The doc has a "Notes after the code review" section that says so and states how each fix can affect a recorded number (S1 max_inflight can only go down; S6 key rename; S7 max_dirty can be higher). The S1 conclusion rests on reply-order inversions, which do not depend on the fixed ordering. I accept this as honest reporting.

### Anti-Patterns Found

None blocking. No debt markers in phase-modified files.

### Warnings (not blockers)

1. The S1 prose says "all 250 `stat` calls finished while the 200 ms READ was still open". That figure comes from the old client (fixed 50 ms sleep). The review note in the doc says this metric is not proof of overlap alone. The conclusion does not depend on it, but the sentence in the main S1 text is stronger than the note allows.
2. The S6 table headers use `fsyncs/s` for both platforms, but the Linux value is fsync-time only and the macOS value is wall-based. The review note states this. Comparisons in the prose stay within one platform, so no wrong conclusion follows.
3. The Linux S3 read value at N=4 (319.5 MB/s) is one unexplained dip. The doc says so.

### Human Verification Required

None. The macOS FSKit run was already done in CI and is recorded.

### Gaps Summary

No gaps. The phase goal is met: S1-S7 are decided from saved runs on current code, and an in-process stress harness with a model, histograms and lock timings exists with a short suite run and a long by-hand run.

---

_Verified: 2026-11-21_
_Verifier: Claude (gsd-verifier)_
