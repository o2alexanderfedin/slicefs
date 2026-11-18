---
status: awaiting_human_verify
trigger: "slowest single FUSE write in bigwrite is 5-8 s (1 GiB and 4 GiB, DIRTY_LIMIT 32 or 16 MiB); find the root cause"
created: 2026-11-18T21:53:57Z
updated: 2026-11-18T22:45:00Z
---

## Current Focus

hypothesis: confirmed - the slow write is the synchronous rebase (splice) in the write that passes DIRTY_LIMIT; its floor is the splice work, its maxima are host stalls inside it
test: done (runs 1-6, see Evidence)
expecting: owner check of the fix and of the open options
next_action: owner runs `scripts/docker-linux-test/run.sh bigwrite` on main after a merge, or on a quiet host, and decides on the not-done options
bug_class: Bohrbug (every rebase is slow, deterministic work) + environment-dependent maxima (cache drops, fsync stalls)
reasoning_checkpoint:
  hypothesis: "A write is slow because it runs a splice that cuts ~92 MiB again, stores ~95 MiB and reads ~287 MiB per 32 MiB of 4 KiB writes, inside the FUSE callback"
  confirming_evidence:
    - "every write > 1 s in 6 runs is a rebase; python - daemon residual 0-1 ms; gate wait 0"
    - "per-rebase time is linear in DIRTY_LIMIT (16 MiB median 1.0 s, 32 MiB 2.2-2.7 s)"
    - "splice components add up to the splice time (put + src + cdc within ~60 ms)"
  falsification_test: "a slow write with no rebase line at the same offset, or a large residual outside the daemon"
  fix_rationale: "the read-ahead read 3x the bytes it needed; reading to the region end + CHUNK_AVG halves reads and read count with the same cuts"
  blind_spots: "the host stalls (cache drops, VM disk) are not controlled; before/after maxima are single runs"
  candidate_causes:
    - "code: synchronous splice cost (re-cut amplification, read-ahead, per-object costs)"
    - "environment: VM page-cache drops and VM disk fsync stalls"
    - "config/harness: reference file on the same disk; DIRTY_LIMIT"
  and_gate: "yes - the maxima need both the splice floor (code) and a stall inside it (environment)"

## Symptoms

expected: a single 1 MiB or 4 KiB pwrite returns in well under 2 s (rebase of 32 MiB scattered extents estimated at tens of ms)
actual: slowest single write 5418, 5870, 7253 ms at DIRTY_LIMIT 32 MiB; 7778 / 5726 ms at 16 MiB; slowest fsync 138-1293 ms; before #98 slowest write 17 s
errors: none (performance)
reproduction: scripts/docker-linux-test/run.sh bigwrite [1024] (release build, Docker/OrbStack); rw.py prints "max write N ms"
started: present since bounded writes (Phase 14); 17 s before #98

## Candidates (from the task; results under Eliminated and Evidence)

- rebase inside the write (splice per-extent cost, tree walks, base chunk reads, decompression)
- wait on the commit gate while a timed commit (exclusive) runs, including relog of dirty extents
- RetryingPack 8 MiB early sync (fsync on btrfs in the VM)
- pack rollover at 64 MiB (seal: sync + index write_durable + dir sync)
- online GC (bigwrite mounts with --gc-interval 0, so expected off)
- kernel FUSE writeback/queueing; Linux dirty-page throttling (balance_dirty_pages) on WAL/pack writes
- Python write timing in rw.py itself

## Eliminated

- hypothesis: wait on the commit gate while a timed commit (incl. relog) runs
  evidence: run 1: gate_wait 0.0 ms for all 15 writes > 1 s; the only gate-wait write was 99 ms (commit 98 ms, relog 0 MiB). The commit timer waited 2.4 s / 1.1 s for rebases instead.
  timestamp: 2026-11-18T22:20:00Z

- hypothesis: kernel FUSE queueing or Python timing in rw.py
  evidence: run 1: python duration minus daemon (gate_wait + handle) is 0-1 ms for every slow write.
  timestamp: 2026-11-18T22:20:00Z

- hypothesis: Linux dirty-page throttling on WAL/pack writes
  evidence: run 1: Dirty ~40 MiB, Writeback 0 during the 5.5 s rebase; wal_append <= 3.4 ms; pack write_max_ms <= 31 ms.
  timestamp: 2026-11-18T22:20:00Z

- hypothesis: background segment GC thread (spawn_background_gc, every 60 s) stalls a write
  evidence: metadata/src/gc/background.rs uses orphan_count = 1 and mount.rs passes orphan_threshold 1000; `1 > 1000` is false, so run_gc_roots_only never runs. Online GC is off in bigwrite (--gc-interval 0).
  timestamp: 2026-11-18T22:10:00Z

- hypothesis: pack rollover at 64 MiB (seal) as a separate cause
  evidence: seals run inside the rebase (1-2 per rebase, 17-58 ms typical, 417 / 885 / 1316 ms at most); outside rebases the slow writes from a seal or an early sync during cp are 50-711 ms. A part of the rebase, not a separate slow-write source.
  timestamp: 2026-11-18T22:40:00Z

Not eliminated, but parts of the rebase cost (see Evidence): the RetryingPack 8 MiB early sync (11-14 fsyncs per rebase,
0.2-0.9 s, up to 3.3 s with the harness's dirty reference file; one 9.4 s at 4 GiB), base chunk reads (9x of the dirty bytes
before the fix, cold after a VM cache drop), zstd tries (~0.4 s), ~8300 pack pwrites (0.2-0.6 s).

## Evidence

- timestamp: 2026-11-18T21:53:57Z
  checked: code reading (filesystem.rs test_write/rebase_overlay/commit_cut, chunk-store file.rs splice, lib.rs put/append, retry.rs, commit_timer.rs, fuser session)
  found: fuser runs 1 thread; every callback holds commit_gate shared; the commit timer (30 s) holds it exclusive for commit_cut; rebase runs inside the write that passes DIRTY_LIMIT; ChunkStore::put holds the store mutex across the pack write_at, and RetryingPack::write_at calls fsync when 8 MiB is kept; chunks are 4/8/32 KiB FastCDC, so a 4 KiB extent re-chunks about 2-3 chunks (~16-24 KiB); 16 rounds of 1 MiB + 256x4 KiB fill 32 MiB, so one rebase re-chunks ~100 MiB, which matches the CAS growth in 14-07 (2.67 GB for 1 GiB + 512 MiB)
  implication: one rebase writes ~100 MiB of new pack bytes -> ~12 early syncs and 1-2 pack seals per rebase; the "tens of ms" estimate is low by the chunk expansion, but CPU alone is still well under 1 s

- timestamp: 2026-11-18T22:02:00Z
  checked: run 1, instrumented 1 GiB bigwrite, DIRTY_LIMIT 32 MiB (probe commits 30c3d64)
  found: |
    rw.py: total 62.1 s; 16 writes > 50 ms, sum 41.8 s; max 5568 ms. The 15 writes > 1 s come one per ~32 MiB written
    (written_before 32, 65, 97, 129, ... 484 MiB) and are all rebases; 9 of the 15 are 4 KiB writes.
    Residual python - (gate_wait + handle) is ~0-1 ms for each (5568 vs 5567.5, 4224 vs 4223.2, 4103 vs 4102.7).
    gate_wait for all of them 0.0 ms; one 99 ms write was the only gate wait (commit 98 ms).
    commit_cut: 34 ms and 98 ms; relog 0 MiB (the rebase before it had just emptied the overlay). The commit timer waited
    2421 ms and 1053 ms for the gate, i.e. it waited for a rebase, not the reverse.
    Per rebase (16 of them): extents ~3980, ext 31.5-32.6 MiB, rechunked 90.5-94.1 MiB, new objects ~8300 / 93-97 MiB,
    reads ~26500 / 284-295 MiB, node loads 308-371, reused_nodes 0-3 (every leaf is rewritten), 11-13 pack syncs, 1-2 seals.
    Splice wall 1771-5566 ms. Typical (1809 ms): put 1119 (zstd 381, hash 51, lock 3, append 655 = write 422 + sync 205 + seal 28),
    src 462 (read 416), cdc 171, other ~57.
    Slow outliers: 5566 ms has read_ms 3393 (normal ~420-560) for the same 286 MiB; 3670 ms has sync_ms 1622 (two syncs
    719 and 692 ms); first rebase 4220 ms has sync 1143 + seal 417 + read 1779.
    Dirty pages stayed ~40 MiB during the 5.5 s rebase (no writeback throttling); wal_append <= 3.4 ms in rebase writes.
  implication: |
    The slow write IS the rebase in the write that crosses DIRTY_LIMIT; nothing else (gate, commit, relog, GC, kernel,
    python) adds more than 0.1 s. A rebase of 32 MiB of 4 KiB-scattered extents costs ~1.8 s at best on this host, because it
    re-chunks ~92 MiB (2.9x: a 4 KiB extent re-cuts ~23 KiB of 8 KiB-average chunks), stores ~8300 new objects (zstd try,
    pwrite, 12 early pack fsyncs), and reads + BLAKE3-verifies ~287 MiB of base chunks (9x of the dirty bytes: each region
    reads 2*CHUNK_MAX = 64 KiB ahead but uses ~23 KiB). Host I/O and CPU variance multiplies one component (read or sync)
    by 3-8x, giving the 3.5-5.6 s maxima. 2/3 of the random-write wall time is spent in rebases.

- timestamp: 2026-11-18T22:09:00Z
  checked: run 2, repeat of run 1 (32 MiB) with per-rebase on-CPU / run-queue split (/proc/thread-self/schedstat), probe a630b88; host load 70-83
  found: |
    total 68.6 s; 16 writes > 50 ms, sum 48.2 s; max 6091 ms; 15 writes > 1.9 s, all rebases, gate_wait 0.
    Per rebase: rechunked 91-94 MiB, read 285-295 MiB, 11-13 syncs. splice 1928-6089 ms.
    on-CPU 1815-4240 ms, run-queue wait <= 1.3 ms (no contention inside the VM).
    6089 ms rebase: on-CPU 2931, sync 3319 (one sync 1636 ms) -> ~3.2 s blocked in pack fsync.
    4123 / 4933 ms rebases: on-CPU 3808 / 4240 with read 2013 / 2438 ms for the same 287 MiB -> the same work
    took 2x CPU time (host takes CPU from the VM; the guest does not see it as steal).
    Commit timer waited 3757 ms and 1589 ms for rebases; commits took 29 and 139 ms.
  implication: the max write repeats (5.6 s, 6.1 s). Floor ~1.9 s on-CPU per 32 MiB rebase; the max adds fsync stalls (up to 3.3 s) or a 2x CPU slowdown.

- timestamp: 2026-11-18T22:12:00Z
  checked: run 3, same build, DIRTY_LIMIT 16 MiB via SLICEFS_PROBE_DIRTY_MIB; host load 56-81
  found: |
    total 56.0 s; 33 writes > 50 ms, sum 37.5 s; max 2964 ms.
    Per rebase (31): extents ~2015, rechunked 46-48 MiB, read ~151 MiB, 6-7 syncs; splice 830-2957 ms, typical 0.9-1.1 s,
    on-CPU 0.71-2.19 s. Outliers: 2507 ms (sync 1550, one sync 1199 ms), 2957 ms (read 1450, on-CPU 2191),
    2236 ms (read 1387, on-CPU 2145).
  implication: |
    Rebase cost is linear in the dirty extents (16 MiB: half the bytes, half the time; ~55 ms on-CPU per MiB of 4 KiB
    scattered extents on this host). The max write is (work proportional to DIRTY_LIMIT) + (a host stall: fsync 1-3 s or
    a 2x CPU slowdown). A smaller limit gives more rebases, so more chances to meet the worst stall; this is why the
    14-07 16 MiB run (7778 ms) did not improve on 32 MiB. 14-07 ran each config once on a host with load 55-93.

- timestamp: 2026-11-18T22:15:00Z
  checked: Cached / MemFree / iowait columns of the meminfo sampler in runs 2 and 3, around the slow-read rebases
  found: |
    The page cache of the VM is dropped in steps of ~3 GiB while MemFree rises by the same amount:
    run 3 22:11:22.3 -> 23.3 Cached 4000 -> 814 MiB (MemFree 9206 -> 12605); run 2 22:07:35.9 -> 36.5 Cached 4543 -> 942 MiB
    and 22:08:21.6 -> 22.1 Cached 3899 -> 1060 MiB. Each drop is followed by the slow-read rebases: run 3 read_ms
    1450, 1387, 885, 607, 423, 328, 272 (decays as the packs come back into the cache; normal ~220 at 16 MiB);
    run 2 read 1237 / 2013 after the 22:07:36 drop and 1462 / 2438 after the 22:08:22 drop. iowait does not rise
    (the slow reads are on-CPU in the guest). Only the read component slows (zstd/cdc/hash 1.0-1.3x).
    No process of the test drops caches; this is consistent with OrbStack returning VM memory to macOS
    (dynamic memory), not confirmed.
  implication: the read outliers are cold base-chunk reads after a page-cache drop of the VM, multiplied by the 9x read volume of the splice.

- timestamp: 2026-11-18T22:21:40Z
  checked: run 4, 4 GiB, 32 MiB, ref on disk (before the fix)
  found: |
    total 95.4 s; 19 writes > 50 ms, sum 77.4 s; max 17056 ms (the last write of the run). All slow writes are rebases.
    Per rebase: rechunked 93-96 MiB, read 319-325 MiB, reused_entries ~377k, reused_nodes 62; splice 2146-6588 ms
    excluding the max, on-CPU 1966-5901 ms, read 680-3886 ms (cold reads; Cached drops at 22:20:29 from 8545 to 2538 MiB).
    The 17050 ms rebase: one pack sync of 8 MiB took 9388 ms (sync total 9889), read 4602, on-CPU 7348 ms. During the
    sync Dirty was flat at 42 MiB, Writeback 0, and iowait rose ~1 CPU for ~8 s: the fsync waited on the virtual disk
    with no guest writeback (host-side I/O stall).
  implication: at 4 GiB the same mechanism holds; the worst case adds a single 9.4 s fsync stall of the VM disk inside the rebase (the 17 s "before #98" number can still happen on this host).

- timestamp: 2026-11-18T22:30:00Z
  checked: splice read-ahead (chunk-store file.rs) with a counting SpliceSource, 16 MiB base, 1000 x 4 KiB + 2 x 256 KiB edits; local release bench 256 MiB base, 3980 x 4 KiB edits
  found: |
    Before: the splice asks 2.76x the re-chunked bytes (31.6 MB for 11.5 MB); the 2 * CHUNK_MAX = 64 KiB read per step is
    dropped after the resync (~23 KiB on). After reading to the region end plus CHUNK_AVG steps: 1.18x (13.6 MB).
    Local bench (MemIo, rounds 1-2): 248-275 ms -> 164-165 ms per splice; source bytes 214 MB -> 83 MB; cdc 126-135 -> 74-76 ms.
    All 11 file:: splice tests (same key as a full write) pass.
  implication: the read volume of a rebase can drop ~2.4x with the same cut points; this is the contained part of the cost.

- timestamp: 2026-11-18T22:50:00Z
  checked: run 5, after the fix (e43a607, probes still in), one container: 1 GiB, 1 GiB, 4 GiB; 32 MiB; ref on disk
  found: |
    | run | size | rw total | max write | splice median / max | read MiB | read_ms median | src MiB / rechunked |
    | 1 (before) | 1 GiB | 62.1 s | 5568 | 2197 / 5566 | 287 | 568 | - / 92 |
    | 2 (before) | 1 GiB | 68.6 s | 6091 | 2742 / 6089 | 287 | 590 | - / 92 |
    | 4 (before) | 4 GiB | 95.4 s | 17056 | 3860 / 17050 | 323 | 1550 | 268 / 94 |
    | 5a (after) | 1 GiB | 54.4 s | 3837 | 2010 / 3835 | 140 | 278 | 108 / 92 |
    | 5b (after) | 1 GiB | 53.4 s | 4731 | 1916 / 4728 | 141 | 296 | 108 / 92 |
    | 5c (after) | 4 GiB | 87.5 s | 6547 | 4244 / 6543 | 169 | 1611 | 111 / 94 |
    Read count per rebase 26.7k -> 12.5k (1 GiB), 28.9k -> 14.1k (4 GiB); cdc_ms median 198-201 -> 100-118.
    The slow writes are still the rebases (15-16 per run). Outliers after the fix: cold reads after a cache drop
    (read_ms 1954 / 2570 for 140 MiB) and pack sync (2654 ms total in one rebase).
    At 4 GiB the read count halved but read_ms stayed (~1.6 s median): reads there are mostly cold, ~114 us each in
    5c against ~54 us in run 4 (cache state of the host differs between single runs).
  implication: |
    The fix halves the read bytes and read count of a rebase and cuts the 1 GiB median rebase by ~10-30% and the
    random-write time by 13-22%. It does not remove the floor (~1.5 s on-CPU per 32 MiB rebase: zstd + pwrite + fsync +
    CDC) or the host stalls. The lower maxima (3.8 / 4.7 s, 6.5 s at 4 GiB) are single runs; the 17 s of run 4 was a
    9.4 s fsync stall, so its drop is not attributable to the fix.

- timestamp: 2026-11-18T22:37:00Z
  checked: run 6, after the fix, 1 GiB twice, ref.bin on a tmpfs (not on the VM disk); one variable against run 5a/5b
  found: |
    rw total 52.6 / 47.1 s; max write 3889 / 3422 ms; splice median 2211 / 1897, max 3887 / 3421.
    Pack sync per rebase: max 421 / 546 ms (run 5a/5b with ref on disk: 668 / 2654; runs 1/2 before: 1622 / 3319).
    rw.py fsync max 173 / 170 ms (ref on disk: 1205 / 343 after, 1293 in 14-07). No pack sync > 550 ms per rebase.
    The cache drops still happen (Cached -1 to -3 GiB) and the slowest rebases are the cold-read ones (read_ms 2088 / 1796).
  implication: the 1-3 s pack fsync stalls come mostly from the harness: rw.py writes the 1 GiB reference file to the same VM disk without fsync, so its dirty pages are flushed while the daemon syncs a pack. The cold reads after a cache drop remain.

## Resolution

root_cause: |
  The slow write is always the write that passes DIRTY_LIMIT: it runs rebase_overlay -> content::splice in the single
  FUSE thread before it returns. Nothing else adds more than ~0.1 s (python - daemon residual 0-1 ms; commit gate wait
  0 ms; commits 29-139 ms with 0 MiB relog; GC off; WAL append <= 3.4 ms). The splice of 32 MiB of 4 KiB-scattered extents
  is not tens of ms: it cuts ~92 MiB again (a 4 KiB write re-cuts ~23 KiB of 8 KiB-average chunks), stores ~8300 new
  objects (~95 MiB: zstd try, ~8300 pwrites, 11-14 early 8 MiB pack fsyncs, 1-2 pack seals), and reads + BLAKE3-verifies
  ~287 MiB of base chunks (~26700 store reads; 3.1x the re-cut bytes because each re-chunk step read 2 * CHUNK_MAX = 64 KiB
  ahead). That is ~1.5-2.5 s on-CPU on this host (median splice 2.2-2.7 s at 1 GiB, 3.9 s at 4 GiB). The 5-8 s maxima
  (17 s at 4 GiB) are this floor plus a host effect inside one rebase: cold base reads after the VM page cache drops by
  ~3 GiB (read 0.5 -> 2.4-3.9 s), or pack fsync stalls (one 8 MiB fsync 1.2-9.4 s) that come mostly from the harness's
  unsynced reference-file writes on the same disk. DIRTY_LIMIT: rebase time is linear in it (16 MiB: median 1.0 s), but
  the max is set by the stalls, so one run per limit cannot rank limits; 14-07's 7778 ms at 16 MiB was not reproduced (2964 ms).
fix: |
  chunk-store splice: read to the end of the changed region in big pieces and past it only CHUNK_AVG at a time (was a
  fixed 2 * CHUNK_MAX read-ahead). Same cut points, same keys. Source bytes 2.76x -> 1.18x of the re-chunked bytes (test).
  In the mount: read 287 -> 140 MiB and 26.7k -> 12.5k store reads per rebase; random-write time 62.1 / 68.6 s -> 54.4 / 53.4 s
  (1 GiB) and 95.4 -> 87.5 s (4 GiB); median rebase 2197 / 2742 -> 2010 / 1916 ms (1 GiB). Max write 5568 / 6091 -> 3837 / 4731 ms
  (1 GiB) and 17056 -> 6547 ms (4 GiB), single runs dominated by host stalls (not attributable to the fix alone).
  Not done (design trade-offs, for the owner): rebase off the FUSE thread; a lower DIRTY_LIMIT; fewer early pack fsyncs
  per rebase (KEEP_LIMIT vs RAM) or batched pack appends; no base read under extents in RangeView::read (~32 MiB of the
  140 MiB); reference file on tmpfs in bigwrite.sh.
oracle_type: derived (the splice key must equal a full write of the same bytes; the read bound is a counter, not wall time)
verification: |
  RED: scattered_small_edits_read_little_more_than_they_rechunk failed at 2.76x before the change; GREEN at 1.18x after.
  Same-key splice tests (11 in file::) pass; cargo test -p slicefs-cli -p chunk-store on the clean tree (probes reverted).
  Docker bigwrite after the fix: 1 GiB x 4 (2 with ref on tmpfs) and 4 GiB x 1, all PASS (cmp, remount cmp, VmHWM <= 161 MiB).
files_changed: [crates/chunk-store/src/file.rs]
