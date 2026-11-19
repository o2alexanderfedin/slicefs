---
phase: 13-write-log-close-out
verified: 2026-11-18T23:30:00Z
status: human_needed
score: 2/3 roadmap success criteria verified (SC2 Linux half verified, FSKit half needs a human)
covered_files:
  - ".planning/phases/13-write-log-close-out/13-01-PLAN.md"
  - ".planning/phases/13-write-log-close-out/13-01-SUMMARY.md"
  - ".planning/phases/13-write-log-close-out/13-02-PLAN.md"
  - ".planning/phases/13-write-log-close-out/13-02-SUMMARY.md"
  - ".planning/phases/13-write-log-close-out/13-03-PLAN.md"
  - ".planning/phases/13-write-log-close-out/13-03-SUMMARY.md"
  - "crates/slicefs-cli/tests/write_log_replay_tests.rs"
  - "scripts/docker-linux-test/verify-phases.sh"
  - "scripts/macos-mount-test/README.md"
covered_digest: "v4:sha256:f4cad56e040d41b4ff2a0342dd96af8f77f3423ecb61a99e3cf45b3d4b23fef3"
behavior_unverified: 0
overrides_applied: 0
human_verification:
  - test: "Run the section 'Large copy on FSKit (by hand, #20)' in scripts/macos-mount-test/README.md on macOS: mount with --backend fskit, cp a 1 GiB file, sample ps -o rss= each second, cmp after the copy, unmount, remount, cmp again."
    expected: "Both cmp results ok. Peak daemon RSS clearly below the file size (same order as the Linux VmHWM, far below 1 GiB). Post file size, peak RSS, both cmp results and the commit as a new comment on #20."
    why_human: "Project rule: no automated macOS mounts. The FSKit half of SC2 / WLOG-02 can only be run by hand, and it has not been run yet."
---

# Phase 13: Write log close-out Verification Report

**Phase Goal:** Issue #20 is closed with evidence: each acceptance item maps to a passing test or to a follow-up issue
**Verified:** 2026-11-18 (retroactive, on tree b4b5494 = origin/develop)
**Status:** human_needed
**Re-verification:** No - initial verification

Evidence base: static reads of the code and scripts; one targeted run of
`cargo test -p slicefs-cli --test write_log_replay_tests` on macOS (18 passed);
the Linux CI job log of PR #114 (run 44477290116, "Live checks" step,
`RESULT verify-phases: PASS`); the PR #114 macOS job (cargo test, green);
`gh issue view 20` (CLOSED 2026-11-18T20:44:53Z) and `gh issue view 93` (CLOSED).

## Goal Achievement

### Observable Truths (roadmap success criteria + plan must-haves)

| # | Truth | Status | Evidence |
|---|-------|--------|----------|
| SC1 | Tests show: a file survives `kill -9` after its `fsync` returns; a rename survives after `fsync` of the directory; a torn last record does not stop the mount | VERIFIED | See rows 1a-1c. |
| 1a | File survives after fsync | VERIFIED | Real kill -9: verify-phases.sh Phase 05b `fsynced-file-survives` (`--commit-interval 0`), Phase 05 `closed-file-survives-crash`; both `ok` in PR #114 CI log. In process: write_log_replay_tests.rs l.161, l.211, l.365; crash_recovery_tests.rs l.113, l.225; fsync_tests.rs l.103 (all exist). |
| 1b | Rename survives after fsync of the directory | VERIFIED (with tech-debt note T1) | In process: `a_rename_survives_a_crash_after_fsync_of_the_directory` (l.408) uses `WalConfig::FlushOnFsync`, `handle_fsyncdir`, `drop(fs)`, replay; control `a_rename_with_no_sync_after_it_is_lost_with_a_buffered_log` (l.421) asserts the old content and the temp name remain. Both pass in the targeted run. Real kill -9: Phase 05c `rename-survives-dir-fsync`, `old-name-gone-after-dir-fsync` ok in CI; control line `rename with no sync after it (flush-on-fsync): lost` printed. |
| 1c | Torn last record does not stop the mount | VERIFIED | In process: `a_torn_last_record_does_not_stop_the_mount` (l.449) cuts 3 bytes of the newest segment with `set_len(len - 3)`, asserts dump equals pre-mkdir dump, `last` absent, new write survives a second crash. Passes. Real kill -9: Phase 05d `cut 3 bytes of segment-000002.seg`, `mount-with-torn-last-record`, `records-before-the-tear-kept`, `torn-record-not-applied`, `write-after-torn-tail`, `scrub-after-torn-tail` all ok in CI. |
| SC2 | A large-file copy check for the current backends (libfuse in Docker; FSKit by hand) passes with bounded daemon memory | UNCERTAIN (human needed) | Linux half VERIFIED: "Large copy (#20)" section in verify-phases.sh, 1 GiB `cp` plus one-handle out-of-order writer, `cmp` after copy and after remount, VmHWM cap 512 MiB. PR #114 CI: all 8 big-* steps, `daemon-rss-bounded`, `no-switch-to-overlay` ok; `daemon VmHWM: after cp 31 MiB, after out-of-order write 55 MiB, after cmp 55 MiB (cap 512 MiB)`. FSKit half: README steps exist (scripts/macos-mount-test/README.md l.29-89), but the run has not been done. |
| SC3 | #20 is closed with a comment that lists the tests and points the commit-cost item to #93 | VERIFIED | #20 CLOSED 2026-11-18T20:44:53Z. Last comment equals 13-issue-20-comment.md: items 1-5 plus the second-comment item, each mapped to named tests/steps; item 5 "Moved to #93". #93 exists and is CLOSED. Every test name the comment cites exists in the tree (checked by grep, 19 names). |
| P1 | Plan 13-02: no handle switches to Buffered (now: overlay) during the large copy | VERIFIED | `nbuf()` counts "switches to overlay" in mount.log; the string is an unconditional `eprintln!` at crates/slicefs-cli/src/filesystem.rs:971, so the step is not vacuous. |
| P2 | Plan 13-03: README tells a person how to run the FSKit copy and what to report | VERIFIED | README section has build, mount `--backend fskit`, 1 s `ps -o rss=` sampler with peak, `cmp` after copy and after remount, and the report list. |
| P3 | Plan 13-03: "#20 is closed only after ... the FSKit check passed" | DEVIATION (accepted by the user per 13-03-SUMMARY) | #20 was closed before the FSKit run; the comment says the FSKit result comes later. Recorded as human item, not as a code gap. |

**Score:** SC1 and SC3 verified; SC2 partly verified (Linux), FSKit half needs a human. No FAILED truth. 0 present-but-behavior-unverified truths.

### Required Artifacts

| Artifact | Expected | Status | Details |
|----------|----------|--------|---------|
| `crates/slicefs-cli/tests/write_log_replay_tests.rs` | 3 new #20 tests + helpers | VERIFIED | `safe_save_then_crash`, `newest_segment`, 3 tests; substantive asserts; 18/18 pass in targeted run. |
| `scripts/docker-linux-test/verify-phases.sh` | Phase 05c, 05d, Large copy sections | VERIFIED | Sections at l.96, l.135, l.188; wired from run.sh verify (l.24) and `.github/workflows/ci.yml` l.26-27 "Live checks". |
| `scripts/macos-mount-test/README.md` | "Large copy on FSKit (by hand, #20)" | VERIFIED | l.29-89. |
| `13-issue-20-comment.md` | Exact posted text, contains #93 | VERIFIED | Matches the closing comment on #20. |

### Key Link Verification

| From | To | Via | Status | Details |
|------|----|-----|--------|---------|
| write_log_replay_tests.rs | handlers.rs | `h::handle_fsyncdir` | WIRED | Called in `safe_save_then_crash` when `fsync_dir` is true. |
| write_log_replay_tests.rs | segment/mod.rs | `set_len(len - 3)` then `load_store_with_log` + `oplog::replay` | WIRED | `reload_and_replay_with` l.44-53. |
| run.sh verify | verify-phases.sh | container build + 45 s wait | WIRED | run.sh l.24; CI step "Live checks". |
| verify-phases.sh | filesystem.rs | "switches to overlay" log count | WIRED | Pattern renamed from "switches to Buffered" by Phase 14 (cfe369a); still matches the source string. |
| verify-phases.sh | handlers.rs | `sync <dir>` -> FSYNCDIR -> `handle_fsyncdir` | WIRED | l.105 (05c), l.123 (control), l.142 (05d). |
| 13-issue-20-comment.md | README / tests / script | cited names | WIRED | All cited names present. |

### Behavioral Spot-Checks

| Behavior | Command | Result | Status |
|----------|---------|--------|--------|
| In-process rename/torn-tail tests pass | `cargo test -q -p slicefs-cli --test write_log_replay_tests` | `18 passed; 0 failed` (6.0 s) | PASS |
| Real kill -9 + large copy checks pass on this tree | PR #114 CI Linux job log (run 44477290116) | `RESULT verify-phases: PASS`; all 05b/05c/05d/Large copy steps ok | PASS |
| macOS cargo test on this tree | PR #114 CI macOS job | SUCCESS | PASS |
| FSKit large copy | by hand only | not run | SKIP (human) |

### Probe Execution

Not applicable: the phase declares no `scripts/*/tests/probe-*.sh` probes.

### Requirements Coverage

| Requirement | Source Plan | Description | Status | Evidence |
|-------------|-------------|-------------|--------|----------|
| WLOG-01 | 13-01, 13-02, 13-03 | Each #20 acceptance item maps to a passing test or a follow-up issue; kill -9 items: file after fsync, rename after directory fsync, torn last record | SATISFIED | Rows 1a-1c above: in-process tests pass (targeted run), Docker real kill -9 steps pass (PR #114 CI). |
| WLOG-02 | 13-02, 13-03 | The "786 MB over FUSE-T NFS" item is restated for FSKit and libfuse, and the restated check passes with bounded daemon memory | NEEDS HUMAN (Linux half SATISFIED) | Linux: 1 GiB copy, VmHWM 55 MiB vs 512 MiB cap in PR #114 CI. FSKit: steps written, run not done. REQUIREMENTS.md marks WLOG-02 `[x]` / Complete while the FSKit run is open. |
| WLOG-03 | 13-03 | Commit-time item points to #93; #20 closed with a comment that lists the evidence | SATISFIED | #20 closed with the evidence comment; item 5 points to #93 (#93 itself closed by Phase 14). |

No orphaned requirements: REQUIREMENTS.md maps only WLOG-01..03 to Phase 13, and all three are claimed by plans.

### Anti-Patterns Found

No TBD / FIXME / XXX markers in the three changed files. No stubs.

### Human Verification Required

#### 1. FSKit large copy (SC2 / WLOG-02, macOS half)

**Test:** Run the steps in `scripts/macos-mount-test/README.md`, section "Large copy on FSKit (by hand, #20)".
**Expected:** `cmp after copy: ok`, `cmp after remount: ok`, peak RSS far below 1 GiB. Post the file size, peak RSS, both cmp results and the commit on #20.
**Why human:** No automated macOS mounts (project rule). Known open item; the user runs it by hand.

### Non-critical gaps / tech debt (do not block the phase goal)

- **T1 - Docker 05c no longer isolates the directory fsync.** Since 33b0ebd (Phase 14-09) the default write log is per-op and writes each record into the file at once, so after a `kill -9` of the daemon a rename survives with no sync at all. The main 05c mount uses the default, so `rename-survives-dir-fsync` passes whether or not `sync <dir>` runs. The discriminating evidence is now the in-process FlushOnFsync pair (which passes) and the separate flush-on-fsync control mount. Fix: run the main 05c mount with `--wal-strategy flush-on-fsync` too.
- **T2 - The 05c control does not gate CI.** It is an `echo` ("kept" / "lost"), not a `step`, so a regression to "kept" would not fail the run. Fix: make it a step that requires "lost".
- **T3 - The posted #20 comment is partly out of date against the current tree.** It says the control rename runs "in the same run" (it now runs in its own flush-on-fsync mount), names `no-switch`-to-Buffered behaviour and says "Buffered mode keeps the whole file in RAM" (removed by Phase 14 / #98). The VmHWM numbers quoted (84/112/130 MiB) are from the Phase 13 tree; this tree measures 31/55 MiB in CI. A short follow-up note on #20 together with the FSKit result would fix this.
- **T4 - Stale comment in verify-phases.sh Large copy section** (l.198-199): "a new open of a file that has content makes the next write go to Buffered mode". Since #98 there is no Buffered mode.
- **T5 - VmHWM scope.** H3 is read before the unmount, so the final commit at unmount is not inside the measured peak (stated in 13-02-SUMMARY).
- **T6 - README `waitexit` loop has no time limit** (it ends when the daemon exits, but would wait forever on a hung unmount). Manual script only.
- **T7 - Bookkeeping.** REQUIREMENTS.md marks WLOG-02 Complete and ROADMAP marks Phase 13 `[x]` although the FSKit half of SC2 is pending; plan 13-03's own truth required the FSKit check before closing #20.

### Gaps Summary

No blocking gap. The three kill -9 items are proven by passing in-process tests (run here) and by real kill -9 checks in Docker (green in PR #114 CI on this exact tree). #20 is closed with a comment that maps every item to tests or to #93. The only open part of the goal is the FSKit large copy on macOS, which is a by-hand run that the user has not done yet; this keeps the status at human_needed. Tech debt T1-T3 weaken the Docker evidence for the directory-fsync rename and make the posted comment partly stale, but the in-process FlushOnFsync test plus control still prove the behaviour.

---

_Verified: 2026-11-18_
_Verifier: Claude (gsd-verifier)_
