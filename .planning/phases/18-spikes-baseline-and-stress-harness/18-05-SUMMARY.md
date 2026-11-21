---
phase: 18-spikes-baseline-and-stress-harness
plan: 05
subsystem: testing
tags: [spike-s2, spike-s4, spike-s6, linux-fuse, docker, wal, bigwrite]
requires: []
provides:
  - "scripts/docker-linux-test/s6-smallfsync.sh and run.sh s6 mode: 2,000 small fsynced files per WAL strategy"
  - "scripts/docker-linux-test/s4-waiting.sh and run.sh s4 mode: FUSE connection queue depth sampler with a sanity check"
  - "bigwrite.sh prints store_fs for the store directory"
  - "runs/s6-smallfsync-linux.log, runs/s4-waiting-linux.log, runs/s2-bigwrite-linux.log"
affects: [18-07, 20, 21]
tech-stack:
  added: []
  patterns:
    - "Every step under timeout -s KILL; every wait loop checks kill -0 and has a counter limit"
    - "Result lines saved as printed; no value written that a run did not print"
key-files:
  created:
    - scripts/docker-linux-test/s6-smallfsync.sh
    - scripts/docker-linux-test/s4-waiting.sh
    - .planning/phases/18-spikes-baseline-and-stress-harness/runs/s6-smallfsync-linux.log
    - .planning/phases/18-spikes-baseline-and-stress-harness/runs/s4-waiting-linux.log
    - .planning/phases/18-spikes-baseline-and-stress-harness/runs/s2-bigwrite-linux.log
  modified:
    - scripts/docker-linux-test/run.sh
    - scripts/docker-linux-test/bigwrite.sh
key-decisions:
  - "D-09 followed: S2, S4 and S6 run on the Linux Docker mount. S4 needed no --privileged: fusectl mounts with --cap-add SYS_ADMIN alone."
requirements-completed: []
duration: ~60 min
completed: 2026-11-21
status: complete
actuals:
  tokens: 9000
  tasks: 3
  commits: 4
plan_head_before: 98e3522bdccd01400e853e2155ecfd78c5535c69
plan_head_after: 34d75755ba82a59e229e2f6e191412f9f6d4ff9a
commits: 4
---

# Phase 18 Plan 05: S6, S4 and S2 on the Linux Docker mount Summary

Three mounted spikes ran on the Linux Docker mount (OrbStack, kernel 7.2.12, 32 CPUs, 15.7 GiB, store on overlayfs) and their raw numbers are saved for Plan 07. The measured `commits: 4` includes one commit that is not part of this plan: `0b2d5a5` (a docs commit on the v2.2 requirements, made by another session while the plan ran). The plan itself made 3 commits.

## Tasks

| Task | Name | Commit |
|------|------|--------|
| 1 (tracer) | S6 script, run.sh s6 mode, S6 run | 49f5a21 |
| 2 | S4 script, run.sh s4 mode, sanity check, S4 run | f31ef2e |
| 3 | S2 measured again, 3 runs per size, store_fs line | 34d7575 |

## Results (raw, for Plan 07)

### S6: 2,000 fsynced 4 KiB files (create, write, timed fsync, close)

| Strategy | Pass | fsyncs/s | fsync p50 ms | p99 ms | max ms |
|----------|------|----------|--------------|--------|--------|
| per-op | 1 | 71.0 | 4.84 | 143.52 | 437.24 |
| per-op | 2 | 252.4 | 2.47 | 31.78 | 179.80 |
| flush-on-fsync | 1 | 237.7 | 3.11 | 27.18 | 121.90 |
| flush-on-fsync | 2 | 358.4 | 2.09 | 16.36 | 168.93 |
| periodic | 1 | 280.1 | 2.87 | 18.41 | 109.55 |
| periodic | 2 | 320.1 | 2.04 | 15.21 | 402.36 |

- The buffered strategies are about 1.3 to 1.4x faster than per-op on pass 2 and have a lower p99 (15 to 16 ms against 32 ms). The pass 1 per-op run was much slower (71/s); pass 2 is 3.5x faster. The cause is not established (the first run in a fresh container is a guess, not a finding). Two passes are too few to put a number on the gap; read the medians as "same order of magnitude, buffered somewhat better".
- No WAL flusher exists yet, so this compares per-op with buffered strategies only. A group-commit benefit is a Phase 21+ question.
- The fsync here is the cost of one FUSE fsync on overlayfs, so it includes the host disk of the Docker VM.

### S4: kernel queue depth (`waiting`) of the FUSE connection, one 256 MiB file, 60 s per pass, sampled every ~2 ms

| Pass | samples | max | p50 | p99 | histogram |
|------|---------|-----|-----|-----|-----------|
| sanity (daemon stopped, one stat) | - | 1 | - | - | counter moves |
| write | 25523 | 1 | 1 | 1 | 0:5012, 1:20511 |
| readahead | 26030 | 2 | 2 | 2 | 0:1783, 1:6961, 2:17286 |
| both | 25753 | 7 | 3 | 5 | 0:456, 1:1407, 2:5617, 3:15382, 4:2597, 5:271, 6:22, 7:1 |

- Random writes alone keep at most 1 request pending (they are one at a time at the daemon, with a single dispatch thread). Readahead alone reaches 2. With both on the same file, up to 7 requests are queued in the kernel, p50 3. So the kernel does queue more than one request when reads and writes meet, and a single dispatch thread is where they wait.
- Limits: `waiting` counts the whole connection, not one inode (one active file only); the daemon has one dispatch thread, so only the kernel-side queue is seen. The readahead pass drops the file page cache (fadvise DONTNEED) before each `dd` so the reads reach the daemon.

### S2: bigwrite measured again on the current code (3 runs per size, all PASS)

| Size | max write ms (min / median / max) | max fsync ms (min / median / max) | VmHWM max MiB |
|------|-----------------------------------|-----------------------------------|---------------|
| 1 GiB | 124 / 224 / 244 | 136 / 308 / 621 | 112 |
| 4 GiB | 314 / 343 / 1353 | 220 / 677 / 1637 | 170 |

- Store file system: overlayfs (the #108 spike used btrfs). References after #108: 440 / 686 ms on 90a5443, 117 / 1542 ms on 3f3ff34. These runs are in the same band; the medians (224 / 343 ms) are under the 500 ms bound of #108 but the 4 GiB maximum (1353 ms, run 2) is not. Run 2 also had a pack sync of 1756 ms and 16.7 s of total pack sync time at 4 GiB (other runs: 4 to 5 s), so the tail looks driven by the disk, not by SliceFS code; this is an inference, not tested.
- Peak memory stays under the 256 MiB cap in every run (max 170 MiB), and every run switched to overlay once.

## Deviations from Plan

None - plan executed as written. Small notes:
- `run.sh s4` and `s6` were first run before the scripts had the executable bit (exit 126); `chmod +x` fixed it and the run was repeated. The first `s4` try died on a transient cargo error reading the host git index through the mount (`Shared index checksum mismatch`); the repeat was clean. Neither run produced saved numbers.
- The S4 sanity check, the sampler and the load generators follow the plan; the load generators are `python3` and `dd` as specified. The reader also calls fadvise DONTNEED before each `dd` (added so that repeated reads are not served from the page cache).
- The S6 log header names the commit of the working tree (`98e3522`) and S4 `49f5a21` (the head at run time, scripts differ only).

## Known Stubs

None.

## Threat Flags

None. The `s4` mode did not need `--privileged` (T-18-16 not triggered); saved logs have 0 lines with a home path (T-18-17).

## Self-Check: PASSED

- FOUND: s6-smallfsync.sh, s4-waiting.sh, run.sh (s4 and s6 modes), bigwrite.sh (1 `store_fs=` line), three run logs
- FOUND commits: 49f5a21, f31ef2e, 34d7575
- Gates: S6 log has 6 result lines and `RESULT s6: PASS`; S4 log has 3 pass lines, sanity 1 and `RESULT s4: PASS`; S2 log has 3 runs, 3 `store_fs=` lines, 2 S2 table rows; home-path count 0 in all three; `bash -n` passes on the four scripts.
