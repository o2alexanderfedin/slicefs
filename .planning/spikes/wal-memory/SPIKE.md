# Spike: write log (WAL) memory

Date: 2026-11-18. Branch: `spike/wal-memory` (local only, not for merge).
Base: `origin/develop` at 6296eb6. Spike code: commit 7f68048.

## Question

How much daemon RAM does the write log use, and what do we change?

- **Baseline** (current code).
- **Option A**: per-op calls `flush()` after each operation record (one
  `write()` into the kernel, no fsync). Also, flush-on-fsync and periodic
  write their buffer to the segment (no fsync) when it passes 4 MiB.
- **Option B**: per-op does not change. Only the 4 MiB cap for
  flush-on-fsync and periodic.

The number of fsync calls must not change in any option.

## Facts in the code (checked)

- `SegmentWriter` writes through a `BufWriter<File>` with the default
  8 KiB buffer.
- Per-op: root and snapshot records are synced at once. Operation records
  are synced only by fsync, fsyncdir and commit. Up to about 8 KiB stays in
  daemon RAM. A record of 8 KiB or more goes past the buffer directly.
- Flush-on-fsync and periodic keep the records in a `Vec<WalEntry>` with no
  limit. `Op::Write` holds the written bytes, so this RAM grows with the
  written bytes.
- The buffer becomes empty only on fsync, fsyncdir or commit (`rotate`).
  **Periodic has no timer**: `interval_secs` is not used, and nothing calls
  `flush_and_sync` on a schedule. Thus periodic is the same as
  flush-on-fsync. The RAM limit of both is "all bytes written since the last
  fsync or commit". With the default commit interval of 30 s, a writer at
  1 GB/s can put about 30 GB into daemon RAM.

## Method

Spike code (commit 7f68048):

- The env var `SLICEFS_WAL_SPIKE` (`a`, `b`, or not set) selects the option
  at run time. Thus one binary runs all options.
- Process counters: `write()` calls into segment files, `sync_all` calls on
  segment files, and the peak size of the buffered log.
- `crates/slicefs-cli/tests/wal_spike_bench.rs`: an in-process benchmark
  through the request handlers (`handle_create`, `handle_write`,
  `handle_release`, `handle_fsync`), the same harness as
  `write_log_replay_tests.rs`. One case per process, so the peak RSS is per
  case. Release build. We ran the test binary directly.
- `scripts/docker-linux-test/wal-spike.sh` with a new `run.sh walspike`
  mode: real Linux FUSE mounts in Docker (OrbStack), release build, kill -9
  of the daemon, then a remount. `run.sh` now accepts `SLICEFS_TARGET_VOL`;
  this spike used the volume `slicefs-target-walspike`.

Run order: the modes were interleaved (base, A, B, base, A, B, ...),
5 rounds for each throughput case, 3 rounds for each memory case. We give
the median and the minimum. CPU time is user + system time of the process
(getrusage). The `write()` counts do not depend on load.

### Machine and load

- macOS 26.2, 32 CPUs, 192 GiB RAM, APFS. The machine was shared with other
  agents.
- Host load average (1 min) during the macOS benchmarks: **72 to 97**.
  Wall-clock times are thus noisy. Use the CPU times and the `write()`
  counts first.
- Docker: OrbStack Linux VM, kernel 7.2.12, 32 CPUs, 15.7 GiB RAM. Load
  average inside the VM: 2.7 to 4.7.

## Results

### 1. Throughput cost of the per-op flush (per-op mode, no fsync)

| Case | Option | Wall s, median (min) | CPU s, median (min) | ops/s, median | MB/s, median | `write()` calls |
|---|---|---|---|---|---|---|
| (a) 1 GiB as 8192 writes of 128 KiB | base | 22.85 (18.76) | 9.80 (9.52) | 359 | 47.0 | 8193 |
| | A | 22.55 (15.69) | 9.80 (9.37) | 363 | 47.6 | 8194 |
| | B | 21.83 (17.68) | 9.86 (9.67) | 375 | 49.2 | 8193 |
| (b) 100k writes of 4 KiB | base | 8.51 (6.99) | 4.76 (4.59) | 11 748 | 48.1 | 99 999 |
| | A | 8.28 (7.21) | 4.95 (4.70) | 12 072 | 49.4 | 100 002 |
| | B | 9.15 (6.75) | 4.94 (4.82) | 10 934 | 44.8 | 99 999 |
| (c) 20k files: create, write 4 KiB, close | base | 4.45 (3.82) | 2.48 (2.36) | 4 493 files/s | 18.4 | 19 999 |
| | A | 5.47 (4.34) | 2.69 (2.60) | 3 658 files/s | 15.0 | 60 000 |
| | B | 4.27 (3.96) | 2.51 (2.34) | 4 684 files/s | 19.2 | 19 999 |
| (d) worst case: 200k writes of 64 bytes | base | 0.96 (0.63) | 0.55 (0.54) | 208 824 | 13.4 | 2 816 |
| | A | 3.27 (2.85) | 1.74 (1.73) | 61 160 | 3.9 | 200 002 |
| | B | 0.76 (0.67) | 0.54 (0.50) | 264 307 | 16.9 | 2 816 |

Why (a) and (b) show no cost: a record of a 128 KiB write is larger than
the 8 KiB buffer, so `BufWriter` writes it directly. A record of a 4 KiB
write is more than half of the buffer, so each record already pushes out
the record before it. In both cases the baseline already makes one
`write()` per record. Option A adds only the last one or two.

Case (c) has three records per file (create, write, release). The
baseline makes one `write()` per file, option A makes three. The CPU cost
is +8 % (about 10 µs per file on APFS).

Case (d) is the worst case that we found: very small records. Option A
makes one `write()` per record instead of one per about 70 records. The
CPU cost is about 6 µs per operation on APFS (3.2 times the CPU of the
in-process path). Note: this path has no FUSE. In a real mount, each
request also pays a FUSE round trip and the kernel calls of the FUSE
channel, so the relative cost is smaller.

**fsync count check** (1000 files, one fsync each, per-op):

| Option | `sync_all` calls | `write()` calls |
|---|---|---|
| base | 1000 | 1000 |
| A | 1000 | 4000 |
| B | 1000 | 1000 |

Also, with `SLICEFS_WAL_SPIKE=a` and `=b`, these tests pass with no
change: `fsync_tests`, `wal_durability_tests`, `wal_strategy_tests`,
`write_log_replay_tests`, `crash_recovery_tests` (slicefs-cli) and all
130 lib tests of `metadata`. Some of these tests count disk syncs.

### 2. Memory of the uncapped modes (1 GiB as 128 KiB writes, no fsync)

| Mode | Option | Peak RSS, median of 3 | Peak log buffer | CPU s, median |
|---|---|---|---|---|
| flush-on-fsync | base | 1276 MiB | 1024.4 MiB | 9.36 |
| flush-on-fsync | B (= A) | 284 MiB | 4.00 MiB | 9.85 |
| periodic | base | 1279 MiB | 1024.4 MiB | 9.18 |
| periodic | B (= A) | 290 MiB | 4.00 MiB | 9.83 |
| per-op (for comparison, case a) | base | 243 MiB | 0 | 9.80 |

The capped run uses about 0.5 s more CPU. That is the write of 1 GiB of
records into the segment file. The baseline did not do this work in the
test because no fsync came; with an fsync later, the baseline does the
same work, but later and in one block.

Options A and B are the same for flush-on-fsync and periodic.

### 3. Safety: real kill -9 in Docker (Linux FUSE)

Steps (commit interval 0, so only the write log can keep the data):
`cp` a 3000-byte file; `dd` a 1 MiB file (64 KiB blocks); `cp` a 1 MiB
file and `mv` it; `dd` a 10 MiB file (128 KiB blocks); `mkdir` and write a
small file in it. No fsync. Wait 1 s, kill -9 the daemon, remount (the
mount replays the log), and compare with the source files.

4 runs for each line (3 in the first Docker run, 1 in the second). All
runs of a line gave the same result.

| Write log mode | Option | small | 1 MiB | renamed | 10 MiB | last mkdir + small file | Requests replayed |
|---|---|---|---|---|---|---|---|
| per-op | base | kept | kept | kept | kept | **lost** | 118 |
| per-op | A | kept | kept | kept | kept | **kept** | 126 |
| per-op | B | kept | kept | kept | kept | **lost** | 118 |
| flush-on-fsync | base | lost | lost | lost | lost | lost | none (no replay line) |
| flush-on-fsync | A | kept | kept | kept | kept | lost | 118 |
| flush-on-fsync | B | kept | kept | kept | kept | lost | 118 |

- Per-op baseline and option B lose the last records (less than 8 KiB),
  here the last `mkdir` and the small file after the 10 MiB write. They
  keep all records before them only because the large write records
  pushed the buffer out. With only small requests (for example an untar
  of small files), a kill -9 loses up to 8 KiB of the most recent
  requests.
- Per-op option A keeps all completed requests.
- Flush-on-fsync with the cap keeps all records up to the last time the
  buffer passed 4 MiB.

**The cap can cut a file.** Flush-on-fsync, one 6 MiB file, no fsync,
kill -9:

| Option | Result after the remount |
|---|---|
| base | the file does not exist |
| B (= A) | the file exists, size 4 194 304 of 6 291 456, a correct prefix of the source, mode 644 |

This is allowed (no fsync came), but it is new behaviour: after a crash,
a user can see a short file instead of no file.

Power loss: no option changes this. Records after the last fsync are only
in the kernel page cache, and they are lost in all options. No option
promises more than fsync.

### 4. 1 GiB `cp` in Docker (per-op, release build)

This is an in-order `cp` of 1 GiB into a mount with the default commit
interval. It is not the full "Large copy" section of `verify-phases.sh`
(that section uses a debug build, an out-of-order pass and `cmp`).

| Option | Wall s, 8 runs | Median | Daemon CPU s, median | Daemon VmHWM |
|---|---|---|---|---|
| base | 31.80 33.08 32.27 20.69 19.31 19.89 12.83 16.63 | 20.29 | 16.2 | 80 to 81 MiB |
| A | 39.86 19.30 24.26 16.99 17.46 19.81 12.34 23.04 | 19.56 | 15.8 | 80 to 81 MiB |

The runs were interleaved (base, A). We cannot see a difference. This
agrees with case (a): 128 KiB records already go past the buffer.

## Recommendation: option A

**Memory footprint.** A and B are the same where the memory problem is:
flush-on-fsync and periodic. The 4 MiB cap takes peak RSS from 1276 MiB to
284 MiB for a 1 GiB write, and the log buffer from 1 GiB (no limit) to
4 MiB. Without a cap, the limit is all bytes since the last fsync or
commit; periodic has no timer, so it is not better. For per-op, A takes
daemon RAM from at most 8 KiB to at most one record. This difference is
small.

**Read and write performance.** Reads do not change in any option. For
data writes of 4 KiB or more, A costs nothing that we can measure
(cases a and b, and the 1 GiB `cp` in Docker), because those records
already go past the 8 KiB buffer. For small-file work A costs +8 % CPU
in-process (case c, three `write()` calls per file instead of one). The
worst case, very small writes, costs about 6 µs per request on APFS
(case d). In a real mount, each request also pays a FUSE round trip, so
the relative cost is smaller. The number of fsync calls does not change.

**Safety.** A is the only option where a kill -9 of the daemon (a crash,
the OOM killer, a panic) loses no completed request in the default mode.
With the baseline and with B, a kill -9 loses the most recent requests
that fit in 8 KiB, and with only small requests these are the most
recent creates, renames and mkdirs. Power loss is the same in all
options.

Reasons to select A, not B:

1. The cost is small and it is only on small-record work. The cost is zero
   for large writes.
2. The safety gain is real and we saw it on Linux FUSE in 4 of 4 runs.
3. The rule is simple to document: "a completed request survives a crash
   of the daemon; fsync makes it survive power loss".

Points for the real change:

- Document the cut file of the capped buffered modes (a short file can
  appear after a crash).
- Periodic has no timer. Either add the timer or remove the mode; it is
  now the same as flush-on-fsync.
- If the small-record cost becomes a problem, one `write()` for each
  batch of requests (for example when the FUSE queue is empty) gives the
  same safety for fewer calls. We did not measure this.
