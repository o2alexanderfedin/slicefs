# Spike E: mmap writes for the active arena

## Question

Can the chunk store write records through a shared map (`mmap` with `MAP_SHARED`) of a fixed-size arena that is allocated in advance, instead of `pwrite`?

The argument for it: the extra buffers (the zstd output `Vec` and the record `Vec`) go away. A put becomes one memory copy.

The known risk: with `mmap`, a disk error or a full disk can become `SIGBUS`, not `EIO` or `ENOSPC`.

## Short answer

**No-go for the current design. Go only if all the conditions in the "Verdict" section are true.**

- On APFS, a full disk did not cause `SIGBUS` in any case. The failure is silent at the time of the write. `msync(MS_SYNC)` also returns success. Only `F_FULLFSYNC` returns `ENOSPC`. After that, `pread` still shows the new bytes from the page cache, but the bytes on disk are zeros or old data.
- `SIGBUS` does occur when the device goes away (forced detach). `pread` and `pwrite` then return `EIO`.
- The speed gain comes from the arena type, not from the removed buffers. On `Reserve` arenas (`F_PREALLOCATE`: the blocks are allocated but not written), `pwrite` costs about 25 to 30 us more system time for each put. On `Zeroed` arenas, the best `pwrite` path and `mmap` have almost the same CPU for each put (2 to 10 us less for `mmap`).
- With a sync every 64 puts or every 1 MiB, `F_FULLFSYNC` (21 to 54 ms) is the largest cost. The put path is small in comparison.

## Key numbers

CPU time for each put (us, median of 10 runs, without the sync calls):

| Workload, sync | `alloc` (today) | `pwbuf` (reused buffers) | `mmap` | `pwbufV3` | `mmapV3` |
|---|---|---|---|---|---|
| random, none | 95.2 | 80.9 | 48.3 | 41.3 | 39.4 |
| random, every 64 puts | 94.2 | 84.3 | 49.5 | 43.5 | 40.8 |
| random, every 1 MiB | 93.5 | 81.4 | 47.0 | 41.4 | 37.8 |
| comp, none | 165.8 | 160.8 | 113.3 | 108.9 | 104.2 |
| comp, every 64 puts | 165.2 | 165.5 | 119.1 | 123.4 | 113.3 |
| comp, every 1 MiB | 159.0 | 148.7 | 113.6 | 112.1 | 104.4 |

Sync (median):

| Item | `pwrite` paths | `mmap` (`Reserve`) | `mmapV3` (`Zeroed`) |
|---|---|---|---|
| Full `sync` call | 21 to 42 ms | 22 to 34 ms | 23 to 34 ms |
| `msync` part | - | 0.41 to 0.92 ms | 0.29 to 1.81 ms |
| CPU for each sync | 105 to 156 us | 341 to 625 us | 206 to 1507 us |
| `msync` of a full 64 MiB arena at rotation (no-sync runs) | - | 38 to 44 ms | 38 to 43 ms |

The `F_FULLFSYNC` time changes much from run to run (min 20.4 ms, max 54.8 ms) because of the load. The sync numbers do not show a difference between the paths that is larger than this noise.

Page faults: `mmap` on `Reserve` has 2.49 major faults for each random put and 1.30 for each compressible put. This is about one fault for each new 4 KiB page of the arena. The kernel counts a first touch of a reserved, unwritten block as a major fault. On `Zeroed` arenas, `mmap` has no major faults, but 1.8 to 3.9 minor faults for each put.

Gains, from these numbers:

- Reuse gain (`alloc` to `pwbuf`, no allocation and one zstd context for each writer): 10 to 14 us for each random put, 0 to 10 us for each compressible put.
- `mmap` gain on `Reserve` (`pwbuf` to `mmap`): 32 to 35 us (random), 35 to 47 us (compressible). Most of it is system time of `pwrite` into reserved, unwritten blocks (`pwbuf` sys 35 to 40 us, `mmap` sys 9 to 16 us).
- `mmap` gain on `Zeroed` (`pwbufV3` to `mmapV3`): 2 to 4 us (random), 5 to 10 us (compressible).

## Machine and conditions

- The machine is an Intel Mac Pro (Xeon W-3245, 16 cores, 32 threads, x86_64), not Apple Silicon. The page size is 4 KiB, not 16 KiB. The kernel is Darwin 25.2.0. The bench volume is the internal APFS data volume.
- The machine had a high load from other work (`load1` from 23 to 33 in all runs). For this reason, CPU time for each put (user and system time from `getrusage`) is the main measure. Wall time is less reliable.
- All runs use one writer thread, record v2, granule 1, zstd level 3, and the 1/8 rule (keep the compressed bytes only if they are at least 1/8 smaller).
- Power loss was not tested. Only process crashes (`kill -9`) were tested.

## Method

### Three write paths (`src/mmap_write.rs`)

The three paths share the hash, the dedup index, and the record format. A test (`the_three_paths_write_the_same_bytes`) shows that the three paths write arena files that are equal byte for byte.

| Path | What it does for each put |
|---|---|
| `alloc` | Today's code. `zstd::bulk::compress` (a new zstd context and a new `Vec`), then `codec::encode_record` (a second `Vec`), then one `pwrite`. |
| `pwbuf` | The best `pwrite` path. The writer keeps one `zstd::bulk::Compressor` and one record buffer. zstd writes into the buffer after the header. The header and the CRC are written in place. Then one `pwrite`. There is no allocation for each put. |
| `mmap` | The same `Compressor`, but its destination is the map at the record offset. The header and the CRC are written in place. `sync` is `msync(MS_SYNC)` on the dirty pages, then `F_FULLFSYNC`. |

The zstd destination has a length of `raw - raw/8`. If the output does not fit, zstd returns an error, and the raw bytes are copied over the partial output. Thus the 1/8 rule needs no bound check. The `pwbuf` and `mmap` paths reserve the worst case (`rec_len(raw)`) before compression, because the record offset must be known before zstd writes.

Variants: `alloc`, `pwbuf` and `mmap` use `Reserve` arenas (V2: `F_PREALLOCATE`, then `ftruncate`). `pwbufV3` and `mmapV3` use `Zeroed` arenas (V3: the full file is written with zeros at create). The arena size is 64 MiB.

### Bench `e1` (`src/bench/e.rs`, `run_e.sh`)

- Workloads: `random` (incompressible) and `comp` (about 2:1 with zstd-3, 16 symbols for each byte). The chunk sizes come from FastCDC 4/8/32 KiB.
- Sync modes: `none` (192 MiB, so 3 or more arenas and the cost of arena rotation are included), `n64` (a sync every 64 puts, 32 MiB), and `mib1` (a sync after each 1 MiB of raw data, like a commit, 32 MiB).
- Each run is a new process. The variants are interleaved in round-robin order, and the start variant changes for each repeat. There are 5 repeats in each of 2 rounds (`r1`, `r2`).
- CPU and page faults for each put = the full span minus the sync calls. The kernel writes dirty pages later (writeback). That cost is not in the numbers of either path.

### Correctness (`tests/mmap_e.rs`, `src/bin/ecrash.rs`)

- Tests (9, all pass): data written through the map reads back with `pread` before and after sync, and after a new open (recovery walk), for `Sparse`, `Reserve` and `Zeroed`. Other tests: arena rotation, resume after reopen, a dedup hit, and a torn tail after the last record (the recovery cuts it, and the tail reads as zeros).
- Crash loop: a child process puts 8 objects, syncs, and appends the 8 ids to a side file, then syncs that file. The parent sends `SIGKILL` at a random time. Then the parent opens each arena read-only (no cut) and records the torn tail. Then it opens each arena for real (the cut runs) and reads every synced id and every record that the walk found.

### Full-disk matrix (`src/bin/edisk.rs`)

- For each case: a new 64 MiB APFS disk image (`hdiutil create -fs APFS`, `attach -nobrowse`, no root). The arena is 16 MiB. The case runs in a child process. Thus a `SIGBUS` stops only the child, and the parent reads the signal. The image is detached and deleted after each case.
- "Full" is proved: a large filler file until `ENOSPC`, again after `F_FULLFSYNC`, then new 4 KiB files until 3 in a row fail, then a probe file must fail with `ENOSPC`. All cases passed this probe. (`f_bavail` still showed about 1.8 MB free; APFS keeps this space.)
- "Equal" means: the bytes are equal to the pattern after the image is detached and attached again. A `pread` before the remount only reads the page cache, so it does not prove that the data is on disk.
- The full matrix ran two times (`git` value `e0ec1f2` and `e0ec1f2+edisk2` in `disk.jsonl`). The two rounds gave the same result for each case.

## Bench results

Each cell: median [min..max] of 10 runs (2 rounds of 5 interleaved repeats). CPU, faults and put times are for each put, without the sync calls. "sync" = the full `sync` call (for `mmap`: `msync` + `F_FULLFSYNC`).

### random, no sync (192 MiB)

Load (`load1`): median 27.5, min 23.8, max 32.0.

| variant | CPU/put us | sys/put us | put p50 us | put mean us | majflt/put | minflt/put | raw MiB/s |
|---|---|---|---|---|---|---|---|
| alloc | 95.2 [87.6..135.2] | 51.4 [45.7..78.6] | 139.0 [116.2..193.3] | 202.7 [166.9..269.5] | 0.00 [0.00..0.00] | 0.62 [0.56..0.69] | 47.3 [36.0..58.0] |
| pwbuf | 80.9 [73.2..135.5] | 40.3 [36.0..76.2] | 130.1 [107.1..203.2] | 179.7 [160.6..267.3] | 0.00 [0.00..0.00] | 0.42 [0.37..0.60] | 52.9 [36.2..60.2] |
| mmap | 48.3 [45.0..61.0] | 16.4 [14.3..22.8] | 32.0 [29.5..38.1] | 91.2 [84.6..115.6] | 2.49 [2.43..2.51] | 0.50 [0.39..0.61] | 104.9 [83.9..112.8] |
| pwbufV3 | 41.3 [36.7..45.1] | 12.6 [11.2..14.5] | 32.5 [27.7..34.3] | 56.4 [49.1..64.3] | 0.00 [0.00..0.00] | 0.53 [0.49..0.57] | 170.1 [148.5..197.0] |
| mmapV3 | 39.4 [34.5..48.4] | 11.1 [9.0..15.3] | 27.9 [24.0..32.8] | 61.4 [52.6..72.0] | 0.00 [0.00..0.00] | 2.98 [2.93..3.12] | 156.7 [135.2..184.5] |

### random, sync every 64 puts (32 MiB)

Load (`load1`): median 28.4, min 25.4, max 33.0.

| variant | CPU/put us | sys/put us | put p50 us | put mean us | majflt/put | minflt/put | sync p50 ms | msync p50 ms | CPU/sync us |
|---|---|---|---|---|---|---|---|---|---|
| alloc | 94.2 [80.8..101.4] | 48.0 [39.2..53.3] | 143.0 [108.3..167.3] | 170.1 [135.4..200.3] | 0.00 [0.00..0.00] | 2.58 [1.93..2.74] | 41.8 [20.9..47.4] | - | 122 [101..135] |
| pwbuf | 84.3 [72.8..90.5] | 40.1 [34.2..42.4] | 131.1 [101.7..174.6] | 160.2 [127.2..200.4] | 0.00 [0.00..0.00] | 1.11 [0.81..1.29] | 35.0 [21.1..48.5] | - | 122 [95..131] |
| mmap | 49.5 [46.3..53.7] | 12.4 [11.6..15.9] | 34.5 [31.7..38.3] | 71.6 [67.5..83.0] | 2.49 [2.43..2.51] | 1.34 [1.12..1.49] | 24.4 [21.5..28.1] | 0.72 [0.66..0.84] | 521 [502..535] |
| pwbufV3 | 43.5 [40.6..47.8] | 11.1 [9.5..13.5] | 33.7 [32.4..36.1] | 44.0 [41.0..48.7] | 0.00 [0.00..0.00] | 1.42 [1.16..1.56] | 23.6 [20.4..30.3] | - | 127 [120..140] |
| mmapV3 | 40.8 [36.9..43.8] | 6.0 [5.0..9.5] | 31.3 [28.5..33.3] | 40.8 [36.9..44.7] | 0.00 [0.00..0.00] | 3.80 [3.52..4.13] | 22.8 [20.6..31.1] | 0.40 [0.38..0.45] | 246 [239..269] |

### random, sync after each 1 MiB of raw data (32 MiB)

Load (`load1`): median 27.0, min 23.6, max 30.1.

| variant | CPU/put us | sys/put us | put p50 us | put mean us | majflt/put | minflt/put | sync p50 ms | msync p50 ms | CPU/sync us |
|---|---|---|---|---|---|---|---|---|---|
| alloc | 93.5 [84.0..107.3] | 48.0 [41.8..62.1] | 125.8 [112.3..153.2] | 169.9 [134.8..218.1] | 0.00 [0.00..0.00] | 2.43 [2.16..2.80] | 33.7 [21.4..52.8] | - | 144 [124..167] |
| pwbuf | 81.4 [74.1..86.7] | 39.1 [35.7..42.7] | 123.5 [102.8..146.0] | 152.0 [133.1..176.0] | 0.00 [0.00..0.00] | 1.12 [0.92..1.29] | 32.0 [21.3..52.7] | - | 142 [126..161] |
| mmap | 47.0 [43.7..48.3] | 11.8 [11.0..14.6] | 32.5 [30.1..33.9] | 70.7 [63.9..78.2] | 2.49 [2.43..2.51] | 1.25 [1.14..1.36] | 28.7 [21.5..44.9] | 0.92 [0.89..1.07] | 625 [593..654] |
| pwbufV3 | 41.4 [37.7..44.6] | 10.6 [9.2..13.2] | 33.2 [30.1..34.6] | 41.6 [37.8..56.4] | 0.00 [0.00..0.00] | 1.41 [1.03..1.57] | 27.1 [20.7..32.7] | - | 156 [146..188] |
| mmapV3 | 37.8 [35.7..40.7] | 5.7 [5.0..8.0] | 29.4 [28.1..31.2] | 37.9 [35.8..41.7] | 0.00 [0.00..0.00] | 3.79 [3.58..4.08] | 30.0 [22.9..38.0] | 1.81 [1.77..1.89] | 1507 [1468..1550] |

### comp, no sync (192 MiB)

Load (`load1`): median 26.4, min 25.5, max 28.4.

| variant | CPU/put us | sys/put us | put p50 us | put mean us | majflt/put | minflt/put | raw MiB/s |
|---|---|---|---|---|---|---|---|
| alloc | 165.8 [161.4..178.3] | 47.8 [42.1..51.1] | 199.7 [189.8..217.7] | 242.9 [224.4..260.4] | 0.00 [0.00..0.00] | 0.61 [0.55..0.63] | 39.6 [37.2..43.3] |
| pwbuf | 160.8 [152.8..170.4] | 39.0 [37.1..40.5] | 195.8 [186.9..209.3] | 237.1 [215.7..246.6] | 0.00 [0.00..0.00] | 0.40 [0.35..0.46] | 41.0 [38.3..44.3] |
| mmap | 113.3 [106.3..131.2] | 10.1 [9.2..10.9] | 101.7 [93.3..124.9] | 134.9 [129.4..164.2] | 1.30 [1.26..1.30] | 0.53 [0.47..0.58] | 71.4 [59.3..73.8] |
| pwbufV3 | 108.9 [102.9..134.6] | 10.3 [9.4..12.0] | 98.8 [91.6..130.1] | 113.2 [107.9..163.8] | 0.00 [0.00..0.00] | 0.54 [0.51..0.62] | 85.1 [59.5..89.9] |
| mmapV3 | 104.2 [95.2..114.5] | 5.6 [5.1..6.9] | 94.6 [84.6..108.0] | 109.9 [101.9..121.0] | 0.00 [0.00..0.00] | 1.84 [1.79..1.89] | 87.5 [80.5..92.6] |

### comp, sync every 64 puts (32 MiB)

Load (`load1`): median 25.9, min 23.9, max 29.6.

| variant | CPU/put us | sys/put us | put p50 us | put mean us | majflt/put | minflt/put | sync p50 ms | msync p50 ms | CPU/sync us |
|---|---|---|---|---|---|---|---|---|---|
| alloc | 165.2 [155.6..183.0] | 46.4 [42.6..63.2] | 188.9 [178.9..211.1] | 212.7 [197.9..273.7] | 0.00 [0.00..0.00] | 2.60 [2.23..2.92] | 23.3 [20.8..53.8] | - | 105 [91..115] |
| pwbuf | 165.5 [154.9..173.8] | 40.1 [36.7..43.2] | 194.0 [178.1..216.0] | 221.3 [196.8..240.5] | 0.00 [0.00..0.00] | 1.02 [0.77..1.27] | 35.4 [20.6..53.8] | - | 109 [91..131] |
| mmap | 119.1 [110.5..135.6] | 9.4 [8.6..10.0] | 103.9 [96.0..126.8] | 134.0 [123.5..152.3] | 1.30 [1.26..1.30] | 1.30 [1.13..1.74] | 22.0 [20.9..46.1] | 0.41 [0.40..0.43] | 341 [328..361] |
| pwbufV3 | 123.4 [112.8..132.6] | 11.5 [10.4..12.7] | 115.0 [100.4..129.0] | 124.3 [113.2..133.7] | 0.00 [0.00..0.00] | 1.30 [1.04..1.60] | 21.2 [20.7..36.4] | - | 125 [122..135] |
| mmapV3 | 113.3 [106.0..126.4] | 5.3 [4.9..7.8] | 104.3 [96.2..122.0] | 114.1 [106.1..128.0] | 0.00 [0.00..0.00] | 2.72 [2.53..2.91] | 28.4 [21.2..49.6] | 0.29 [0.28..0.30] | 206 [201..220] |

### comp, sync after each 1 MiB of raw data (32 MiB)

Load (`load1`): median 25.0, min 23.9, max 28.4.

| variant | CPU/put us | sys/put us | put p50 us | put mean us | majflt/put | minflt/put | sync p50 ms | msync p50 ms | CPU/sync us |
|---|---|---|---|---|---|---|---|---|---|
| alloc | 159.0 [147.0..170.7] | 43.7 [38.4..49.7] | 184.4 [178.0..199.2] | 212.3 [200.8..227.6] | 0.00 [0.00..0.00] | 2.42 [1.94..2.68] | 34.5 [21.0..49.7] | - | 113 [103..140] |
| pwbuf | 148.7 [141.4..176.1] | 35.4 [33.2..42.3] | 185.8 [158.6..205.7] | 211.8 [184.9..230.2] | 0.00 [0.00..0.00] | 0.85 [0.78..1.20] | 29.3 [21.1..50.5] | - | 108 [96..132] |
| mmap | 113.6 [107.3..132.5] | 9.2 [7.9..14.0] | 100.6 [94.5..116.9] | 126.9 [118.7..145.0] | 1.30 [1.26..1.30] | 1.25 [0.93..1.61] | 33.6 [21.3..54.8] | 0.50 [0.48..0.54] | 376 [362..394] |
| pwbufV3 | 112.1 [103.4..122.0] | 10.7 [9.3..11.9] | 100.0 [89.8..113.0] | 112.7 [103.6..122.7] | 0.00 [0.00..0.00] | 1.24 [1.18..1.50] | 38.3 [20.5..49.2] | - | 132 [122..143] |
| mmapV3 | 104.4 [100.7..115.9] | 4.8 [4.2..5.6] | 94.4 [85.0..107.4] | 104.6 [101.0..116.4] | 0.00 [0.00..0.00] | 2.64 [2.36..3.02] | 33.5 [21.4..49.8] | 0.36 [0.33..0.39] | 236 [226..245] |

## Full-disk and error matrix

"Cache" = what `pread` showed after the `F_FULLFSYNC` error. "Remount" = the bytes after detach and attach.

| Case | Write | `msync` | `F_FULLFSYNC` | Second `F_FULLFSYNC` | Cache | Remount | Result |
|---|---|---|---|---|---|---|---|
| a. `Reserve`, fill, `mmap` write of the full arena | ok, no `SIGBUS` | ok | ok | - | - | equal | **ok** |
| a. `Sparse`, fill, `mmap` write | ok, no `SIGBUS` | **ok** | **ENOSPC** | ENOSPC | new bytes | all zeros | **silent loss until fsync** |
| a. `Zeroed`, fill, `mmap` write | ok | ok | ok | - | - | equal | **ok** |
| a. control: `Reserve`, fill, `pwrite` | ok | - | ok | - | - | equal | ok |
| a. control: `Sparse`, fill, `pwrite` | **ENOSPC** | - | ok | - | - | zeros | error at write |
| b. hole punched (`F_PUNCHHOLE`, 4 MiB), fill, `mmap` write into the hole | ok, no `SIGBUS` | **ok** | **ENOSPC** | ENOSPC | new bytes | zeros | **silent loss until fsync** |
| b. same, `pwrite` into the hole | **ENOSPC** | - | ok | - | - | zeros | error at write |
| b. hole, then `F_PREALLOCATE` with `F_PEOFPOSMODE` (4 MiB), fill, `mmap` write | ok | ok | **ENOSPC** | ENOSPC | new bytes | zeros | **silent loss**: the blocks went after EOF, not into the hole (`SEEK_HOLE` still at 4 MiB) |
| b. hole, then `F_PREALLOCATE` with `F_VOLPOSMODE` | - | - | - | - | - | - | `EINVAL` at the hole offset and at offset 0 |
| b. hole, then `pwrite` of zeros into the hole, `F_FULLFSYNC`, fill, `mmap` write | ok | ok | ok | - | - | equal | **ok** |
| c. written arena, `clonefile`, fill, `mmap` write over 4 MiB | ok | **ok** | **ENOSPC** | ENOSPC | new bytes | old bytes | **silent loss until fsync** |
| c. same, `pwrite` | **ok** | - | **ENOSPC** | ENOSPC | new bytes | old bytes | silent loss until fsync (also for `pwrite`) |
| c. new `Reserve` arena, `clonefile`, fill, `mmap` write | ok | **ok** | **ENOSPC** | ENOSPC | new bytes | zeros | **silent loss until fsync** |
| c. same, `pwrite` | **ok** | - | **ENOSPC** | ENOSPC | new bytes | zeros | silent loss until fsync (also for `pwrite`) |
| d. `Reserve` arena mapped, 8 MiB written and synced, then `hdiutil detach -force` | - | - | - | - | - | - | **`SIGBUS`** at the first read of a mapped page after the detach |
| d. same, with a `SIGBUS` handler | the handler ran 2047 times (reads) and 2048 times (writes); the process did not stop | ok (anonymous pages) | `EBADF` | `EBADF` | `pread`: `EIO` | - | `pwrite` and `pread` return `EIO` |
| h. map of 2 pages on a 1-page file, store into page 2 | - | - | - | - | - | - | `SIGBUS` without a handler; with the handler, the store returns and the fault is counted |

Facts from the matrix:

1. On APFS, a full disk never caused `SIGBUS` for an `mmap` write. In 5 `mmap` cases the data did not get to the disk. In all 5, `msync(MS_SYNC)` returned success and only `F_FULLFSYNC` returned `ENOSPC`.
2. The `ENOSPC` from `F_FULLFSYNC` stays: a second call also fails. The page cache still holds the new bytes, so a read before the remount shows data that is not on disk.
3. `mmap` writes are safe on a full disk only where the file has its own blocks: a new `Reserve` arena that is not cloned, a `Zeroed` arena, or a punched hole that was filled again with `pwrite` of zeros.
4. `F_PREALLOCATE` cannot reserve space inside a punched hole. `F_PEOFPOSMODE` allocates after the physical end of the file. `F_VOLPOSMODE` returns `EINVAL`.
5. A clone (`clonefile`) of the arena makes its blocks shared. Then a write needs new blocks, and `pwrite` also gets the error only at `F_FULLFSYNC`. An APFS snapshot (for example a Time Machine local snapshot) probably has the same effect. This was not tested, because a snapshot needs root.
6. `SIGBUS` does occur when the device goes away. A handler that maps an anonymous page over the fault address (`MAP_FIXED`) and returns keeps the process alive (cases d and h). The data of each faulted page is lost.

## Crash test (`kill -9`)

| Path | Kills | Synced ids checked | Bad or missing | Bad records found | Rounds with a torn tail | Largest torn tail |
|---|---|---|---|---|---|---|
| `mmap` | 40 | 28712 | 0 | 0 | 1 | 7692 bytes |
| `mmap` | 200 | 515936 | 0 | 0 | 4 | 3412 bytes |
| `pwbuf` | 40 | 28408 | 0 | 0 | 0 | 0 |

- Recovery cut only the torn tail. Every record before the last sync was intact after all 240 `mmap` kills.
- The `mmap` path left a torn tail 5 times: a part of a record stayed in the page cache, because the kill came during the copy. The kernel keeps these bytes after the process stops. The `pwrite` path left no torn tail. The CRC and the recovery cut handled every torn tail.
- "Synced ids checked" counts each id again after each kill.

## Code complexity

Numbers are from `src/mmap_write.rs`.

| Item | `pwbuf` | `mmap` |
|---|---|---|
| Put branch | 9 lines | 22 lines |
| Shared `encode_into` (zstd into a slice, header, CRC) | 24 lines | 24 lines |
| Extra code | none | `Map` type (`mmap`, `msync` with page alignment, `munmap`): about 70 lines; a dirty range; a new map at each new arena |
| Error at put | `pwrite` returns `ENOSPC` or `EIO` | none; a store into the map cannot return an error |
| Error at sync | `F_FULLFSYNC` | `msync` (it returned success in every full-disk case) and `F_FULLFSYNC` |
| Device lost or I/O error | `EIO` from `pwrite` or `pread` | `SIGBUS`; the process stops if there is no handler |

More design costs of `mmap` in the production store:

- The store compresses outside the lock today, so many threads can compress at the same time. With `mmap`, zstd writes at the record offset, so the offset must be fixed before compression. There are two options. Compress under the lock (then compression has no parallelism). Or reserve the worst case and leave a gap after the record. A gap breaks the chain of records that the recovery walk reads, so the format then needs skip records. `pwbuf` keeps one buffer for each thread and has none of these problems.
- A `SIGBUS` handler is global for the process. Rust std already installs a `SIGBUS` and `SIGSEGV` handler (for stack overflow), so a new handler must call the old one for other addresses. The fault can occur inside zstd (C code), so `siglongjmp` would leave zstd in a bad state. The anonymous-page method works (cases d and h), but the data of each faulted page is lost, and the writer must check a fault counter after each copy.
- After a failed `F_FULLFSYNC`, the store must treat every record since the last good sync as lost. This is also necessary for `pwrite` on cloned blocks. The index entries and the dedup hits that point to these records must go, and the arena must not take more records. With `mmap`, this is the only error signal for a full disk.

## Verdict

**No-go now.** The facts do not support the change:

1. The CPU gain is not from the removed buffers. `alloc` to `pwbuf` (buffers and zstd context reused) saves 0 to 14 us for each put. `pwbufV3` to `mmapV3` saves 2 to 10 us for each put. The large difference between `pwbuf` and `mmap` on `Reserve` arenas (32 to 47 us) comes from `pwrite` into blocks that are reserved but not written. `Zeroed` arenas remove that cost for `pwrite` too.
2. With a sync every 64 puts or every 1 MiB, one sync takes 21 to 42 ms (median). 64 puts on the best path cost about 64 x 41 us = 2.6 ms of CPU (random) or 64 x 112 us = 7.2 ms (compressible). The `mmap` gain on `Zeroed` arenas is 0.1 to 0.6 ms for each sync interval. That is 3 percent or less of one sync.
3. With `mmap`, no full-disk error is visible before `F_FULLFSYNC`, and `msync` hides it. A `SIGBUS` handler is necessary for device loss.

**Go only if all of these are true:**

1. Each arena has its own blocks before the first `mmap` write: `Reserve` (`F_PREALLOCATE`, then `ftruncate`) or `Zeroed`. A `Sparse` arena never takes `mmap` writes.
2. No part of a mapped arena is a punched hole, unless the hole was filled with `pwrite` of zeros and `F_FULLFSYNC` returned success before the map writes. This excludes free-space reuse (spike C) with `mmap` writes into punched space.
3. No clone and no snapshot contains the active arena. This cannot be controlled on a volume with Time Machine local snapshots. On such a volume, the "safe" `Reserve` case probably changes into the silent-loss case (not tested, see fact 5).
4. `sync` uses the result of `F_FULLFSYNC` and does not trust `msync`. On an error, the store removes all records since the last good sync from the index, seals the arena, and returns the error to the caller.
5. The process has a chained `SIGBUS` handler for the arena maps (anonymous page with `MAP_FIXED`, then a fault counter that the writer checks after each copy), or the project accepts a process crash when the device goes away.
6. The store keeps compression outside the lock with a worst-case reservation and a format for gaps, or accepts compression under the lock.
7. A measurement on the target machine (Apple Silicon, 16 KiB pages) shows a gain that is larger than the gain of `Zeroed` arenas with `pwbuf`. This spike ran on an Intel machine with 4 KiB pages.

**Recommendation:** use the `pwbuf` path (one zstd context and one record buffer for each writer) in `chunk-store`. Decide the arena type (`Reserve` or `Zeroed`) with the data from spike A, because that choice changes the put CPU more than `mmap` does.

## Files

- `src/mmap_write.rs`: the three write paths.
- `tests/mmap_e.rs`: correctness tests (9 tests).
- `src/bench/e.rs`, `run_e.sh`: bench `e1`. Raw data: `results/e/e1_*_r1.jsonl`, `results/e/e1_*_r2.jsonl`. Tables: `python3 -I results/e/summarize.py results/e/e1_*.jsonl`.
- `src/bin/ecrash.rs`: crash loop. Raw data: `results/e/crash.jsonl`.
- `src/bin/edisk.rs`: full-disk matrix. Raw data: `results/e/disk.jsonl`. Console output: `results/e/disk.txt`, `results/e/disk2.txt`.

## Not tested

- Apple Silicon and 16 KiB pages (the machine is Intel).
- Power loss.
- APFS snapshots (they need root). `clonefile` was the proxy.
- A real disk I/O error. A forced detach was the proxy.
- More than one writer thread.
