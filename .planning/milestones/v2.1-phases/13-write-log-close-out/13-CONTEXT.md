# Phase 13: Write log close-out - Context

**Gathered:** 2026-11-18
**Status:** Ready for planning

<domain>
## Phase Boundary

Close #20 with evidence and no change to the write-log design. Map each #20 acceptance item to a passing test, or point it to a follow-up issue. Add the few missing tests: an end-to-end torn last record, a rename made durable only by fsync of the directory, and a large copy with a daemon RSS bound in Docker. Restate the FUSE-T NFS item for FSKit (by hand) and libfuse in Docker. Point the commit-cost item to #93 (Phase 14), then close #20 with a comment that lists the tests.

</domain>

<decisions>
## Implementation Decisions


### Decided autonomously
- [kill -9 / fsynced file survives] Q: "Is 'a file survives after its fsync returns' already covered?" → Selected: "Yes, at two levels. In process (simulated crash, drop without destroy, then reload_and_replay): crates/slicefs-cli/tests/write_log_replay_tests.rs files_written_and_fsynced_survive_a_crash (l.127), a_file_still_open_at_the_crash_keeps_its_fsynced_bytes (l.177), fsynced_buffered_bytes_survive_a_commit_and_a_crash (l.331); crash_recovery_tests.rs test_sc2_fsync_guarantees_durability (l.113), test_sc4_multiple_fsync_cycles_all_survive; fsync_tests.rs test_fsync_crash_durability (l.103). Real kill -9 of the daemon on libfuse: scripts/docker-linux-test/verify-phases.sh Phase 05b step 'fsynced-file-survives' (--commit-interval 0, so only the write log keeps the data) and Phase 05 'closed-file-survives-crash'. No new test needed. Cite these." — Only the evidence list changes.
- [kill -9 / rename after directory fsync] Q: "Is 'a rename survives after fsync of the directory' covered?" → Selected: "Partly. verify-phases.sh Phase 05b 'renamed-file-survives' makes the rename durable through a later fsync of another file (c.bin, l.83), not through fsync of the directory. directories_renames_links_and_unlinks_replay (write_log_replay_tests.rs l.140) uses the PerOp WAL, where every record is durable at once. wal_durability_tests.rs fsync_of_a_directory_syncs_the_write_log (l.50) only counts syncs. Add two tests. (a) In process, in write_log_replay_tests.rs: use new_fs_with(store, WalConfig::FlushOnFsync), rename, call h::handle_fsyncdir, drop without commit, reload_and_replay, check that the new name is there and the old name is gone. First check that the FlushOnFsync WAL has no Drop flush that would make this test pass for the wrong reason. (b) In Docker, in verify-phases.sh Phase 05b: mv a file, then `sync $R/m5b/dir` (coreutils 9 `sync FILE` fsyncs that path, and the path can be a directory) with no fsync of a file after it, then kill -9, remount, and check." — Additive tests only.
- [kill -9 / torn last record] Q: "Is 'a torn last record does not stop the mount' covered?" → Selected: "Only at the component level. crates/metadata/tests/segment_frame_tests.rs a_record_cut_in_the_middle_is_ignored (l.39), a_changed_byte_stops_the_read_at_that_record (l.49); segment_tests.rs l.72 (truncate mid-record); chunk-store store_tests.rs a_torn_tail_of_the_active_pack_is_cut_and_the_rest_kept (l.128); mount_lifecycle_tests.rs test_load_store_truncated_segment_skipped (l.164) covers only a segment shorter than 16 bytes. There is no end-to-end test. Add one to write_log_replay_tests.rs: do some operations after the commit, fsync, drop, then cut the last bytes of the newest file in <store>/segments with set_len(len-3), the same as segment_frame_tests l.44. Then reload_and_replay (load_store_with_log in crates/metadata/src/segment/mod.rs:114, then oplog::replay in oplog.rs:218) must return Ok, keep every record before the cut, and accept new writes. Optionally add the same check to Docker Phase 05b: after kill -9, truncate the newest segment file by a few bytes, then remount." — Test-only; the reader already stops at the torn tail.
- [Large copy / bounded memory (libfuse, Docker)] Q: "How do we restate '786 MB over FUSE-T NFS' and check that memory is bounded on Linux?" → Selected: "Add a new section to scripts/docker-linux-test/verify-phases.sh (run with `run.sh verify`). Make a 1 GiB file from /dev/urandom, cp it into a mount with the default commit interval, and also write it in reverse 1 MiB blocks with `dd seek=N conv=notrunc` to get out-of-order writes. During the copy, read the daemon's peak RSS from /proc/$MP/status VmHWM. Require cmp to pass after the copy and after a remount, and require VmHWM below a fixed cap. Proposed cap: 512 MiB. REORDER_WINDOW is 64 MiB (crates/slicefs-cli/src/filesystem.rs:41), so the cap is about 8x that window and well under the file size. Print the measured value. Also record `docker stats --no-stream` in the evidence." — The script and the cap value are easy to change.
- [Large copy / bounded memory] CHANGED 2026-11-18 at planning: a literal reverse pass (one dd per 1 MiB block, or a full reverse on one handle) switches the handle to Buffered mode and holds the whole file in RAM (filesystem.rs ~733-796), so no cap below the file size can hold. Instead: one python3 process, one handle, 1 MiB blocks reversed inside each 16 MiB group (all jumps stay inside the 64 MiB REORDER_WINDOW), fsync every 64 MiB, and the check fails if any handle switches to Buffered. The cap is set from a measured VmHWM (512 MiB if peak <= 341 MiB, else 1.5x peak rounded up to 64 MiB if <= 768 MiB, else stop). See 13-02-PLAN.md.
- [Large copy (FSKit, macOS, by hand)] Q: "How is the FSKit side checked without an automated macOS mount?" → Selected: "This is a manual step only, and no automated macOS mount is added. Add a short section to scripts/macos-mount-test/README.md: mount with the FSKit backend, copy a file of 1 GiB or more, sample `ps -o rss= -p <daemon pid>` in a loop and keep the peak, then cmp the file after the copy and after a remount. The existing boost-test.sh fskit run in .github/workflows/macos-mounts.yml covers the many-small-files case. In the #20 comment, record the manual result (size, peak RSS, commit)." — Documentation and manual evidence only.
- [Commit time item] Q: "What do we do with 'commit time proportional to changed data'?" → Selected: "Point it to #93 (Phase 14) in the #20 close-out comment. Mention the partial evidence that exists now: metadata node_crash_tests.rs a_small_change_stores_little_at_the_next_commit (l.57), fsync_tests.rs fsync_time_on_buffered_handle_does_not_grow_with_file_size (l.154), and metadata/tests/commit_bench.rs. The open gap is the re-log of a large open Buffered file at a commit, which is #93." — It only changes the text of a comment.
- [Issue #20 comment item (objects durable before root)] Q: "Is the second #20 comment (fsync objects before RootUpdate) covered?" → Selected: "Yes. crates/metadata/tests/write_log_tests.rs a_commit_makes_its_objects_durable_before_the_root (l.81) and node_crash_tests.rs a_failed_node_sync_fails_the_commit (l.44) cover it. List both in the close-out comment." — Only the evidence list changes.
- [Wording of 'kill -9' in-process tests] Q: "Do in-process drop-without-destroy tests count as kill -9 evidence?" → Selected: "Use both kinds and label them in the comment. In-process tests (simulated crash) run in `cargo test` on macOS. The real kill -9 checks in Docker (verify-phases.sh Phase 05/05b) are the direct evidence for the kill -9 wording." — Only the labels in the comment change.

### Claude's Discretion
- Test names, helper layout and plan split are at Claude's discretion.

</decisions>

<code_context>
## Existing Code Insights

### Reusable Assets
- crates/slicefs-cli/tests/write_log_replay_tests.rs helpers: new_fs_with(store, WalConfig) l.27, reload_and_replay l.39, dump l.52, check(scenario) l.112, create/write/fsync/release
- crates/metadata/tests/segment_frame_tests.rs torn-tail technique (set_len(ends[i]-3)) l.44
- crates/slicefs-cli/tests/wal_durability_tests.rs ProbeWal/Probe (sync counting, injected failures)
- crates/slicefs-cli/src/handlers.rs handle_fsync l.214, handle_fsyncdir l.230
- crates/metadata/src/segment/mod.rs load_store_with_log l.114; crates/slicefs-cli/src/oplog.rs replay l.218
- scripts/docker-linux-test/verify-phases.sh helpers mnt/umnt/step/ok/bad, Phase 05 and 05b real kill -9 sections (l.50-94)
- scripts/docker-linux-test/run.sh verify (OrbStack/Docker, private mount namespace, 45 s container-age wait)
- scripts/macos-mount-test/boost-test.sh fskit + README.md (manual FSKit runs)
- crates/slicefs-cli/tests/small_file_bench.rs RSS sampling via ps (pattern)

### Established Patterns
- Simulated crash = drop SliceFsFilesystem without destroy/commit, then reload_and_replay and compare dump() maps
- Durability tests use --commit-interval 0 in Docker so only the write log can keep data
- Docker steps wrap each check in step <name> <timeout> <cmd> with timeout -s KILL; daemon PID in $MP
- Test names are full sentences in snake_case (e.g. a_torn_tail_of_the_active_pack_is_cut_and_the_rest_kept)
- Module doc comments reference the issue number (#20)

### Integration Points
- handle_fsyncdir -> WAL flush_and_sync (FlushOnFsync mode is where the directory-fsync test means something)
- Segment reader stops at the first bad CRC or sequence number -> load_store_with_log -> oplog::replay at mount
- REORDER_WINDOW (filesystem.rs:41, 64 MiB) bounds pending out-of-order bytes, which sets the expected daemon RSS
- verify-phases.sh runs only through scripts/docker-linux-test/run.sh verify (not in cargo test)
-  #20 close-out comment links #93 (Phase 14)

### Files likely modified
- crates/slicefs-cli/tests/write_log_replay_tests.rs
- scripts/docker-linux-test/verify-phases.sh
- scripts/macos-mount-test/README.md

### Risks
- The in-process FlushOnFsync rename test can pass for the wrong reason if the WAL flushes its buffer on Drop. Check this first in crates/metadata/src/wal/ (the FlushOnFsync implementation). If it does flush on Drop, use mem::forget or a ProbeWal-style double to simulate the crash.
- Docker `sync <dir>` must really call fsyncdir. coreutils 9.1 in bookworm calls fsync on the path, and FUSE sends FSYNCDIR. Check this in the Docker run (for example with a Replayed count, or a log line).
- The VmHWM cap (512 MiB) is a guess based on REORDER_WINDOW of 64 MiB plus caches. Measure once before you set the cap, so the check does not fail at random.
- The OrbStack startup quirk: verify-phases.sh needs the container to be 45 s old before the first mount. run.sh already does this wait. A long new section makes the verify run longer.

</code_context>

<specifics>
## Specific Ideas

EXISTING TESTS, by #20 acceptance item:
(1) The file survives after fsync. COVERED. In process: write_log_replay_tests.rs files_written_and_fsynced_survive_a_crash, a_file_still_open_at_the_crash_keeps_its_fsynced_bytes, fsynced_buffered_bytes_survive_a_commit_and_a_crash; crash_recovery_tests.rs test_sc2/test_sc4; fsync_tests.rs test_fsync_crash_durability. Real kill -9: verify-phases.sh Phase 05b fsynced-file-survives, Phase 05 closed-file-survives-crash.
(2) The rename survives after fsync of the directory. PARTIAL. Phase 05b renamed-file-survives makes the rename durable through a later fsync of another file. directories_renames_links_and_unlinks_replay runs on PerOp. fsync_of_a_directory_syncs_the_write_log only counts syncs. MISSING: (a) an in-process test with FlushOnFsync + rename + handle_fsyncdir + crash + replay; (b) a Docker step that uses `sync <dir>` as the only sync before kill -9.
(3) A torn last record does not stop the mount. COMPONENT ONLY: segment_frame_tests.rs (cut, changed byte, out of sequence), segment_tests.rs l.72, chunk-store store_tests.rs torn pack tail, mount_lifecycle_tests.rs truncated segment under 16 bytes. MISSING: an end-to-end test that truncates the newest file in <store>/segments, then requires reload_and_replay to succeed with the earlier records intact. Optional: a Docker variant after kill -9.
(4) The 786 MB copy over FUSE-T NFS. OBSOLETE BACKEND, restate it. Linux: a new verify-phases.sh section copies 1 GiB in order and in reverse 1 MiB blocks (dd seek conv=notrunc), checks with cmp before and after a remount, and requires /proc/$MP/status VmHWM below 512 MiB. macOS FSKit: a manual check of the same copy with peak `ps -o rss=` sampling, written up in scripts/macos-mount-test/README.md, with the result in the #20 comment. MISSING today: no large-file RSS check exists anywhere (Phase 05 writes 64 MB but does not measure memory).
(5) Commit time proportional to the changed data. Goes to #93 (Phase 14). Partial evidence: node_crash_tests.rs a_small_change_stores_little_at_the_next_commit, fsync_tests.rs fsync_time_on_buffered_handle_does_not_grow_with_file_size, metadata/tests/commit_bench.rs.
(6) Comment item: objects are durable before the root. COVERED: write_log_tests.rs a_commit_makes_its_objects_durable_before_the_root, node_crash_tests.rs a_failed_node_sync_fails_the_commit.
For a map-reduce plan: the cargo-test additions (crates/slicefs-cli/tests/write_log_replay_tests.rs) and the Docker script additions (verify-phases.sh) are independent. Do them in parallel, run `cargo test -p slicefs-cli --test write_log_replay_tests` once, and run `scripts/docker-linux-test/run.sh verify` once at the end.

Project rules: no macOS mounts in automated tests; use targeted cargo commands in worktrees and one full test run per merge; no store backward compatibility; FSKit is the only macOS backend.

</specifics>

<deferred>
## Deferred Ideas

The commit cost of a large open Buffered file is #93 (Phase 14). No automated macOS mount tests (FSKit is by hand only). No real-process kill -9 harness inside cargo test (the Docker script covers real kill -9). Power-loss tests (F_FULLFSYNC and drive cache) are out of scope.

</deferred>
