All acceptance items of this issue now map to a test that passes, or to a follow-up issue. I close this issue.

**Two kinds of evidence**
- *Simulated crash*: a test inside the process. It drops the file system with no commit and no unmount. Then it loads the last root and replays the write log, as a mount does. These tests run in `cargo test` on Linux and macOS.
- *Real `kill -9`*: a check on Linux (libfuse) in Docker, in `scripts/docker-linux-test/verify-phases.sh`. CI runs it in the step "Live checks" (`scripts/docker-linux-test/run.sh verify`). The script stops the daemon with `kill -9`, then it mounts the store again.

**1. A file survives `kill -9` after its `fsync` returns. Done.**
- Real `kill -9`: Phase 05b `fsynced-file-survives`. The mount uses `--commit-interval 0`, so only the write log keeps the data. Also Phase 05 `closed-file-survives-crash`.
- Simulated crash: `crates/slicefs-cli/tests/write_log_replay_tests.rs`: `files_written_and_fsynced_survive_a_crash`, `a_file_still_open_at_the_crash_keeps_its_fsynced_bytes`, `fsynced_buffered_bytes_survive_a_commit_and_a_crash`. `crates/slicefs-cli/tests/crash_recovery_tests.rs`: `test_sc2_fsync_guarantees_durability`, `test_sc4_multiple_fsync_cycles_all_survive`. `crates/slicefs-cli/tests/fsync_tests.rs`: `test_fsync_crash_durability`.

**2. A rename survives `kill -9` after `fsync` of the directory. Done.**
- Real `kill -9`: Phase 05c `rename-survives-dir-fsync` and `old-name-gone-after-dir-fsync`. The mount uses `--commit-interval 0`. The check writes a file with `fsync`, renames it, and runs `sync` on the directory (an `fsync` of the directory). No other sync comes after the rename. Then it stops the daemon with `kill -9`. In the same run, a second rename with no sync after it is lost (the script prints this). Thus the `fsync` of the directory is what keeps the first rename.
- Simulated crash: `write_log_replay_tests.rs`: `a_rename_survives_a_crash_after_fsync_of_the_directory`. It uses the write log mode `--wal-strategy flush-on-fsync`, which keeps the records in RAM until a sync. The control test `a_rename_with_no_sync_after_it_is_lost_with_a_buffered_log` shows that the rename is lost without the `fsync` of the directory.
- Unit test: `crates/slicefs-cli/tests/wal_durability_tests.rs`: `fsync_of_a_directory_syncs_the_write_log`.

**3. A torn last record does not stop the mount. Done.**
- Real `kill -9`: Phase 05d. After `kill -9`, the check cuts the last 3 bytes of the newest log segment. The store mounts (`mount-with-torn-last-record`). The records before the cut stay (`records-before-the-tear-kept`). The cut record is not applied (`torn-record-not-applied`). New writes work (`write-after-torn-tail`), and `slicefs scrub` passes (`scrub-after-torn-tail`).
- Simulated crash: `write_log_replay_tests.rs`: `a_torn_last_record_does_not_stop_the_mount`. It also writes a new file after the replay and reads it back after one more crash.
- Component tests: `crates/metadata/tests/segment_frame_tests.rs`: `a_record_cut_in_the_middle_is_ignored`, `a_changed_byte_stops_the_read_at_that_record`. `crates/metadata/tests/segment_tests.rs`: `test_truncated_record_is_skipped`. `crates/chunk-store/tests/store_tests.rs`: `a_torn_tail_of_the_active_pack_is_cut_and_the_rest_kept`. `crates/slicefs-cli/tests/mount_lifecycle_tests.rs`: `test_load_store_truncated_segment_skipped`.

**4. A large copy keeps the daemon memory bounded (was: 786 MB over FUSE-T NFS). Restated. Done.**
SliceFS does not have the NFS backend now. On macOS it uses only the FSKit backend. On the old NFS backend, the copy of the 786 MB Boost tar took 228 s with a daemon peak RSS of 43 MB (see #2). The check is now:
- Linux (libfuse, Docker, debug build): the section "Large copy" in `verify-phases.sh`. It makes a 1 GiB random file and copies it with `cp` (in order). Then one process writes the file again on one handle, in 1 MiB blocks, in reverse order inside each 16 MiB group. These are out-of-order writes inside the 64 MiB reorder window, as an NFS client can send them. The process calls `fsync` after each 64 MiB, as the NFS COMMIT calls did. The mount uses the default commit interval (30 s). `cmp` passes after the copy and after a remount. No handle switches to Buffered mode. Daemon peak RSS (`VmHWM`): 84 MiB after `cp`, 112 MiB after the `cmp` steps (before the unmount). An earlier run of the same check measured 130 MiB. The check fails above 512 MiB. `docker stats` for the container: 114.8 MiB at the peak.
- macOS (FSKit, by hand; the steps are in `scripts/macos-mount-test/README.md`): a {{FSKIT_SIZE}} file, daemon peak RSS {{FSKIT_PEAK_RSS_MIB}} MiB (from `ps -o rss=` each second), `cmp` {{FSKIT_CMP}} after the copy and after a remount, commit {{FSKIT_COMMIT}}.
- Many small files on FSKit: the Boost round trip in `.github/workflows/macos-mounts.yml`.

Limit of the bound: it holds for writes in order, and for out-of-order writes inside the 64 MiB reorder window. A write before the stream position, an overlap, or a gap larger than the window still puts the handle in Buffered mode, which keeps the whole file in RAM. Part 2 of the design (see the last section) removes Buffered mode.

**5. The time of a commit is proportional to the changed data. Moved to #93.**
The open part: a commit writes the full buffer of each open Buffered file to the write log again. #93 tracks this. Partial evidence now: `crates/metadata/tests/node_crash_tests.rs`: `a_small_change_stores_little_at_the_next_commit`. `fsync_tests.rs`: `fsync_time_on_buffered_handle_does_not_grow_with_file_size`. `crates/metadata/tests/commit_bench.rs` (a benchmark that you run by hand).

**Item from the second comment: the objects of a root are durable before the root. Done.**
`crates/metadata/tests/write_log_tests.rs`: `a_commit_makes_its_objects_durable_before_the_root`. `node_crash_tests.rs`: `a_failed_node_sync_fails_the_commit`.

**Not in the acceptance list**
Part 2 of the design (overlays, re-chunk at commit, no Buffered mode) is not done. It continues in #98; see also #21 and #22.

Evidence: merge commit {{MERGE_SHA}}, CI run {{CI_RUN_URL}}.
