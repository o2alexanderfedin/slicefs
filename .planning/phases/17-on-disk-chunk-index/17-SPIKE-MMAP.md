# Phase 17 spike: a read-only mmap read path for the sealed runs

Spike after plan 17-01 (see `17-SPIKE.md` for the run layout and the
17-01 numbers). Local branch `spike/17-mmap`, made from `spike/17-index`
(`8a1f656`). Not merged, not pushed.

Question: can a dedup hit get back to about 0.8 µs (the in-RAM HashMap of
the Phase 16 baseline) while the RAM per object stays at a few bytes?

Short answer: yes, about 0.8 µs or less in most cases. With the run mapped
read-only and interpolation search at both levels (warm page cache):

| Value (interpolation / interpolation) | macOS 1M | macOS 10M | Linux 1M | Linux 10M |
|---|---|---|---|---|
| `contains` hit, pass 2 (pages already mapped: a running mount) | 0.43-0.44 µs | 0.64-0.66 µs | 0.51 µs | 0.81 µs |
| `contains` hit, pass 1 (fresh process, the 17-01 method) | 0.46-0.48 µs | 0.93-0.95 µs | 0.51 µs | 0.82 µs |
| put dedup-hit (the real dedup path, with the BLAKE3 hash) | 0.53-0.56 µs | 0.76 µs | 0.58 µs | 0.89 µs |
| HashMap put dedup-hit, same session | 0.40-0.41 µs | 0.46-0.48 µs | 0.50 µs | 0.63 µs |

The macOS 10M pass 1 value is above 0.8 µs: it includes one soft page
fault for each first touch of a mapped page (see the notes). The anonymous
RAM of the index stays at 1.37 to 1.46 B per object. The HashMap of today
is still faster in the same session (`contains` 0.30 to 0.39 µs on macOS),
by about 1.4 to 1.7 times.

## What was built

All code is in `benchmarks/index-bench` on the spike branch.

- Backend `spike-runs-mmap`. It opens the same run files as `spike-runs`
  (the bench accepts a `spike-runs` store for it). The run files are
  written as before (buffered writes, fsync, rename) and are never changed.
- At open, each sealed run is mapped with `mmap(PROT_READ, MAP_SHARED)`
  (page area only). The Bloom filter and the fence table are read into
  RAM as before.
- Lookup: Bloom filter (RAM), then the fence table (RAM) gives the page,
  then a search inside the page directly in the mapping. No `pread`, no
  copy, no page CRC check.
- Run format change for this spike: the last 4 bytes of each 4 KiB page
  (in the 36-byte padding) hold the CRC32C of the first 4092 bytes. This
  costs no disk space. `spike-runs` (pread) ignores it.
- Variants, set with environment variables (one store for all of them):
  - `SPIKE_CRC=1`: variant B2, check the page CRC32C on each page search.
  - `SPIKE_FENCE=bin|interp|eytz`: fence search by binary search (as in
    17-01), interpolation search, or an Eytzinger (BFS order) copy of the
    fences with a software prefetch of the node 3 levels down.
  - `SPIKE_PAGE=bin|interp`: search inside the page by binary search (as
    in 17-01) or interpolation from the page fences, then a linear walk.
  - `SPIKE_ADVISE=random`: `madvise(MADV_RANDOM)` on each mapping.
  - `SPIKE_COLD=1`: before the hit round, unmap and map each run again.
    On Linux, also `posix_fadvise(POSIX_FADV_DONTNEED)` on the run file,
    which drops its clean pages from the page cache.
- The lookup step now also gives: a second hit pass in the same process,
  CPU ns for each round, minor and major faults per hit pass, the open
  time, and the memory split before open, after open, after the hits and
  at the end. With `--breakdown` it times the parts of one hit (below).
- The crate `crc32c 0.6` is now also a dependency of index-bench. It was
  already in `Cargo.lock` (chunk-store uses it), so no new crate came in.

## Method

- Same object scheme and lookup method as 17-01: N unique 8-byte objects,
  1M hits sampled with replacement from the store, 1M misses, then 1M puts
  of present objects (dedup-hit path, includes the BLAKE3 hash). Warm page
  cache. Median of 3 rounds, the backends interleaved in each round.
- Hit pass 1 is the first pass after open: each first touch of a mapped
  page is a soft page fault, even with a warm page cache. Hit pass 2 runs
  the same sample again in the same process, with the pages mapped. Pass
  2 is the state of a mount that runs for some time.
- The stores were made new for this spike (the 17-01 run files have no
  page CRC). One spike-runs store per size, used by all spike variants.
  The chunk-store stores are the Phase 16 stores.
- Memory split:
  - macOS: `task_info(TASK_VM_INFO)`, field `internal` (anonymous) and
    field `external` (file-backed), and `phys_footprint` from
    `proc_pid_rusage`.
  - Linux: `RssAnon` and `RssFile` from `/proc/self/status`.
  - "Index anon B/object" is the anonymous growth from before to after
    the open. The other values include the lookup sample of the step
    (about 74 MB of IDs and numbers), the same for every backend.
- Runs:
  - macOS run 1: all backends, B2, MADV_RANDOM, breakdowns, cold.
  - macOS run 2: the interpolation code changed from a `u128` division
    (a slow library call) to `f64` arithmetic. Run 2 measures again the
    chunk-store, `mmap` and the interpolation variants, and all
    breakdowns. The interpolation numbers in this file come from run 2.
  - Linux: one run (3 rounds) in Docker.

## Machine and load

macOS: Intel Xeon W-3245 (16 cores, 32 threads), 192 GiB RAM, macOS 26.2
(x86_64), APFS on an internal PCIe SSD. Release build, rustc
1.95.0-nightly.

Linux: Docker on OrbStack on the same host, image `slicefs-fusetest`, 32
CPUs, about 12 GB available memory. Store on a Docker volume (on the
OrbStack virtual disk). No FUSE, no privileges.

Load average (1, 5, 15 min), macOS host:

| Step | Before | After |
|---|---|---|
| run 1, 1M (lookups, breakdowns, cold) | 22.63 28.71 28.39 | 25.76 27.70 28.02 |
| run 1, 10M | 25.76 27.70 28.02 | 24.10 25.50 26.94 |
| run 2, 1M | 23.65 25.04 26.63 | 25.92 25.42 26.60 |
| run 2, 10M | 25.92 25.42 26.60 | 27.39 25.89 26.53 |

Linux (load inside the VM): 4.04 2.59 1.90 before, 3.45 3.12 2.30 after.
The host load at the time of the Linux run was not recorded.

The load was lower than in 17-01 (about 25 against about 80). The CPU
time per operation is within about 1 % of the wall time in the medians
(one B2 round at 10M differs by about 7 %), so the tables give wall time.

## Results: macOS

Hit = `contains` of a present ID. Values in ns per operation, median of 3
rounds (range in brackets where useful).

Run 1:

| Backend | Objects | Hit pass 1 | Hit pass 2 | Miss | Put dedup-hit | Open s |
|---|---|---|---|---|---|---|
| chunk-store (HashMap) | 1M | 299 (292-310) | 306 | 122 | 397 | 0.444 |
| spike-runs (pread) | 1M | 2599 (2394-2608) | 2542 | 84 | 2509 | 0.001 |
| spike-runs-mmap, no CRC | 1M | 576 (549-600) | 554 | 50 | 661 | 0.001 |
| spike-runs-mmap, CRC per hit (B2) | 1M | 972 (872-981) | 910 | 51 | 1044 | 0.001 |
| spike-runs-mmap, MADV_RANDOM | 1M | 607 (561-619) | 581 | 50 | 690 | 0.001 |
| chunk-store (HashMap) | 10M | 383 (364-385) | 382 | 171 | 462 | 5.086 |
| spike-runs (pread) | 10M | 3109 (3101-3248) | 2929 | 111 | 3067 | 0.009 |
| spike-runs-mmap, no CRC | 10M | 1276 (1105-1281) | 949 | 97 | 1058 | 0.008 |
| spike-runs-mmap, CRC per hit (B2) | 10M | 1581 (1508-1658) | 1300 | 111 | 1374 | 0.009 |
| spike-runs-mmap, MADV_RANDOM | 10M | 1285 (1248-1357) | 916 | 91 | 1033 | 0.009 |

Run 2 (interpolation with `f64`):

| Backend | Objects | Hit pass 1 | Hit pass 2 | Miss | Put dedup-hit | Open s |
|---|---|---|---|---|---|---|
| chunk-store (HashMap) | 1M | 311 (307-321) | 318 | 149 | 414 | 0.471 |
| mmap, binary / binary | 1M | 624 (621-654) | 611 | 48 | 715 | 0.002 |
| mmap, interpolation / interpolation | 1M | 478 (436-480) | 444 | 54 | 534 | 0.001 |
| chunk-store (HashMap) | 10M | 393 (392-405) | 390 | 179 | 484 | 5.274 |
| mmap, binary / binary | 10M | 1331 (1282-1339) | 1017 | 112 | 1100 | 0.010 |
| mmap, interpolation / interpolation | 10M | 925 (916-962) | 638 | 100 | 762 | 0.008 |

"binary / binary" means binary search of the fences, then binary search in
the page.

Open time of the `open` step (median of the last 3 of 4 opens): spike-runs
and spike-runs-mmap both 0.001 s at 1M and 0.008 s at 10M. The `mmap`
calls add no measurable time.

Notes:

- Pass 1 against pass 2: at 10M, 1M hits touch almost all 142,858 pages,
  and pass 1 has 142,714 minor faults (one per 4 KiB page, macOS does no
  fault-around here). That is about 2.2 µs per fault, and adds about 0.3
  µs to each hit of pass 1. At 1M, the 14,286 faults are spread over 1M
  hits, so pass 1 is close to pass 2. Pass 2 has 11 to 45 faults.
- B2 (CRC32C of 4092 bytes on each hit) adds 300 to 400 ns per hit at
  both sizes (pass 2: 910 - 554 = 356 at 1M, 1300 - 949 = 351 at 10M).
  That is +37 % to +64 % of the hit time.
- MADV_RANDOM has no effect with a warm cache (within the noise).
- A miss does not touch the mapping (the Bloom filter stops it), so the
  miss time is the filter time: 48 to 112 ns, lower than the HashMap.
- The pread path in this session: 2.5 to 3.1 µs per hit (17-01, under a
  load of about 80: 5.5 to 5.9 µs).

## Results: memory split

| Backend | OS | Objects | Index anon B/object (after open) | File-backed after hits | File-backed B/object | macOS phys_footprint at end | Peak RSS of the step |
|---|---|---|---|---|---|---|---|
| chunk-store | macOS | 1M | 308.5 | 0.6 MB | 0 | 383.2 MB | 465.4 MB |
| spike-runs (pread) | macOS | 1M | 1.46 | 0.5 MB | 0 | 76.2 MB | 76.8 MB |
| spike-runs-mmap | macOS | 1M | 1.46 | 59.0 MB | 58.5 | 76.2 MB | 135.3 MB |
| chunk-store | macOS | 10M | 139.2 | 0.6 MB | 0 | 1467.1 MB | 2638.2 MB |
| spike-runs (pread) | macOS | 10M | 1.37 | 0.5 MB | 0 | 88.4 MB | 89.0 MB |
| spike-runs-mmap | macOS | 10M | 1.37 | 585.0 MB | 58.5 | 88.6 MB | 673.8 MB |
| chunk-store | Linux | 1M | 153.3 (RssAnon) | 3.1 MB (RssFile) | 0 | n/a | n/a |
| spike-runs (pread) | Linux | 1M | 1.38 | 3.0 MB | 0 | n/a | n/a |
| spike-runs-mmap | Linux | 1M | 1.40 | 61.5 MB | 58.5 | n/a | n/a |
| chunk-store | Linux | 10M | 123.2 | 2.9 MB | 0 | n/a | n/a |
| spike-runs (pread) | Linux | 10M | 1.37 | 3.0 MB | 0 | n/a | n/a |
| spike-runs-mmap | Linux | 10M | 1.37 | 588.1 MB | 58.5 | n/a | n/a |

What this means:

- The anonymous memory of the daemon (RssAnon, macOS `internal`) is the
  same for mmap and pread: 1.37 to 1.46 B per object (Bloom filter and
  fences). This is the "few bytes per object" answer.
- The file-backed resident memory (RssFile, macOS `external`) grows to
  the part of the run that the lookups touched, at most the run size (58.5
  B per object). These pages are clean page-cache pages. The kernel can
  drop them at any time with no write. The pread path uses the same pages
  in the page cache, but they are not charged to the process.
- Tools that show "RSS" (top, `ru_maxrss`) show the mapped pages: 674 MB
  at 10M against 89 MB for pread. macOS `phys_footprint` (Activity Monitor
  "Memory") does not count them: 88.6 MB.
- On Linux in a container with a memory limit, page-cache pages are
  charged to the memory cgroup in both cases (pread and mmap). They are
  reclaimable, but a user can see them as memory use.
- The `open` step gives a steady RSS of 1.37 to 1.47 B per object for
  both backends (the open touches no page of the mapping).

## Results: Linux (Docker, OrbStack)

One run of 3 rounds, median.

| Backend | Objects | Hit pass 1 | Hit pass 2 | Miss | Put dedup-hit | Open s (lookup step) |
|---|---|---|---|---|---|---|
| chunk-store (HashMap) | 1M | 415 | 411 | 155 | 503 | 0.663 |
| spike-runs (pread) | 1M | 1715 | 1509 | 74 | 1571 | 0.001 |
| mmap, binary / binary | 1M | 759 | 758 | 63 | 837 | 0.001 |
| mmap, binary / binary, CRC per hit (B2) | 1M | 1183 | 1216 | 75 | 1234 | 0.001 |
| mmap, interpolation / interpolation | 1M | 509 | 510 | 61 | 582 | 0.002 |
| chunk-store (HashMap) | 10M | 564 | 527 | 209 | 631 | 7.767 |
| spike-runs (pread) | 10M | 3994 | 2336 | 148 | 2426 | 0.015 |
| mmap, binary / binary | 10M | 1366 | 1256 | 125 | 1286 | 0.004 |
| mmap, binary / binary, CRC per hit (B2) | 10M | 1803 | 1742 | 138 | 1870 | 0.005 |
| mmap, interpolation / interpolation | 10M | 817 | 806 | 125 | 893 | 0.005 |

Linux maps 16 pages per fault (fault-around): pass 1 has 893 faults at 1M
and 8,929 at 10M, so pass 1 and pass 2 are almost equal. B2 adds 420 to
490 ns per hit.

The Linux pread pass 1 values are not clean warm values. pread has no
first-touch faults, so pass 1 and pass 2 must be about equal (macOS: 3109
and 2929 ns). On Linux, pass 1 is 3.8 to 4.4 µs at 10M (all 3 rounds)
against 2.3 to 2.5 µs for pass 2, and 1.7 µs at 1M in rounds 2 and 3
against 1.1 µs in round 1. A likely cause: the cold rounds drop the run
file from the page cache, and the pread round is the first to read it
again; at 10M, the chunk-store create (2.6 GB peak) in a 12 GB VM can also
have pushed run pages out. Use pass 2 for Linux pread: 1.1 to 1.5 µs at
1M, 2.3 to 2.5 µs at 10M. The mmap rounds ran after the pread round, with
the pages back in the cache.

## Cost of one hit (breakdown)

Each part is a prefix of the hit path, timed alone over the same 1M hit
IDs in the same process (after one warm-up round), and the parts come
from subtraction:

- Bloom: the filter check only.
- (a) Fence: find the page in the RAM fence table.
- (b) Touch: (a) plus one read of one byte in the middle of the page in
  the mapping, minus (a).
- (c) Search: (a) plus the full search in the page, minus (a) plus (b).
- Full: the full `get` (Bloom, (a), (b), (c)).

These are throughput costs: the CPU runs the next lookups while a cache
miss is open, so the parts are not latencies, and the split of shared
miss time between (b) and (c) changes a little from variant to variant.
Bloom plus (a) + (b) + (c) is within 15 % of Full. Probes: the mean number
of fence reads and of entries compared.

macOS run 2, ns per hit (median of 3 rounds):

| Objects | Fence / page search | Bloom | (a) Fence | (b) Touch | (c) Search | Full | Fence probes | Page probes |
|---|---|---|---|---|---|---|---|---|
| 1M | binary / binary | 23 | 49 | 89 | 361 | 594 | 13.9 | 5.3 |
| 1M | interpolation / binary | 24 | 48 | 75 | 341 | 569 | 3.3 | 5.3 |
| 1M | binary / interpolation | 25 | 50 | 93 | 202 | 437 | 13.9 | 3.6 |
| 1M | interpolation / interpolation | 25 | 48 | 73 | 238 | 429 | 3.3 | 3.6 |
| 1M | Eytzinger / binary | 23 | 38 | 101 | 345 | 579 | 13.9 | 5.3 |
| 1M | Eytzinger / interpolation | 23 | 40 | 106 | 228 | 473 | 13.9 | 3.6 |
| 10M | binary / binary | 69 | 156 | 195 | 493 | 974 | 17.2 | 5.3 |
| 10M | interpolation / binary | 67 | 82 | 118 | 451 | 747 | 3.9 | 5.3 |
| 10M | binary / interpolation | 66 | 171 | 231 | 278 | 809 | 17.2 | 3.6 |
| 10M | interpolation / interpolation | 68 | 90 | 125 | 313 | 614 | 3.9 | 3.6 |
| 10M | Eytzinger / binary | 64 | 106 | 260 | 383 | 865 | 17.2 | 5.3 |
| 10M | Eytzinger / interpolation | 67 | 111 | 289 | 312 | 826 | 17.2 | 3.6 |

Linux, interpolation / interpolation: 1M: Bloom 26, (a) 51, (b) 85, (c)
262, Full 490. 10M: Bloom 84, (a) 70, (b) 235, (c) 337, Full 708.

Hit time of the `contains` rounds of the same breakdown runs (macOS run 2,
pass 1 / pass 2):

| Objects | binary / binary | interp. / binary | binary / interp. | interp. / interp. | Eytzinger / binary | Eytzinger / interp. |
|---|---|---|---|---|---|---|
| 1M | 637 / 603 | 583 / 553 | 491 / 475 | 457 / 425 | 638 / 600 | 511 / 488 |
| 10M | 1307 / 1013 | 1049 / 772 | 1120 / 792 | 953 / 659 | 1196 / 884 | 1133 / 859 |

How spike-runs finds the page (17-01 code): `partition_point` (binary
search) over the fence table, then a binary search over the 70 entries of
the page, comparing the full 33-byte ID.

Findings:

- The search inside the page is the largest part: 240 to 490 ns. Each
  probe reads a different 58-byte entry, so most probes are a new cache
  line and often a cache miss. Interpolation in the page (uniform digests;
  position from the page fence and the next fence, then a linear walk)
  cuts the probes from 5.3 to 3.6 and the part by about 120 to 180 ns.
- Fence search: binary search makes 14 to 17 probes over a 114 KB (1M) or
  1.1 MB (10M) table. Interpolation search (repeated, with a linear walk
  when 8 or fewer fences are left) makes 3.3 to 3.9 probes. At 10M this
  cuts (a) from 156 to 82-90 ns. At 1M the table is in the CPU cache, and
  (a) stays at about 48 ns.
- The first interpolation code used a `u128` division. That costs more
  than the probes it saves (run 1: (a) at 1M went up from 49 to 92 ns).
  With `f64` arithmetic, interpolation is better at 10M and equal at 1M.
  17-05 must not use a 128-bit division on this path.
- After interpolation, the fence search is 48 to 90 ns of a 430 to 614 ns
  hit (11 % to 15 %). Item 3 of the request (Eytzinger only if the fence
  search is still large) thus does not apply. It was measured anyway:
  Eytzinger with prefetch is the fastest fence search at 1M (38 ns) but
  slower than interpolation at 10M (106 to 111 ns against 82 to 90 ns),
  and it needs a second copy of the fences (or a BFS-order format). Not
  recommended.
- (b), the first touch of the page: 73 to 125 ns warm with interpolation.
  This is a TLB miss plus a cache miss: a 585 MB mapping in 4 KiB pages
  does not fit the TLB.
- Best variant (interpolation at both levels): 429 ns at 1M and 614 ns at
  10M for the bare `get`; 425 to 444 ns and 638 to 659 ns for `contains`
  through the bench (pass 2).

## Cold-ish case

Each round: open, remap the run (`SPIKE_COLD=1`), then 20,000 hits (so
most hits touch a new page), then a second pass of the same hits.

macOS. There is no unprivileged way to drop the page cache (`purge` needs
root), and `mincore` shows that all pages stay in the page cache. So this
measures a fresh mapping with a warm page cache (soft faults only):

| Objects | Variant | Hit pass 1 ns | Hit pass 2 ns | Minor faults pass 1 |
|---|---|---|---|---|
| 1M | default | 1605 | 545 | 10,772 |
| 1M | MADV_RANDOM | 1580 | 580 | 10,772 |
| 10M | default | 2983 | 936 | 18,677 |
| 10M | MADV_RANDOM | 2943 | 1083 | 18,677 |

Linux. `posix_fadvise(DONTNEED)` works without privileges: `mincore`
shows 0 resident pages of the run after it, in every round. So this is a
real cold read (from the OrbStack virtual disk):

| Objects | Variant | Hit pass 1 ns | Hit pass 2 ns | Major faults pass 1 | Minor faults pass 1 | Run data read into the page cache |
|---|---|---|---|---|---|---|
| 1M | default | 3940 (3865-5226) | 746 | 21 | 878 | 58 MB (all 14,286 pages) |
| 1M | MADV_RANDOM | 8284 (7953-8402) | 789 | 10,771 | 0 | 44 MB (10,771 pages) |
| 10M | default | 27769 (27405-29655) | 1198 | 202 | 7,805 | 585 MB (142,846 of 142,858 pages; RssFile 526 MB) |
| 10M | MADV_RANDOM | 16037 (12680-16818) | 1240 | 18,676 | 0 | 79 MB (18,676 pages) |

Findings:

- With default advice, Linux readahead reads large blocks around each
  fault. At 10M, 20,000 random hits read almost the full 585 MB run.
  MADV_RANDOM reads only the touched pages: 7.6 times less data and 1.7
  times faster per hit at 10M.
- At 1M (a 58 MB run), readahead is better: it reads the full run in 21
  large reads, and MADV_RANDOM is 2.1 times slower.
- So MADV_RANDOM matters only for cold pages, and only on Linux. It has no
  effect when the pages are warm. A reasonable rule for 17-05: use
  MADV_RANDOM on large runs and the default on small runs. The size limit
  needs its own measurement (between 58 MB and 585 MB here).
- A cold hit through mmap waits for one blocking page fault: about 14 to
  17 µs per major fault on this virtual disk (8 to 16 µs per hit in the
  MADV_RANDOM rounds, where some hits share a page). The pread path has the same disk read; this spike did not
  measure pread cold.

## Risks seen

- SIGBUS on read errors. A read error of the disk, or a file that becomes
  shorter than the mapping, gives SIGBUS at the access, not an error value.
  The default action stops the daemon. SliceFS never truncates or changes
  a sealed run, so only a disk error or an outside change can cause it. A
  safe SIGBUS handler (sigsetjmp around each access) is complex and not
  recommended. Proposal: accept the stop (the mount restarts, and scrub
  finds the bad page through pread), and keep scrub, merge and rebuild on
  the pread path, where a read error is a normal error value.
- Address space. On 64-bit systems there is no practical limit (47-bit
  user space is 128 TiB; 10M objects need 585 MB, 1 billion objects about
  58 GB). On 32-bit systems a mapping above about 2 GB fails. SliceFS
  targets 64-bit only.
- Merge and delete of an old run. On Linux and macOS, a file that is
  removed while it is mapped stays valid until the last `munmap`; only
  then is the disk space freed. Plan for 17-05/17-06: each run is an
  `Arc<Run>` that owns its mapping; the merge writes the new run, writes
  the new MANIFEST, swaps the run list, then removes the old files. A
  reader that holds an `Arc` to an old run keeps a valid mapping. The
  `munmap` runs when the last `Arc` drops. The space of an old run comes
  back only after the last reader is done (the time of one lookup).
- PackIo. The mapping reads the file directly, so the `MemIo` strategy
  (crash, flip, fault injection) cannot see these reads. 17-05 needs a
  small read interface: a mapped slice for `FileIo`, the in-memory bytes
  for `MemIo`. The damage tests (flip a page byte) then still work.
- No CRC check on the hot path. A page that becomes bad on the disk after
  the write can give a wrong "not found" (a duplicate is stored again; no
  data loss) or a wrong Loc (the pack read checks the record CRC and the
  hash, so it fails there). The CRC is checked at write, and scrub, merge
  and rebuild check it on every page they read.
- RSS reports. Process RSS includes the mapped pages (see the memory
  split). Documentation and the `stats` output must say which number is
  the daemon's own memory.
- `mincore` on macOS reports the page-cache state, not the pages mapped
  into the process. Use `task_info` (`external`) for the process view.

## Recommendation

Adopt the mmap read path in 17-05, with these choices:

1. Map each sealed run read-only (`PROT_READ`, `MAP_SHARED`) after the
   fsync and before it goes into the MANIFEST run list. Keep the Bloom
   filter and the fence table in RAM as now.
2. No page CRC check on the hit path. B2 costs 300 to 490 ns per hit (+37
   % to +64 %). Check the page CRC at write, in scrub, in merge and in
   rebuild (those read through pread).
3. Interpolation search at both levels (fences, then inside the page), in
   `f64` or integer multiply-shift, not a 128-bit division. Keep a binary
   search fallback for the correction walk. No Eytzinger layout.
4. MADV_RANDOM on large runs, default advice on small runs; measure the
   limit in 17-05.
5. Each run as an `Arc<Run>` that owns the mapping; unmap when the last
   reference drops after the merge installs the new MANIFEST.

With these (warm page cache): a `contains` hit is 0.43 to 0.66 µs on
macOS and 0.51 to 0.81 µs on Linux in a running process (0.93 to 0.95 µs
on macOS at 10M in the first pass of a fresh process), and a put
dedup-hit is 0.53 to 0.76 µs on macOS and 0.58 to 0.89 µs on Linux
(target: about 0.8 µs). The pread path needs 2.5 to 2.9 µs on macOS and
1.1 to 2.5 µs on Linux (pass 2) for a `contains` hit. The
anonymous memory stays at 1.37 to 1.46 B per object, against 123 to 309 B
for the chunk-store HashMap, and the open stays at 1 to 8 ms against 0.4
to 7.8 s.

Possible next step (not measured): the search inside the page is still
the largest part (240 to 340 ns). A page layout that puts the 8-byte
digest prefixes of all entries together at the start of the page (560
bytes, about 9 cache lines) would let the search read 1 or 2 cache lines
before it reads one full entry.
