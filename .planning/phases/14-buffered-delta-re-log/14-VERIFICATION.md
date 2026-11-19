---
phase: 14-buffered-delta-re-log
verified: 2026-11-19T07:25:06Z
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
  - "crates/metadata/src/wal/buffered.rs"
  - "crates/metadata/src/wal/flush_on_fsync.rs"
  - "crates/metadata/src/wal/mod.rs"
  - "crates/metadata/src/wal/per_op.rs"
  - "crates/metadata/src/wal/periodic.rs"
  - "crates/slicefs-cli/src/content.rs"
  - "crates/slicefs-cli/src/filesystem.rs"
  - "crates/slicefs-cli/src/overlay.rs"
  - "crates/slicefs-cli/tests/fsync_tests.rs"
  - "crates/slicefs-cli/tests/gc_online_tests.rs"
  - "crates/slicefs-cli/tests/memory_bound_tests.rs"
  - "crates/slicefs-cli/tests/write_log_replay_tests.rs"
  - "crates/slicefs-cli/tests/write_path_tests.rs"
  - "docs/design/chunk-store.md"
  - "docs/design/write-log.md"
  - "scripts/docker-linux-test/bigwrite.sh"
  - "scripts/docker-linux-test/run.sh"
  - "scripts/docker-linux-test/verify-phases.sh"
covered_digest: "v4:sha256:a40a1e9611d94d9fff0218970d14a483b534560a688df5e35cfb31efdf77f76b"
behavior_unverified: 0
overrides_applied: 0
human_verification:
  - test: "On the current tree (b4b5494 or later), run `scripts/docker-linux-test/run.sh bigwrite 1024 4096` (by hand, Linux Docker; not on a macOS mount)"
    expected: "`bw-rss-bounded-1024: ok`, `bw-rss-bounded-4096: ok`, both cmp steps ok, `RESULT bigwrite: PASS` under the unchanged CAP_MIB=256"
    why_human: "The only Docker VmHWM run of the new code was at 37de6ad (14-07). After it, 31 commits changed daemon code (14-09 write-log RAM, #107 splice read-ahead, Phase 17 on-disk chunk index). #108 cites later bigwrite runs for write latency (before Phase 17) but records no VmHWM result; no later VmHWM run is recorded. bigwrite is a by-hand mode, so the green CI of PR #114 did not run it. The in-process counting-allocator test is green on HEAD, but it does not measure daemon RSS."
---

# Phase 14: Bounded-memory random writes - Verification Report

**Phase Goal:** A random write never loads the whole file into RAM. The memory of an open file is bounded by a fixed limit, and the write-log cost of a commit depends on the changed bytes, not on the file size (#98, which also fixes #93).
**Verified:** 2026-11-19T07:25:06Z (retroactive, tree b4b5494 = origin/develop)
**Status:** human_needed
**Re-verification:** No (initial verification)

Method: code read and grep on the tree. Test results come from the CI run of PR #114 (run 44477290116; Linux Docker job and macOS `cargo test --workspace` job both pass, on merge commit b4b5494). The macOS log shows a `... ok` line for each named test below. No test was run locally and no file system was mounted.

## Goal Achievement

### Observable Truths (roadmap success criteria)

| # | Truth | Status | Evidence |
|---|-------|--------|----------|
| 1 | A 1 GiB and a 4 GiB random-write test in Docker keep the daemon peak RSS (VmHWM) below the same fixed cap | ? UNCERTAIN (human) | `scripts/docker-linux-test/bigwrite.sh` has `CAP_MIB=256`, the `bw-rss-bounded-S` and `bw-overlay-used-S` checks, and VmHWM from `/proc/$MP/status`. `run.sh bigwrite` exists (by hand, not CI). The raw 14-07 log (session scratchpad `1407/14-07-check.log`) shows 1 GiB = 114 MiB and 4 GiB = 161 MiB under cap 256, `RESULT bigwrite: PASS`. The 14-03 "before" run was 6224 MiB. The same numbers are committed in `docs/design/chunk-store.md:122-125`. **But** that run was at 37de6ad. 14-09, #107 and Phase 17 changed daemon code after it. No later VmHWM result is recorded (#108 cites bigwrite only for write latency, before Phase 17). |
| 2 | A commit with a large open file and a small change writes about the size of the change to the write log | VERIFIED | `commit_cut` (`filesystem.rs:1246`) takes descriptors before `meta.commit()`, then logs Setattr(visible), Setattr(size) and `Op::Write` pieces of at most `RELOG_PIECE` = 1 MiB, copied one at a time. `a_commit_relogs_only_the_bytes_changed_since_the_last_rebase` asserts 4096 / 8192 / 4096 for a 4 MiB and a 16 MiB file, and that the two sizes give the same values. Passes in CI. |
| 3 | Replay after a crash gives the same content, size and times, for PerOp and FlushOnFsync | VERIFIED | `an_overlay_replays_after_a_crash_with_each_log_mode` loops over `[WalConfig::PerOp, WalConfig::FlushOnFsync] x [RawRecords, RelogRecords, AfterRebase]`. It compares the full stat dump (sizes, mtime, ctime, atime, modes, xattrs, content hash) and the content against a model. `an_overlay_with_more_than_64_mib_of_relog_replays` covers relog > 64 MiB. Both pass in CI. |
| 4 | The commit gate is not held for the hash of a whole file; online GC during a rebase loses no chunk | VERIFIED (with a narrowed reading) | `commit_cut` and `gc_start` contain no rebase and no hash. The rebase runs only in the write (`filesystem.rs:986-993`), setattr, release and destroy callbacks. `rebase_overlay` is a splice (`content::splice`, `filesystem.rs:761`) that cuts again only near the change. Tests: `commit_and_fsync_do_not_rebase_an_open_overlay`, `online_gc_during_a_rebase_keeps_every_chunk`, `gc_start_waits_for_a_rebase_in_a_callback`, `an_open_overlay_keeps_its_base_through_online_gc`. All pass in CI. Reading: the gate is never held **exclusively** for a hash. On long repeating non-zero data, a callback holds it shared during a re-chunk to EOF, so a commit can wait. This reading is documented in `write-log.md:133` and in the approved #98 comment. |
| 5 | After a small change in a large file, the store grows by about the changed chunks only | VERIFIED | `chunk_store::file::splice` (`file.rs:565`) with oracle tests `a_splice_gives_the_same_key_as_a_full_write`, `a_splice_across_many_leaves_gives_the_same_key`, `a_small_splice_reuses_old_nodes`. `a_small_change_adds_only_the_changed_chunks` asserts at most 12 new objects, CAS growth < 1 MiB, and more than 3000 old chunk IDs kept. All pass in CI. |

**Score:** 4/5 truths verified (0 present-but-behavior-unverified; 1 uncertain, routed to human)

### Required Artifacts

| Artifact | Expected | Status | Details |
|----------|----------|--------|---------|
| `crates/slicefs-cli/src/overlay.rs` | Overlay + RangeView, property tests, 1/8 growth | VERIFIED | 377 lines; `GROW_DIV = 8`; `a_growing_extent_does_not_double_its_capacity`; `pub mod overlay` used by filesystem.rs |
| `crates/slicefs-cli/src/filesystem.rs` | Overlay arm at every former full-buffer site, dirty limit, relog pieces, splice rebase | VERIFIED | `WriteMode::Overlay` arms in write, read, commit_cut, flush, setattr, gc_start; `DIRTY_LIMIT = 32 << 20` with the Measured doc line; `OverlaySource` implements `SpliceSource` |
| `crates/chunk-store/src/file.rs` | `splice`, `SpliceSource`, `SpliceStats` | VERIFIED | Lines 351-565; 6 splice tests |
| `crates/slicefs-cli/src/content.rs` | `content::splice` wrapper | VERIFIED | `pub fn splice` at line 242; 2 callers in filesystem.rs (rebase, setattr Case B) |
| `crates/slicefs-cli/tests/memory_bound_tests.rs` | Counting-allocator proof | VERIFIED | `#[global_allocator]`; one test; 12 MiB `PEAK_BOUND`, `SIZE_SLACK` between 16 and 48 MiB |
| `crates/chunk-store/src/retry.rs` | Bounded RAM copy of unsynced pack writes | VERIFIED | `KEEP_LIMIT = 8 << 20` (0285b2a); `the_copy_of_unsynced_writes_stays_below_the_keep_limit` |
| `crates/metadata/src/wal/buffered.rs` + modes (14-09) | Per-op flush, 4 MiB cap, periodic timer, sticky failure | VERIFIED | `BUFFER_CAP_BYTES = 4 MiB`; `with_interval`; 6 `wal::` tests pass in CI |
| `scripts/docker-linux-test/bigwrite.sh`, `run.sh bigwrite` | Docker VmHWM check | VERIFIED (exists, substantive) | Not in CI by design (`run.sh:10`, "by hand, not in CI") |
| `docs/design/write-log.md`, `docs/design/chunk-store.md` | Design notes | VERIFIED | "Overlays (#98, part 2)" (line 90), "Write-log RAM (#98)" (line 240), "Splice (#98)" (line 75) with the Docker table |

### Key Link Verification

| From | To | Via | Status |
|------|----|-----|--------|
| FUSE write/read/setattr/release | `SliceFsFilesystem::test_write` etc. | `handlers.rs:163,187,324,507` | WIRED (the test_* names are the production entry points) |
| `test_write` | `rebase_overlay` | `ov.cost() > dirty_limit` after each write (`filesystem.rs:986`) | WIRED |
| `rebase_overlay` / setattr Case B | `chunk_store::file::splice` | `content::splice` + `OverlaySource` | WIRED |
| `commit_cut` | write log | `log_record` of RELOG_PIECE pieces after `meta.commit()` | WIRED |
| `gc_start` | open overlay bases | `WriteMode::Overlay(ov) => file_roots.extend(ov.base)` (`filesystem.rs:595`) | WIRED |
| `verify-phases.sh` Large copy | switch log text | `switches to overlay` / `no-switch-to-overlay` | WIRED |

Whole-file gate (production region before `#[cfg(test)]`, comment lines skipped): no `Buffered`, `handle_content`, `pwrite_into`, `buf.resize(`, `content.resize(` or `buf.clone()`. `content::get_all` is used only by `read_content` (called from lib tests only) and `simulate_readlink`. `content::put_all` is used only for a symlink target.

### Behavioral Spot-Checks

| Behavior | Command | Result | Status |
|----------|---------|--------|--------|
| Phase-14 tests pass on b4b5494 | `gh run view 44477290116 --log` grep for each named test | each test named in this report has a `... ok` line; 0 `test result: FAILED` | PASS |
| PR #114 CI | `gh pr checks 114` | Linux (Docker) pass, macOS cargo test pass | PASS |
| Docker bigwrite on HEAD | not run (by-hand Docker mode) | - | SKIP (human) |

### Requirements Coverage

| Requirement | Source Plan | Description | Status | Evidence |
|-------------|-------------|-------------|--------|----------|
| BUFR-01 | 14-04, 14-06, 14-08 | A commit with a large open file logs an amount that depends on the bytes changed since the last commit, not on the file size | SATISFIED | `commit_cut` relog of extents in 1 MiB pieces; `a_commit_relogs_only_the_bytes_changed_since_the_last_rebase` (4096/8192/4096, same for 4 and 16 MiB); `an_overlay_with_more_than_64_mib_of_relog_replays`; #93 closed with these tests. Note: the amount is the bytes since the last **rebase** (see tech debt). |
| BUFR-02 | 14-01, 14-06, 14-08 | Replay after a crash gives the same content, size and times (PerOp and FlushOnFsync) | SATISFIED | `an_overlay_replays_after_a_crash_with_each_log_mode` (2 log modes x 3 crash points, full dump compare); 64 MiB relog test; `write_log_replay_tests` (18 tests, per the CI log) pass in CI |
| BUFR-03 | 14-02, 14-04, 14-05, 14-06, 14-08 | The commit gate is not held for the hash of a whole file; online GC during a rebase loses no chunk | SATISFIED | Narrowed reading (see tech debt 5). No rebase or hash in `commit_cut`/`gc_start`/fsync (`commit_and_fsync_do_not_rebase_an_open_overlay`); GC tests `online_gc_during_a_rebase_keeps_every_chunk`, `gc_start_waits_for_a_rebase_in_a_callback`, `an_open_overlay_keeps_its_base_through_online_gc`. "Never held exclusively"; a shared hold can delay a commit on long repeating data |
| BUFR-04 | 14-01, 14-02, 14-03, 14-04, 14-05, 14-07, 14-08 | A random write never loads the whole file into RAM; a 1 GiB and a 4 GiB Docker test stay below the same VmHWM cap | ✓ SATISFIED | Docker re-run on b4b5494 (2026-11-19): `RESULT bigwrite: PASS`, VmHWM 88 MiB (1 GiB) and 134 MiB (4 GiB), cap 256 MiB. Grep gates clean; `memory_bound_tests` (LIMIT 1 MiB, PEAK_BOUND 12 MiB, SIZE_SLACK 2 MiB between 16 and 48 MiB files; green in CI on HEAD); retry buffer bounded by `KEEP_LIMIT`; Docker 114 / 161 MiB under cap 256 (vs 6224 MiB before) at 37de6ad only |
| BUFR-05 | 14-02, 14-05, 14-08 | After a small change in a large file, the store grows by about the changed chunks only | SATISFIED | `a_small_change_adds_only_the_changed_chunks` (<= 12 new objects, < 1 MiB growth, > 3000 old IDs kept); splice oracle tests; `an_extend_without_a_handle_rechunks_little` |

Orphaned requirements: none. REQUIREMENTS.md maps BUFR-01..05 to Phase 14, and each is claimed by at least one plan. 14-09 declares `requirements: []` (gap closure for write-log RAM).

Issues: #98 and #93 are CLOSED (completed). Their evidence comments cite the tests above. #98 names follow-ups #105 and #106 (both OPEN). A later #98 comment links the slow-write cause to #107 (merged) and #108 (OPEN).

### Anti-Patterns Found

| File | Line | Pattern | Severity | Impact |
|------|------|---------|----------|--------|
| (phase files) | - | TBD / FIXME / XXX | none found | - |
| `.planning/phases/14-buffered-delta-re-log/deferred-items.md` | - | Says the retry buffer is open | Info | Out of date: 0285b2a fixed it (`KEEP_LIMIT`, `retry_tests`) |

### Human Verification Required

#### 1. Docker bigwrite on the current tree

**Test:** `scripts/docker-linux-test/run.sh bigwrite 1024 4096` on b4b5494 or later (Linux Docker, by hand).
**Expected:** `bw-rss-bounded-1024: ok`, `bw-rss-bounded-4096: ok`, all cmp steps ok, `RESULT bigwrite: PASS` with `CAP_MIB=256`.
**Why human:** The only run was at 37de6ad. After it, 31 commits changed daemon code (14-09, #107, Phase 17 on-disk index). The run is not in CI, no later VmHWM result is recorded, and the raw logs are only in a session scratch folder.

### Non-critical Gaps / Tech Debt

1. **Per-file limit only (#105, OPEN).** N open files can hold N x max(DIRTY_LIMIT 32 MiB, REORDER_WINDOW 64 MiB).
2. **Replay loads all pending records into RAM (#106, OPEN).** Bounded by the commit interval, not by a fixed cap. With `--commit-interval 0` it can grow until unmount.
3. **Slow write at the dirty limit (#108, OPEN).** The write that passes the limit runs the whole rebase on the FUSE thread (5-8 s seen in Docker under load). #107 reduced the base reads.
4. **BUFR-01 wording.** A commit relogs the bytes changed since the last **rebase**, not since the last commit. Between rebases, each commit can log up to about DIRTY_LIMIT again. This does not depend on file size, so the requirement intent holds.
5. **BUFR-03 narrowed reading.** On long repeating non-zero data, the splice can re-chunk to EOF while a callback holds the gate shared. A timed commit or `gc_start` then waits. Memory stays bounded.
6. **Docker evidence is not durable or regression-guarded.** bigwrite is not in CI. The raw logs are only in a session scratch folder; only the summary table is committed in `chunk-store.md`. The VmHWM reading excludes the final commit at unmount, and the workload uses only the per-op log with GC off.
7. **Stale planning text.** `deferred-items.md` and the 14-07 "Open items" still list the chunk-store retry buffer as open. 0285b2a fixed it.

### Gaps Summary

No blocking gap. All five requirements have code and passing tests on b4b5494. The in-process memory proof (`memory_bound_tests`) is green in CI on HEAD. One item needs a human: the Docker VmHWM check (success criterion 1). It passed at 37de6ad, but daemon code changed after that, and no later VmHWM result is recorded.

---

_Verified: 2026-11-19T07:25:06Z_
_Verifier: Claude (gsd-verifier)_

## Re-verification 2026-11-19 (orchestrator)

The human item (Docker `bigwrite` on current code) was run on b4b5494 with `scripts/docker-linux-test/run.sh bigwrite 1024 4096`:

- 1024 MiB: VmHWM after cp 28, after random write 88, after cmp 88 MiB (cap 256 MiB); max write 3361 ms, max fsync 181 ms; overlay switches 1.
- 4096 MiB: VmHWM after cp 67, after random write 134, after cmp 133 MiB (cap 256 MiB); max write 2751 ms, max fsync 51 ms; overlay switches 1.
- `RESULT bigwrite: PASS`. BUFR-04 is satisfied on the current tree. Status changed to passed.
