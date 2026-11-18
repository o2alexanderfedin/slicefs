Fixed in #103 (merge 75a9743, CI: https://github.com/o2alexanderfedin/slicefs/actions/runs/44411885166). A random write no longer loads the file into RAM, at any place in the daemon.

What changed:
- A handle that gets a random write is an overlay on the last stored content. The base stays in the chunk store and is in the manifest at once. Only the written bytes are in RAM.
- When the written bytes of a file reach the dirty limit (32 MiB per open file), and at close, the file is chunked again only near the changed bytes. The new chunk references go into the manifest tree (`splice` in `crates/chunk-store/src/file.rs`). Unchanged chunks and unchanged manifest nodes keep their IDs.
- A commit logs only the bytes changed since the last rebase, in records of at most 1 MiB (#93).
- fsync does not chunk the file again. The write log makes the bytes durable.
- Online GC cannot start during a rebase (the rebase holds the commit gate shared), and the base of each open file is a GC root.
- The commit gate is never held exclusively (timed commit, GC start) for a hash of file content. A rebase holds the gate shared, as every request does. So a rebase over long repeating data that is not zeros (chunked again to the end of the file) can make a timed commit or a GC start wait until that request ends.
- Write log RAM. In the per-op mode (the default), each operation record goes into the segment file at once (one write into the kernel, no disk sync). Thus a completed request survives a crash of the daemon. The flush-on-fsync and periodic modes keep at most 4 MiB of records in RAM (`BUFFER_CAP_BYTES`). Above that, they write the records to the segment file with no disk sync. The periodic mode now syncs on its timer. If a write or a sync of the log fails in these two modes, every later fsync fails until the next commit. The number of disk syncs does not change.
- After a crash with no fsync, the flush-on-fsync and periodic modes can give back a file that is shorter than written: a correct first part of the data (for example 4 MiB of 6 MiB). Before, such a file did not come back at all.
- The chunk store keeps a RAM copy of pack writes that are not synced yet, so that it can write them again after a failed sync. This copy is now at most 8 MiB (`KEEP_LIMIT` in `crates/chunk-store/src/retry.rs`). Before, it could hold up to 64 MiB until the next commit.

Evidence:
- Docker, Linux libfuse, release build, per-op log, online GC off: 512 MiB of random writes (1 MiB and 4 KiB blocks) on a file copied in with cp. Daemon peak RSS (VmHWM): before this change 6224 MiB for a 1 GiB file. With this change 114 MiB for 1 GiB and 161 MiB for 4 GiB. Cap: 256 MiB for both sizes. cmp passes after the writes and after a remount. Run: `scripts/docker-linux-test/run.sh bigwrite` (sizes 1024 and 4096 MiB).
- Peak heap in the process above the start value, dirty limit 1 MiB: 8353 KiB (random, 16 MiB file), 9272 KiB (random, 48 MiB), 7279 KiB (repeating, 16 MiB), 7779 KiB (repeating, 48 MiB). Bound: 12 MiB. A version that holds the whole file in RAM fails all four cases. Test: `an_open_file_uses_memory_bounded_by_the_dirty_limit_not_the_file_size`.
- A 4 KiB change in a 32 MiB file adds 5 objects and chunks 38,355 bytes again. Tests: `a_small_change_adds_only_the_changed_chunks`, `a_rebase_rechunks_only_near_the_change`.
- A size change from 1 MiB to 4 GiB with no open handle chunks 35,628 bytes again, in 0.14 s (before: 39 s). Test: `an_extend_without_a_handle_rechunks_little`.
- Replay after a crash gives the same content, size and times with the per-op and the flush-on-fsync log: `an_overlay_replays_after_a_crash_with_each_log_mode`.
- Online GC during a rebase loses no chunk: `online_gc_during_a_rebase_keeps_every_chunk`, `gc_start_waits_for_a_rebase_in_a_callback`.
- The splice gives the same key as a full write of the same bytes: `a_splice_gives_the_same_key_as_a_full_write`.
- Write log RAM: `op_record_reaches_the_file_without_a_sync` (per-op), `ram_stays_at_most_the_cap_with_no_fsync` (flush-on-fsync and periodic), `timer_syncs_and_drop_stops_it` (periodic), `failed_cap_write_out_fails_fsync_until_rotate` and `failed_timer_sync_fails_fsync_until_rotate` (a failure stays until the next commit).

Open point: in the Docker random-write run, the slowest single write takes 5 to 8 s (before this change: 17 s). We do not know the cause yet. We are looking into it, and it will get a separate fix.

Limits, in new issues:
- #{{ISSUE_A}}: one memory limit for all open files. Today each open file has its own limit.
- #{{ISSUE_B}}: replay at mount loads all write-log records after the last commit into RAM before it applies them.

Note: chunking stays canonical (the same bytes give the same chunks on every node). On long repeating data that is not zeros, a rebase can chunk to the end of the file. CPU grows with that run; memory does not.
