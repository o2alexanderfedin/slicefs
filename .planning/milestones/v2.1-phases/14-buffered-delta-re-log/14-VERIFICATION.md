---
phase: 14-buffered-delta-re-log
verified: 2026-11-20T09:39:49Z
status: passed
score: 5/5 must-haves verified
covered_files:
  - ".planning/phases/14-buffered-delta-re-log/14-01-PLAN.md"
  - ".planning/phases/14-buffered-delta-re-log/14-01-SUMMARY.md"
  - ".planning/phases/14-buffered-delta-re-log/14-02-PLAN.md"
  - ".planning/phases/14-buffered-delta-re-log/14-02-SUMMARY.md"
  - ".planning/phases/14-buffered-delta-re-log/14-03-PLAN.md"
  - ".planning/phases/14-buffered-delta-re-log/14-03-SUMMARY.md"
  - ".planning/phases/14-buffered-delta-re-log/14-04-PLAN.md"
  - ".planning/phases/14-buffered-delta-re-log/14-04-SUMMARY.md"
  - ".planning/phases/14-buffered-delta-re-log/14-05-PLAN.md"
  - ".planning/phases/14-buffered-delta-re-log/14-05-SUMMARY.md"
  - ".planning/phases/14-buffered-delta-re-log/14-06-PLAN.md"
  - ".planning/phases/14-buffered-delta-re-log/14-06-SUMMARY.md"
  - ".planning/phases/14-buffered-delta-re-log/14-07-PLAN.md"
  - ".planning/phases/14-buffered-delta-re-log/14-07-SUMMARY.md"
  - ".planning/phases/14-buffered-delta-re-log/14-08-PLAN.md"
  - ".planning/phases/14-buffered-delta-re-log/14-08-SUMMARY.md"
  - ".planning/phases/14-buffered-delta-re-log/14-09-PLAN.md"
  - ".planning/phases/14-buffered-delta-re-log/14-09-SUMMARY.md"
  - "crates/chunk-store/src/file.rs"
  - "crates/chunk-store/src/retry.rs"
  - "crates/chunk-store/tests/retry_tests.rs"
  - "crates/metadata/src/segment/writer.rs"
  - "crates/metadata/src/store.rs"
  - "crates/metadata/src/wal/buffered.rs"
  - "crates/metadata/src/wal/flush_on_fsync.rs"
  - "crates/metadata/src/wal/mod.rs"
  - "crates/metadata/src/wal/per_op.rs"
  - "crates/metadata/src/wal/periodic.rs"
  - "crates/slicefs-cli/src/commit_timer.rs"
  - "crates/slicefs-cli/src/content.rs"
  - "crates/slicefs-cli/src/filesystem.rs"
  - "crates/slicefs-cli/src/overlay.rs"
  - "crates/slicefs-cli/tests/dirty_budget_tests.rs"
  - "crates/slicefs-cli/tests/fsync_tests.rs"
  - "crates/slicefs-cli/tests/gc_online_tests.rs"
  - "crates/slicefs-cli/tests/memory_bound_tests.rs"
  - "crates/slicefs-cli/tests/rebase_step_tests.rs"
  - "crates/slicefs-cli/tests/replay_memory_tests.rs"
  - "crates/slicefs-cli/tests/write_log_replay_tests.rs"
  - "crates/slicefs-cli/tests/write_path_tests.rs"
  - "docs/design/chunk-store.md"
  - "docs/design/write-log.md"
  - "scripts/docker-linux-test/bigwrite.sh"
  - "scripts/docker-linux-test/run.sh"
  - "scripts/docker-linux-test/verify-phases.sh"
covered_digest: "v4:sha256:4f52de67cf766d1002ee574bd3802534744c3e1ad18faedee19d0a2b4c46e572"
behavior_unverified: 0
overrides_applied: 0
re_verification:
  previous_status: passed
  previous_score: 5/5
  gaps_closed: []
  gaps_remaining: []
  regressions: []
---

# Phase 14: Bounded-memory random writes - Verification Report

**Phase Goal:** A random write never loads the whole file into RAM. The memory of an open file is bounded by a fixed limit, and the write-log cost of a commit depends on the changed bytes, not on the file size (#98, which also fixes #93).
**Verified:** 2026-11-20T09:39:49Z (tree a7b29a2 = develop, plus planning-doc edits on branch docs/v2.1-close-items)
**Status:** passed
**Re-verification:** Yes. The report of 2026-11-19 (tree b4b5494) was stale: 87 commits came after it, and about 45 of them changed covered files (#105 dirty budget, #106 streamed replay, #108 rebase steps, background pack sync and seal, commit gate held only for the cut, #112 locality cache, #113 Bloom-filter GC mark, #133 crash cases). Every truth was checked again on the current code. The previous report had no `gaps:` section.

Method: code read and grep on the tree. Test results come from CI run 44704112946 (push to develop, a7b29a2, conclusion success). Its macOS `cargo test --workspace` log has one `... ok` line for each test named below, and no `test result: FAILED` line. PR #144 checks (Linux Docker with live checks, macOS cargo test) pass. Docker `bigwrite` and the macOS FSKit large copy come from `.planning/v2.1-MILESTONE-AUDIT.md` (committed in a7b29a2). PLAN files have no `<human-check>` blocks and no `prohibitions:`; the roadmap goal and the five success criteria are unchanged. The diff e5e55e1..a7b29a2 changes only that audit file, so the e5e55e1 runs apply to this tree. No test was run locally. No file system was mounted. Docker was not run.

## Goal Achievement

### Observable Truths (roadmap success criteria)

| # | Truth | Status | Evidence |
|---|-------|--------|----------|
| 1 | A 1 GiB and a 4 GiB random-write test in Docker keep the daemon peak RSS (VmHWM) below the same fixed cap | VERIFIED | `bigwrite.sh` still has `CAP_MIB=256` and the `bw-rss-bounded-S` check (VmHWM from `/proc/$MP/status`, line 34, 161). Run by hand on e5e55e1: `RESULT bigwrite: PASS`, VmHWM 106 MiB (1 GiB) and 165 MiB (4 GiB), cap 256 MiB, slowest write 124 / 709 ms (audit line 41). The code is the same on a7b29a2. Before phase 14 the value was 6224 MiB. Other evidence: macOS FSKit large copy (CI run 44702241281, workflow_dispatch on e5e55e1, success), 1 GiB peak RSS 65 MiB; `an_open_file_uses_memory_bounded_by_the_dirty_limit_not_the_file_size` (counting allocator) is ok in CI on a7b29a2. |
| 2 | A commit with a large open file and a small change writes about the size of the change to the write log | VERIFIED | `commit_cut_begin` (`filesystem.rs:1575`) takes only descriptors under the gate. After `meta.begin_commit()` it relogs `Setattr` records and the overlay extents in pieces of at most `RELOG_PIECE` = 1 MiB (`filesystem.rs:1655-1668`). Nothing is hashed. `a_commit_relogs_only_the_bytes_changed_since_the_last_rebase` asserts `(4096, 8192, 4096)` for each file size and "no rebase at a commit" (`write_log_replay_tests.rs:616-631`). It is ok in CI. |
| 3 | Replay after a crash gives the same content, size and times, for PerOp and FlushOnFsync | VERIFIED | `an_overlay_replays_after_a_crash_with_each_log_mode` (`[PerOp, FlushOnFsync]` x 3 crash points, full dump compare) and `an_overlay_with_more_than_64_mib_of_relog_replays` are ok in CI. New: `a_crash_between_the_cut_and_the_root_replays` also covers `Periodic { interval_secs: 1 }` (line 1109), and there are torn-segment cases (lines 1132, 1167). Replay is streamed (`replay_memory_is_bounded_by_a_record_not_by_the_log`, ok). |
| 4 | The commit gate is not held for the hash of a whole file; online GC during a rebase loses no chunk | VERIFIED | `timed_commit` (`commit_timer.rs:43-59`) holds the gate exclusively only around `commit_cut_begin`, then drops it before `commit_cut_finish`. `begin_commit` (`metadata/src/store.rs:685`) cuts the log and clones maps. It writes no node and syncs nothing. The rebase runs only in callbacks, in bounded steps (`REBASE_STEP` = 1 MiB, `filesystem.rs:1205-1211`, `keep_budget`). Tests ok in CI: `a_timed_commit_holds_the_gate_for_a_small_part_of_the_commit`, `commit_and_fsync_do_not_rebase_an_open_overlay`, `online_gc_during_a_rebase_keeps_every_chunk`, `gc_start_waits_for_a_rebase_in_a_callback`, `an_open_overlay_keeps_its_base_through_online_gc`. `gc_start` still keeps open overlay bases (`filesystem.rs:690`). Narrowed reading (kept): the gate is never held **exclusively** for a hash. On long repeating non-zero data, one rebase step can re-chunk to EOF while its callback holds the gate shared, so a commit or `gc_start` can wait (`rebase_step` doc, `filesystem.rs:899-905`; `write-log.md:133-136`; accepted in the audit). |
| 5 | After a small change in a large file, the store grows by about the changed chunks only | VERIFIED | `chunk_store::file::splice` with the oracle tests `a_splice_gives_the_same_key_as_a_full_write`, `a_splice_across_many_leaves_gives_the_same_key`, `a_small_splice_reuses_old_nodes`. `a_small_change_adds_only_the_changed_chunks` asserts at most 12 new objects, CAS growth < 1 MiB, and more than 3000 chunk IDs kept (`write_path_tests.rs:493-511`). `an_extend_without_a_handle_rechunks_little`. All are ok in CI. |

**Score:** 5/5 truths verified (0 present-but-behavior-unverified)

### Required Artifacts

| Artifact | Expected | Status | Details |
|----------|----------|--------|---------|
| `crates/slicefs-cli/src/overlay.rs` | Overlay, 1/8 growth | VERIFIED | `a_growing_extent_does_not_double_its_capacity` ok in CI |
| `crates/slicefs-cli/src/filesystem.rs` | Overlay arms, dirty limit, relog pieces, splice rebase, global budget | VERIFIED | `DIRTY_LIMIT` 32 MiB, `REBASE_STEP` 1 MiB, `DIRTY_BUDGET` 64 MiB (#105), `RELOG_PIECE` 1 MiB; `content::splice` at lines 932 and 1910 |
| `crates/slicefs-cli/src/commit_timer.rs` | Gate held only for the cut | VERIFIED | `timed_commit`; production caller of `commit_cut_begin`/`finish`, started by `mount.rs:404` |
| `crates/chunk-store/src/file.rs` | `splice`, `SpliceSource` | VERIFIED | Splice oracle tests ok |
| `crates/chunk-store/src/retry.rs` | Bounded RAM copy of unsynced pack writes | VERIFIED | `KEEP_LIMIT` = 8 MiB, `KEEP_CAP` = 3 x `KEEP_LIMIT` (the writer waits past it); `the_copy_of_unsynced_writes_stays_below_the_keep_limit` ok |
| `crates/metadata/src/wal/*` (14-09) | Bounded write-log buffer, modes | VERIFIED | `BUFFER_CAP_BYTES` = 4 MiB (`wal/mod.rs:37`), sticky `failed` flag (`buffered.rs:25`); ok in CI: `wal::per_op::tests::op_record_reaches_the_file_without_a_sync`, `wal::flush_on_fsync::tests::ram_stays_at_most_the_cap_with_no_fsync`, `wal::periodic::tests::ram_stays_at_most_the_cap_with_no_fsync`, `wal::periodic::tests::timer_syncs_and_drop_stops_it`, and the failed-write tests of both modes |
| `crates/slicefs-cli/tests/memory_bound_tests.rs` | Counting-allocator proof | VERIFIED | `#[global_allocator]`, `PEAK_BOUND` 12 MiB, `SIZE_SLACK` 2 MiB; ok in CI |
| `scripts/docker-linux-test/bigwrite.sh`, `run.sh bigwrite` | Docker VmHWM check | VERIFIED | By hand, not in CI (`run.sh:10`); PASS on e5e55e1 |

### Key Link Verification

| From | To | Via | Status |
|------|----|-----|--------|
| commit timer thread | `commit_cut_begin` / `commit_cut_finish` | `timed_commit` (`commit_timer.rs:55-58`), started at `mount.rs:404` | WIRED |
| `test_write` (FUSE write entry) | `rebase_step` / `keep_budget` | `cost > limit` after each overlay write (`filesystem.rs:1205-1216`) | WIRED |
| rebase / setattr | `chunk_store::file::splice` | `content::splice` (`filesystem.rs:932, 1910`) | WIRED |
| `commit_cut_begin` | write log | `log_record` of `RELOG_PIECE` pieces after `begin_commit` | WIRED |
| `gc_start_mode` | open overlay bases | `WriteMode::Overlay(ov) => file_roots.extend(ov.base)` (`filesystem.rs:690`) | WIRED |

Whole-file gate (production region of `filesystem.rs`, before `#[cfg(test)]` at line 2726): there is no `Buffered`, `handle_content`, `pwrite_into`, `buf.resize(`, `content.resize(` or `buf.clone()`. `content::get_all` is used only in `read_content` (line 402, called only from lib tests) and in `simulate_readlink` (line 2393, symlink target, `handlers.rs:420`). `content::put_all` is used only for a symlink target (line 2370).

### Behavioral Spot-Checks

| Behavior | Command | Result | Status |
|----------|---------|--------|--------|
| Phase-14 tests on a7b29a2 | `gh run view 44704112946 --log`, grep `<name> ... ok` | 15 named tests + `memory_bound`, `replay_memory`, `dirty_budget`, `rebase_step` suites run; 0 `test result: FAILED` | PASS |
| CI of develop head | `gh run list --branch develop` | 44704112946 (CI, a7b29a2) success; PR #144 Linux Docker and macOS pass | PASS |
| macOS FSKit large copy | `gh run view 44702241281` | success, head e5e55e1 | PASS |
| Docker bigwrite | not run here (Docker forbidden); audit record on e5e55e1 | PASS, 106 / 165 MiB, cap 256 | PASS (recorded) |

### Probe Execution

Not applicable: the phase declares no `probe-*.sh`. `bigwrite.sh` needs Docker and is a by-hand mode; its recorded result is used above.

### Requirements Coverage

| Requirement | Source Plan | Description | Status | Evidence |
|-------------|-------------|-------------|--------|----------|
| BUFR-01 | 14-04, 14-06, 14-08 | A commit with a large open file logs an amount that depends on the bytes changed, not on the file size | SATISFIED | Truth 2 |
| BUFR-02 | 14-01, 14-06, 14-08 | Replay after a crash gives the same content, size and times (PerOp, FlushOnFsync) | SATISFIED | Truth 3; Periodic also covered now |
| BUFR-03 | 14-02, 14-04, 14-05, 14-06, 14-08 | The commit gate is not held for a whole-file hash; online GC during a rebase loses no chunk | SATISFIED | Truth 4 |
| BUFR-04 | 14-01..05, 14-07, 14-08 | A random write never loads the whole file into RAM; 1 GiB and 4 GiB Docker runs stay below the same VmHWM cap | SATISFIED | Truth 1 and the whole-file gate |
| BUFR-05 | 14-02, 14-05, 14-08 | After a small change, the store grows by about the changed chunks only | SATISFIED | Truth 5 |

Orphaned requirements: none. REQUIREMENTS.md maps BUFR-01..05 to Phase 14, and plans claim each of them. 14-09 declares `requirements: []`.

Issues #93, #98, #105, #106 and #108 are all CLOSED.

### Anti-Patterns Found

| File | Line | Pattern | Severity | Impact |
|------|------|---------|----------|--------|
| covered source, tests, scripts, docs | - | TBD / FIXME / XXX | none found | - |
| `crates/slicefs-cli/src/filesystem.rs` | 92-96 | `DIRTY_LIMIT` doc gives the 14-07 numbers (114/161 MiB, slowest write 5870/7253 ms) | Info | Historical numbers. They do not show the current run (106/165 MiB, 124/709 ms) |
| `docs/design/chunk-store.md` | 120-127 | Docker table has only the 14-07 runs | Info | Same as above. The current numbers are only in the milestone audit |
| `.planning/phases/14-buffered-delta-re-log/deferred-items.md` | - | Retry buffer item | Info | Now marked `resolved` by 0285b2a (working-tree edit); the code agrees (`KEEP_LIMIT`, `retry_tests`) |

### Human Verification Required

None. The Docker VmHWM check that was the open item in the first report was run again on e5e55e1 (code-identical to a7b29a2) and passed.

### Gaps Summary

No gap. All five success criteria hold on the current tree, and the follow-up work after b4b5494 kept them. The commit gate is now held for less time (only the cut). The dirty budget now also covers all open files (#105). Replay is streamed (#106). The write past the limit is bounded (#108). Notes that remain, none blocking:

1. BUFR-01 reading: a commit relogs the bytes changed since the last rebase step, not since the last commit. This is bounded by `DIRTY_LIMIT`, not by the file size (accepted in the audit).
2. Docker `bigwrite` is by hand, not in CI. Its raw log is not in the repo. The result is recorded only in `.planning/v2.1-MILESTONE-AUDIT.md`. A regression in daemon RSS would be found only by `memory_bound_tests` (in-process allocator) and the macOS large-copy RSS check.
3. BUFR-03 reading: on long repeating non-zero data, one rebase step can re-chunk to EOF under the shared gate, so a commit can wait. Memory stays bounded (accepted in the audit).
4. Under a disk slower than the writes, a writer waits at `KEEP_CAP` (by design: bounded RAM; #108 closed with this choice).

---

_Verified: 2026-11-20T09:39:49Z_
_Verifier: Claude (gsd-verifier)_
