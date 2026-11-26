# Phase 21.2 deferred items

Out-of-scope findings during execution. Not fixed in this phase unless a plan says so.

## 1. Flaky `pool_panic_tests` child start on a loaded Mac (found in 21.2-01, Task 1 gate)

- Tests: `slicefs-cli --test pool_panic_tests`:
  `a_worker_panic_aborts_and_the_remount_replays` (gate runs 2 and 3),
  `a_reader_thread_panic_aborts_and_the_remount_replays` (gate run 2 and a
  run of the binary alone) and `a_panic_on_another_thread_does_not_abort`
  (a run of the binary alone). `a_full_queue_that_stays_full_aborts` also
  starts a child; it has not failed yet.
- Symptom: `the child (worker|reader) did not end in 60s; killed`, with an
  empty child stdout and stderr. The child (the same test binary run again
  with `--exact panic_child`) printed nothing at all, not even the libtest
  line `running 1 test`, so it hung at process start, before any pool code.
- Load average: about 11 in run 2 (a 15-minute value of 10.2 right after it),
  about 7 in run 3 (5.8 / 5.6 / 7.2 right after it), on the developer Mac.
- Not caused by the 21.2-01 change: in a normal build `chunk_store::sync::raw`
  re-exports the same std types that `pool.rs` used before, so the pool code is
  unchanged. The same binary passed in gate run 1 (4 of 4, 26.7 s) and passed
  alone between runs 2 and 3 (4 of 4, 34 s, load 6.9).
- Start time of a test binary on this Mac (`<binary> --list`, no test code
  runs), measured right after the failures, load average 7.8 to 22:

  | Binary | Start time |
  |--------|------------|
  | `pool_panic_tests` (6 runs) | 11.7 to 16.8 s |
  | `statfs_tests` | 15.4 s |
  | `statfs_tests`, built before the 21.2-01 change | 61 s (load rose to 22) |
  | `/bin/ls` (control) | 0.07 s |

  A binary built before the change was even slower, so the slow start is a
  property of the host, not of the change. The load came from other projects
  on the host and Spotlight indexing, not from this repository. Each child pays
  this start cost inside the 60 s `CHILD_LIMIT`.
- Gate rule for the rest of 21.2-01 (orchestrator decision): `cargo test
  --workspace` with the 3 failing child tests skipped must exit 0, and
  `pool_panic_tests` must pass in CI on Linux and macOS.
- Logs (not committed; the executor scratch directory of the 21.2-01 run, outside
  the repository): `test-t1b.log` (gate run 2), `test-t1c.log` (gate run 3), both
  `cargo test --workspace` on the default toolchain, and `test-t1d-pool.log`
  (`pool_panic_tests` alone).
- Possible follow-up (not done): find why a fresh test-binary start can stall for
  60 s on macOS under load (an exec scan of a new binary is one guess), or raise
  `CHILD_LIMIT`, or make the child a small separate binary.

## 2. Follow-ups of the unsafe review (found in 21.2-09)

None of these is a memory-safety bug. Not done in this phase (D-33, and
plan 21.2-09 changes no file under crates). See docs/design/unsafe-review.md.

- `unmount.rs` `kill_processes_at_mountpoint` and `fuset_watch.rs`
  `spawn_unmount_watcher`: skip a pid that is not `pid > 1` before `kill`
  (0 signals the process group, -1 every process of the user), with a unit
  test on `fuse_t_servers_of`.
- `signals.rs` `on_signal`: save and restore `errno` around `write`.
- Comment-only: a `SAFETY` comment on the four `unsafe` blocks of
  `filesystem.rs` `compute_statfs`; the `alloc_tune.rs` macOS comment on the
  call line; the `needless_range_loop` reason in
  `crates/slicefs-cli/tests/lock_free_io.rs` says "the block number", but the
  index there is the write number of the thread (from 21.2-03).
