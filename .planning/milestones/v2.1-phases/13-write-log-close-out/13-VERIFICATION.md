---
phase: 13-write-log-close-out
verified: 2026-11-20T10:00:00Z
status: passed
score: 3/3 roadmap success criteria verified (plus 3/3 plan truths; 1 historical deviation, resolved)
covered_files:
  - ".github/workflows/macos-mounts.yml"
  - ".planning/phases/13-write-log-close-out/13-01-PLAN.md"
  - ".planning/phases/13-write-log-close-out/13-01-SUMMARY.md"
  - ".planning/phases/13-write-log-close-out/13-02-PLAN.md"
  - ".planning/phases/13-write-log-close-out/13-02-SUMMARY.md"
  - ".planning/phases/13-write-log-close-out/13-03-PLAN.md"
  - ".planning/phases/13-write-log-close-out/13-03-SUMMARY.md"
  - "crates/slicefs-cli/tests/write_log_replay_tests.rs"
  - "scripts/docker-linux-test/verify-phases.sh"
  - "scripts/macos-mount-test/README.md"
  - "scripts/macos-mount-test/large-copy.sh"
covered_digest: "v4:sha256:c14396277dcf5246e687719854e19352ae1bde6ba066135a749828c0556b3c03"
behavior_unverified: 0
overrides_applied: 0
re_verification:
  previous_status: human_needed
  previous_score: "2/3 roadmap success criteria verified (SC2 Linux half verified, FSKit half needs a human)"
  gaps_closed:
    - "SC2 / WLOG-02 FSKit half: 1 GiB large copy on FSKit now runs in the macOS CI workflow; run 44702241281 on e5e55e1 PASS (peak daemon RSS 65 MiB, limit 512 MiB, cmp ok after copy and after remount)"
  gaps_remaining: []
  regressions: []
---

# Phase 13: Write log close-out Verification Report

**Phase Goal:** Issue #20 is closed with evidence: each acceptance item maps to a passing test or to a follow-up issue
**Verified:** 2026-11-20, on branch docs/v2.1-close-items (code = develop a7b29a2; a7b29a2 differs from e5e55e1 only in `.planning/v2.1-MILESTONE-AUDIT.md`; the working tree has no non-planning changes)
**Status:** passed
**Re-verification:** Yes. The previous report (2026-11-18, tree b4b5494) was `human_needed` with one item, the FSKit large copy. Covered files changed since then (11 commits touch them), so every truth was checked again against the current tree.

Evidence base (no mount and no Docker run on this Mac):
static reads of the code, scripts and workflow; `git diff b4b5494 HEAD` of the covered files;
CI run 44704112946 (push, a7b29a2, success): Linux "Live checks" log (`RESULT verify-phases: PASS`) and macOS `cargo test --workspace` log (`write_log_replay_tests`: 23 passed);
macOS mounts run 44702241281 (workflow_dispatch, develop, headSha e5e55e1, success): `RESULT large-copy: PASS`;
`gh issue view 20` (CLOSED 2026-11-18T20:44:53Z, 4 comments); `.planning/v2.1-MILESTONE-AUDIT.md` (passed on e5e55e1).

## Goal Achievement

### Observable Truths

| # | Truth | Status | Evidence |
|---|-------|--------|----------|
| SC1 | Tests show: a file survives `kill -9` after its `fsync` returns; a rename survives after `fsync` of the directory; a torn last record does not stop the mount | VERIFIED | Rows 1a-1c. |
| 1a | File survives after fsync | VERIFIED | Real kill -9 in CI run 44704112946: `closed-file-survives-crash: ok`, `fsynced-file-survives: ok`, `old-name-gone: ok`. In process: `files_written_and_fsynced_survive_a_crash` (l.166), `a_file_still_open_at_the_crash_keeps_its_fsynced_bytes` (l.216), `fsynced_buffered_bytes_survive_a_commit_and_a_crash` (l.370) pass in the macOS CI cargo test (23/23). |
| 1b | Rename survives after fsync of the directory | VERIFIED | In process: `a_rename_survives_a_crash_after_fsync_of_the_directory` (l.413, `WalConfig::FlushOnFsync`, `h::handle_fsyncdir` at l.405) and control `a_rename_with_no_sync_after_it_is_lost_with_a_buffered_log` (l.426): both `ok` in CI. Real kill -9: 05c main mount now uses `--wal-strategy flush-on-fsync` (fixes old T1), so only the directory fsync can keep the rename; `rename-survives-dir-fsync: ok`, `old-name-gone-after-dir-fsync: ok`; the control is now a gating check `control-rename-without-sync-lost: ok` (fixes old T2: a kept rename now fails the run). |
| 1c | Torn last record does not stop the mount | VERIFIED | In process: `a_torn_last_record_does_not_stop_the_mount` (l.454, `set_len(len - 3)` at l.480): `ok` in CI. Extra torn-segment cases from #133 also pass (`a_torn_segment_before_a_cut_ends_the_log_there`, `the_log_of_a_mount_after_a_torn_cut_replays`). Real kill -9 05d: `mount-with-torn-last-record`, `torn-record-not-applied`, `write-after-torn-tail`, `scrub-after-torn-tail` all ok. |
| SC2 | A large-file copy check for the current backends (libfuse in Docker; FSKit by hand) passes with bounded daemon memory | VERIFIED | **libfuse (Docker, CI run 44704112946):** `big-copy-in-order`, `big-write-out-of-order`, both cmp steps, `daemon-rss-bounded`, `no-switch-to-overlay`, `daemon-rss-bounded-at-unmount`, both cmp-after-remount steps: all ok; `daemon VmHWM: after cp 49 MiB, after out-of-order write 82 MiB, after cmp 82 MiB, at unmount 82 MiB (cap 512 MiB)` for a 1 GiB file. **FSKit (macOS, run 44702241281, e5e55e1):** step "Large copy (fskit)" runs `scripts/macos-mount-test/large-copy.sh fskit`; log: `copy: rc=0`, `cmp-after-copy: rc=0`, `cmp-after-remount: rc=0`, `large-copy file size: 1073741824 bytes`, `large-copy peak RSS: 65 MiB (limit 512 MiB: ok)`, `cmp after copy: ok`, `cmp after remount: ok`, `RESULT large-copy: PASS`; Boost `tar-cmp: ok` and `tar-cmp-after-remount 1_85_0: ok` in the same job. The script samples `ps -o rss=` of the daemon PID each second and fails on a cmp failure or on peak > limit (or peak 0), so the PASS is not vacuous. The check now runs in CI instead of by hand, which is stronger than the criterion asks. |
| SC3 | #20 is closed with a comment that lists the tests and points the commit-cost item to #93 | VERIFIED | #20 CLOSED 2026-11-18T20:44:53Z; evidence comment (= 13-issue-20-comment.md) maps items 1-5, item 5 to #93. A later correction comment (2026-11-19) updates item 2 (05c flush-on-fsync + gating control), item 4 (overlay, VmHWM incl. unmount) and reports the FSKit CI result (run 44517381182: 1 GiB, peak RSS 64 MiB, both cmp ok). This fixes old T3. |
| P1 | Plan 13-02: no handle switches to the overlay during the large copy | VERIFIED | `no-switch-to-overlay: ok` in CI; `nbuf()` greps "switches to overlay" (source string still present, per previous report). |
| P2 | Plan 13-03: README tells a person how to run the FSKit copy and what to report | VERIFIED | README section "Large copy on FSKit (#20)" keeps the manual steps, now points to `large-copy.sh` and the CI step; `waitexit` now has a 120 s limit (fixes old T6). |
| P3 | Plan 13-03: "#20 is closed only after ... the FSKit check passed" | VERIFIED (historical deviation, resolved) | #20 was closed before the FSKit run (accepted by the user per 13-03-SUMMARY). The FSKit check has since passed (runs 44517381182 and 44702241281) and the result is posted on #20. |

**Score:** 3/3 success criteria verified (0 present, behavior-unverified). All behavior-dependent truths (crash durability, torn tail, bounded memory) are backed by tests that ran green in CI on the current code.

### Required Artifacts

| Artifact | Expected | Status | Details |
|----------|----------|--------|---------|
| `crates/slicefs-cli/tests/write_log_replay_tests.rs` | 3 #20 tests + helpers | VERIFIED | `safe_save_then_crash` (l.393), `newest_segment` (l.438), 3 tests at l.413/426/454; file grew to 23 tests (#106/#108/#133 cases); 23/23 pass in macOS CI on a7b29a2. |
| `scripts/docker-linux-test/verify-phases.sh` | 05c, 05d, Large copy sections | VERIFIED | 05c now flush-on-fsync with gating control; new 05e (periodic, #133); Large copy adds VmHWM at unmount. `RESULT verify-phases: PASS` in CI. |
| `scripts/macos-mount-test/large-copy.sh` | Automated FSKit large copy (new) | VERIFIED | Timeouts on each wait, RSS sampler on daemon PID, PASS/FAIL with non-zero exit on failure. |
| `.github/workflows/macos-mounts.yml` | Runs large-copy.sh | VERIFIED | Step "Large copy (fskit)" (l.60-66), runs when the build passed, not `continue-on-error`. |
| `scripts/macos-mount-test/README.md` | FSKit large copy section | VERIFIED | See P2. |
| `13-issue-20-comment.md` | Posted text, cites #93 | VERIFIED | Unchanged; matches the #20 evidence comment. |

### Key Link Verification

| From | To | Via | Status | Details |
|------|----|-----|--------|---------|
| write_log_replay_tests.rs | handlers.rs | `h::handle_fsyncdir` | WIRED | l.405 in `safe_save_then_crash`. |
| write_log_replay_tests.rs | write-log segments | `set_len(len - 3)` then reload + replay | WIRED | l.480. |
| CI "Live checks" | verify-phases.sh | run.sh verify | WIRED | Step output in run 44704112946. |
| macos-mounts.yml | large-copy.sh | step "Large copy (fskit)" | WIRED | Step output in run 44702241281. |
| verify-phases.sh 05c | write-log flush-on-fsync | `--wal-strategy flush-on-fsync` on both mounts | WIRED | Diff vs b4b5494. |

### Behavioral Spot-Checks

| Behavior | Command | Result | Status |
|----------|---------|--------|--------|
| In-process #20 tests pass on current code | macOS CI cargo test, run 44704112946 (a7b29a2) | `write_log_replay_tests`: 23 passed, 0 failed | PASS |
| Real kill -9 + Linux large copy | Linux CI "Live checks", run 44704112946 | `RESULT verify-phases: PASS`; all 05b-05e and Large copy steps ok | PASS |
| FSKit large copy | `gh run view 44702241281 --log \| grep -i -E 'large\|rss\|cmp'` | 1 GiB, peak RSS 65 MiB (limit 512), cmp ok x2, `RESULT large-copy: PASS` | PASS |
| Code at a7b29a2 = code at e5e55e1 | `git diff --stat e5e55e1 a7b29a2` | only `.planning/v2.1-MILESTONE-AUDIT.md` | PASS |

No local test run: CI evidence covers the exact code, and other verifiers share the tree.

### Probe Execution

Not applicable: the phase declares no `scripts/*/tests/probe-*.sh` probes.

### Requirements Coverage

| Requirement | Source Plan | Description | Status | Evidence |
|-------------|-------------|-------------|--------|----------|
| WLOG-01 | 13-01, 13-02, 13-03 | #20 items map to passing tests; kill -9: file after fsync, rename after dir fsync, torn last record | SATISFIED | SC1 rows above. |
| WLOG-02 | 13-02, 13-03 | Large copy restated for FSKit and libfuse, passes with bounded daemon memory | SATISFIED | libfuse VmHWM 82 MiB / cap 512 MiB (CI); FSKit peak RSS 65 MiB / limit 512 MiB (run 44702241281). REQUIREMENTS.md `[x]` is now accurate (old T7 resolved). |
| WLOG-03 | 13-03 | Commit-time item points to #93; #20 closed with evidence comment | SATISFIED | #20 closed; item 5 -> #93 (closed). |

No orphaned requirements.

### Anti-Patterns Found

No TBD / FIXME / XXX / TODO markers in the covered implementation files. No stubs.

Info only (not gaps):
- The FSKit RSS sampler reads once per second, so a spike shorter than 1 s could be missed. The Linux check uses VmHWM (a true peak), which does not have this limit.
- The FSKit check measures the slicefs daemon only, not the FUSE-T FSKit extension process. That matches the criterion ("bounded daemon memory").

### Human Verification Required

None. The previous item (FSKit 1 GiB large copy) is closed by macOS CI run 44702241281.

### Gaps Summary

No gaps. All three success criteria hold on the current code, backed by CI runs on a7b29a2 / e5e55e1 (same code). Old tech debt T1, T2, T4, T5, T6 is fixed in the scripts, T3 is fixed by the correction comment on #20, and T7 no longer applies because the FSKit half passed.

---

_Verified: 2026-11-20_
_Verifier: Claude (gsd-verifier)_
