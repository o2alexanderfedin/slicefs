---
phase: 17-on-disk-chunk-index
plan: 02
subsystem: chunk-store index
tags: [index, lsm, run-file, pack-io, format]
status: complete

requires: [17-01]
provides:
  - crates/chunk-store/src/run.rs (run file format; RunWriter, Run::open, Run::get, Run::iter; Entry::Put / Entry::Tomb)
  - Kind::Run and Kind::Manifest; PackIo list_runs, create_run, open_run on FileIo, MemIo and RetryingIo
  - FileIo keeps index files in STORE/index (pack_dir.with_file_name("index"))
affects: [17-03, 17-04, 17-05, 17-06, 17-07]

actuals:
  tokens: 11300
  tasks: 2
  commits: 2
plan_head_before: c2bfef0c52ad5c10f678298b73c10b3d910bae39
plan_head_after: f11b4ccfb10ffda8bb88996418a50adc4b4e6692

tech-stack:
  added: []
  patterns:
    - run files streamed through PackFile (write_at, sync every 64 MiB and at the end), not write_durable
    - every footer value checked as an exact layout equality with checked arithmetic before any read or allocation
    - fences kept in RAM as big-endian u64 of digest bytes 0..8 (numeric order is byte order)

key-files:
  created:
    - crates/chunk-store/src/run.rs
  modified:
    - crates/chunk-store/src/pack_io.rs
    - crates/chunk-store/src/mem.rs
    - crates/chunk-store/src/retry.rs
    - crates/chunk-store/src/lib.rs

key-decisions:
  - "A run holds one entry per ID (RunWriter refuses keys that are not strictly ascending); this matches the newest-wins-per-ID rule of 17-03 and 17-06"
  - "A run with no put entries stores smallest pack u64::MAX and largest pack 0; Run::pack_range returns None for it"
  - "A short read of a run file (UnexpectedEof) is Error::Corrupt; any other read error stays Error::Io, so 17-06 can tell damage from a transient error"
  - "Run::open also refuses: fences out of order inside a tag, tags out of order, tag page ranges that are not contiguous, filter type 0 with a filter, any unknown filter type; Run::read_page also checks that the reserved and padding bytes are zero (the CRC covers only the entry bytes)"
  - "Run::get steps back while the fence equals the key prefix (a loop, not only one page), so any number of keys with one 8-byte prefix is found"

duration: not measured (task commits 15:25 and 15:28 local time)
completed: 2026-11-18
---

# Phase 17 Plan 02: run file format Summary

**Run files of 58-byte entries in 4 KiB pages, with a tag table, an 8-byte fence table and a 112-byte footer (magic SFRN, CRCs), written through PackIo::create_run and found again through open_run with one page read; open reads only the footer and the two tables.**

## What was built

- `pack_io.rs`: `Kind::Run` (`{num:016x}.run`) and `Kind::Manifest` (`MANIFEST`, num 0). FileIo routes both to `STORE/index` through one path helper, so `read`, `write_durable`, `remove` and `exists` work for them. `prepare(false)` makes both directories; `prepare(true)` answers for the pack directory only. `sync_dir` also syncs the index directory if it exists. New trait methods without default bodies: `list_runs` (empty if there is no index directory), `create_run`, `open_run`. `list`, `create`, `open_read`, `open_rw` and `FileIo::new` did not change.
- `mem.rs`: `handle(num, kind)` and a shared `create_new(num, kind)`; run files use the same durable model as packs (bytes after `sync`, the name after `sync_dir`), so `crash`, `flip`, `fail_next_syncs` and the read faults also apply to runs.
- `retry.rs`: `list_runs` and `open_run` are forwarded; `create_run` is wrapped in `RetryingPack` like `create`. The KEEP_LIMIT (8 MiB) sync is unchanged.
- `run.rs` (`pub(crate)`, `mod run` has `#[allow(dead_code)]` until 17-03): the format in the module comment, `RunWriter::create(io, num, expected)` / `push` / `finish -> RunInfo { len, entries, tombstones }`, `Run::open(handle, num, len)`, `Run::get(id) -> Result<Option<Entry>>`, `Run::iter()` (one page at a time; it also checks ascending IDs and that the page counts add up to the footer count).
- `lib.rs`: `mod run`; `Loc` derives `PartialEq, Eq` (for the entry type and the tests).

## Task Commits

| Task | Name | Commit | Files |
| ---- | ---- | ------ | ----- |
| 1 | Tracer: a run file written through PackIo is found again with one page read | 27f7ef2 | pack_io.rs, mem.rs, retry.rs, run.rs, lib.rs |
| 2 | Run format edge cases and damage | f11b4cc | run.rs |

## Tests

- `cargo test -q --locked -p chunk-store --lib run::`: 8 passed (`a_run_round_trips_through_each_io`, `runs_of_each_size_round_trip`, `each_tag_starts_a_new_page`, `keys_with_the_prefix_of_a_page_fence_are_found`, `tombstones_round_trip_and_are_counted`, `the_writer_refuses_keys_out_of_order`, `a_damaged_footer_or_table_is_refused`, `a_damaged_page_is_corrupt_for_get_and_iter`).
- `cargo test -q --locked -p chunk-store`: every suite passed (lib 25, then 10, 4, 5, 10, 9, 6, 11).
- The tracer test checks, on MemIo, FileIo (temp dir) and RetryingIo: each of 1,000 keys is found with exactly one `read_at`; a miss does at most one read; a tag that is not in the run does no read; `Run::open` reads nothing below `pages * 4096`; the FileIo run file is `TEMPDIR/index/0000000000000001.run` and `list_runs` returns `[1]`.
- `cargo clippy -p chunk-store --all-targets`: no warning in the changed files (two older warnings in lib.rs and tests/file_tests.rs are not from this plan).

## TDD Gate Compliance

Task 2 (`tdd="true"`): the tests were written after the Task 1 code, which already follows the format, so they passed on the first run. There is no RED commit, because a RED commit with passing tests would be false. To show that the tests can fail, two code mutations were run and reverted: without the step-back to the previous page, `keys_with_the_prefix_of_a_page_fence_are_found` failed; without the zero-byte check of a page, `a_damaged_page_is_corrupt_for_get_and_iter` failed.

## Acceptance checks

- `Kind::Run` in pack_io.rs: 4; run methods in retry.rs: 3; `SFRN` in run.rs: 2; the tracer test name: 1; `#[test]` in run.rs: 8.
- `const FORMAT_VERSION: u16 = 2;` is unchanged.
- `git diff --quiet origin/develop...HEAD -- crates/chunk-store/src/file.rs crates/slicefs-cli/src/content.rs`: exit 0 (file.rs is not touched).
- No `fjall` in Cargo.toml, Cargo.lock or crates/*/Cargo.toml.

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 2 - Missing critical functionality] More checks in Run::open and the page read than the plan lists**
- **Found during:** Task 1
- **Issue:** The page CRC covers only the entry bytes, so a flipped reserved or padding byte would pass. Also, `get` indexes the fence table through the tag table, so a tag table that is not contiguous could index out of range.
- **Fix:** The reserved and padding bytes of a page must be zero. Run::open checks exact block positions, a contiguous tag table that covers every page, fences in order inside each tag, the entry, tombstone and pack-range counts, and the filter type and length.
- **Files modified:** crates/chunk-store/src/run.rs
- **Commit:** 27f7ef2

**2. [Rule 2] `Loc` derives `PartialEq, Eq`**
- **Found during:** Task 1
- **Issue:** The run `Entry` and the tests compare entries.
- **Fix:** Added the two derives in lib.rs (no format change).
- **Commit:** 27f7ef2

**3. [Process] Commits on `feature/17-on-disk-index`, not on an `agent-*` branch**
- The launch message said to switch to this branch. The stock worktree branch-name check expects `agent-*`. This branch is not a protected branch, so the commits were made here. `git symbolic-ref HEAD` gives `refs/heads/feature/17-on-disk-index` (HEAD is not detached). The spawn branch `worktree-agent-aaa54b9d52fcc82c9` has no commit of this plan.

## Not covered by these tests

- Only `-p chunk-store` was built and tested. Two changes are visible outside the crate: every writable FileIo open now makes `STORE/index`, and `FileIo::sync_dir` syncs two directories.
- The 64 MiB sync of `RunWriter` and a failed run sync with a non-empty backoff are not exercised (the tracer uses an empty backoff list, as the plan says).

## Known Stubs

- The filter block is empty (filter type 0) by design; 17-05 adds the blocked Bloom filter (type 1). `RunWriter::create` already takes the expected entry count for it.

## Threat Flags

None. T-17-23 is covered by the checks above and by `a_damaged_footer_or_table_is_refused` and `a_damaged_page_is_corrupt_for_get_and_iter`. T-17-SC: no dependency change.

## Self-Check: PASSED

- FOUND: crates/chunk-store/src/run.rs
- FOUND: 27f7ef2, f11b4cc (ancestors of HEAD)
