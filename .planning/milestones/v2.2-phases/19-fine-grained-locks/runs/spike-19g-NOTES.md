# Spike 19g: causes of the long lock holds (measurement only)

Harness: `cargo test -p slicefs-cli --features lock-stats --test stress_harness serialize_off_target -- --nocapture`
(debug, 4 threads, serialize off). All code changes were local and are reverted. The probe is in
`spike-19g-probe.patch`. It records, for each hold of 5 ms or more, the waits on other lock sites
in that hold (`wait[site]`) and named spans in that hold (`span[name]`). It also records which
fsyncs were in flight when a pack pwrite or a WAL write took 5 ms or more (`slowio`).

Files:
- `spike-19g-macos-probe.log`: 3 runs, with backtraces.
- `spike-19g-macos-eprintln-ab.log`: 3 runs with eprintln on, 3 runs with it off. Stderr goes to a file.
- `spike-19g-macos-inflight.log`: 2 runs, fsyncs in flight.
- `spike-19g-macos-sync-unlocked.log`: 3 runs with the phase-2 fsync under the store lock, 3 runs with it out of the lock.
- `spike-19g-macos-fallbacks.log`: 3 runs, counts of the locked fallbacks.
- `spike-19g-linux-baseline.log`: 3 runs, OrbStack Docker, no probe.
- `spike-19g-linux-probe.log`: 3 runs, OrbStack Docker, with the probe.
- `spike-19g-pwrite-vs-fsync.py` and `.log`: microbenchmark. One process does a pwrite every 0.5 ms. A second process syncs the same file or another file.

## Q1: eprintln in a cell hold

- `write_in_cell` prints "switches to overlay" (filesystem.rs:2077) with the cell locked. It is on the main write path.
- The `[rebase]` line (filesystem.rs:1631) is in `step_io`. It runs in a cell hold only through the locked `rebase_step`. That path did not run in these runs (see Q2).
- Stderr to a file, eprintln on (3 runs): 39 `file_cell` holds of 5 ms or more. 34 of them are the overlay eprintln alone, 20-37 ms each (`span[eprintln_overlay]` is about the same as the hold). 4 holds are `wait[store]`. 1 hold is 6.4 ms of CPU.
- Eprintln off (3 runs): 5 holds of 5 ms or more. 3 are `wait[store]` (about 20 ms). 2 are 5-6.5 ms of CPU in one-file mode.
- `file_cell` hold max in many-files mode: on 21.8-37.0 ms; off 0.45-20.5 ms. The 20 ms values that stay are store waits.
- Stderr to a pipe: the eprintln did not stall. A write to a stderr file waits behind an F_FULLFSYNC on the volume (see Q3).
- **Conclusion:** the eprintln is the cause of most of the 20-37 ms cell holds, but only when stderr is a file. With the eprintln off, the stripe holds do not change, because their cause is the store lock.

## Q2: are the stripe holds a wait on a cell?

- No. There were 122 stripe holds of 5 ms or more over all macOS probe logs. No stripe hold had `wait[file_cell]`.
  - 114 have `wait[store]` that covers the hold to within 2.4 ms.
  - 2 are 41 ms, from two store waits of 20 ms.
  - 6 are 21-37 ms with no lock wait and no instrumented span. That is I/O we did not instrument, in `unlink_logged` (`meta.unlink`, `drop_last_link`). It is most likely also behind an F_FULLFSYNC, but this is not measured.
- The backtrace is `unlink_logged`, which holds two stripes (NsGuard). So each store wait shows twice.
- The 3 long `file_cell` holds in the first probe are `release_write`, also `wait[store]` (19-41 ms).
- Locked fallbacks did not run in 3 runs. These spans were never recorded: `finalize_steps` locked `rebase_step`, the locked flush retry, the locked step of `rebase_cell`, `write_in_cell` with `unlocked = false` (`has_content`, `stream_step`, `to_overlay`).
- `FB_locked_flush_io` was recorded 85 times, max 0.58 ms. That is `flush_cell(false)` of the commit cut (exclusive gate), not a fallback.
- **Conclusion:** the long stripe holds wait for the store lock. The locked fallbacks are real (code reading in 19-11), but this harness does not run them, so it cannot measure them.

## Q3: store lock (macOS)

Store holds of 5 ms or more, by span:

| Cause | 3-run probe | 3 runs, phase-2 fsync locked | 3 runs, phase-2 fsync unlocked |
|---|---|---|---|
| `sync` phase 2: `sync_active` (+ `sync_dir`) | 39, median 20.0 ms, max 61.7 ms | 47, mean 24.5 ms | **0** |
| `append` record pwrite (`raw_pwrite`) | 211, median 19.6 ms, max 39.4 ms | 216, mean 21.2 ms | **246**, mean 21.3 ms |

- `join_seal` was 0 ms each time. The phase-2 time is the F_FULLFSYNC.
- Store hold max with the fsync out of the lock: 20-56 ms in one-file and many-files mode (before: 21-63 ms). In multi-handle mode 0.08-20 ms. Most of the long holds are the record pwrite.
- **The pwrite stall stays when the fsync is out of the lock. Most of the time the stalled pwrite is not behind an fsync of the same pack.** Fsyncs in flight when a slow pack pwrite started:

| Run | WAL F_FULLFSYNC (another file) | same pack | none of ours |
|---|---|---|---|
| inflight (2 runs) | 97 | 18 | 31 |
| locked (3 runs) | 148 | 31 | 37 |
| unlocked (3 runs) | 138 | 62 | 46 |

- Microbenchmark (no GIL: the syncer is a separate process, APFS):
  - F_FULLFSYNC on the same file: pwrite p50 19.2 ms.
  - F_FULLFSYNC on another file: pwrite p50 19.5 ms.
  - Plain `fsync()` on either file: pwrite p50 18-20 us, max 1.9 ms.
  - So on macOS, any F_FULLFSYNC on the volume stops all pwrites for about 20 ms.
- `wal_writer` is the same effect. Slow WAL drains: 283 behind a WAL fsync, 20 behind a pack fsync, 47 behind no fsync of ours. Hold p99 is 20.6-36.4 ms.

## Q4: Linux (OrbStack Docker, overlay fs, same debug harness)

- Baseline: `wal_writer` hold p99 0.26-1.5 ms (macOS: about 21 ms). Hold max 9-49 ms.
- Store hold p99 is 29-67 us. Store hold max is 3-50 ms, plus one 144 ms.
- Probe (3 runs): only 2 slow pack pwrites (16 and 18.5 ms, both behind a same-pack fsync). There were 10 slow WAL drains (all behind a WAL fsync, mean 40 ms, max 158 ms). There were 9 holds of phase-2 `sync_active` under the store lock.
- Microbenchmark: no stall from fsync or fdatasync on another file. There was 1 outlier of 14.8 ms with fdatasync on the same file.
- **Conclusion:** the cross-file stall is a macOS behavior (F_FULLFSYNC). On Linux, a pwrite stall is rare and only on the same file. The phase-2 fsync under the store lock is a problem on both systems.
- Caveat: OrbStack is a virtual disk, so its fsync does not flush the drive cache as a real disk does.

## Recommendations (code changes, for the owner)

1. **Store sync:**
   - In `ChunkStore::sync` phase 2, under the lock, do only `join_seal` and take a sync handle. Clear `dirty` and `dir_dirty`. Then run `PackSync::sync` and `io.sync_dir` with no lock held.
   - `RetryingPack` needs a `sync_handle` (or a `sync_soon` + wait loop), so the retry copies are settled.
   - On an error, set `failed` again under the lock.
   - Do the same for `flush_frozen`'s `sync_active`.
   - The spike version removed all 47 phase-2 holds.
2. **Record pwrite:** moving the fsync out of the lock is not sufficient. Two ways:
   - (a) Take the pwrite out of the store lock. Under the lock, reserve `offset = a.len; a.len += rec.len()` and push the entry as pending. Write with no lock held. Publish under the lock: readers must not read a reserved range before its write ends.
   - (b) On macOS, take the volume-wide flush out of the hot path. Use plain `fsync` for the record path. Do one F_FULLFSYNC per commit or barrier, in a thread that holds no lock.
   - (a) is the general fix. It also covers the WAL: `PerOpWal::drain` can write its batch with `pwrite` at a reserved offset outside `writer`.
3. **Locked fallbacks:**
   - Add a stress mode or test that makes a step drop. For example, a writer that changes the overlay during each unlocked step. Then measure them. The current harness never takes them.
   - The code change: replace "retry locked" with a bounded number of unlocked retries, then a hand-off. Mark the cell `needs_step` and let the next writer, the release, or the Phase 20 flusher take it. Keep the hard-cap locked step only past `hard_cap`.
4. **Eprintln:** move the "switches to overlay" line (and `[rebase]`) to `tracing::debug!`, or out of the cell hold. On macOS, a stderr file write in a cell hold can wait 20-37 ms behind any F_FULLFSYNC.
