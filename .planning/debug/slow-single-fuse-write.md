---
status: investigating
trigger: "slowest single FUSE write in bigwrite is 5-8 s (1 GiB and 4 GiB, DIRTY_LIMIT 32 or 16 MiB); find the root cause"
created: 2026-11-18T21:53:57Z
updated: 2026-11-18T21:53:57Z
---

## Current Focus

hypothesis: unknown; candidates listed below, none tested yet
test: temporary timing in the daemon (gate wait, rebase/splice, put/read/sync counters, pack syncs, WAL append, commit_cut) and a per-write log in rw.py; one 1 GiB bigwrite run
expecting: the slow write's wall time is covered by one component
next_action: add instrumentation (eprintln with durations) and run `scripts/docker-linux-test/run.sh bigwrite 1024` with SLICEFS_TARGET_VOL=slicefs-target-slowwrite
bug_class: Bohrbug-or-Mandelbug (not yet classified; slowest write is 5-8 s in every run, so the trigger is deterministic; the size is noisy)

## Symptoms

expected: a single 1 MiB or 4 KiB pwrite returns in well under 2 s (rebase of 32 MiB scattered extents estimated at tens of ms)
actual: slowest single write 5418, 5870, 7253 ms at DIRTY_LIMIT 32 MiB; 7778 / 5726 ms at 16 MiB; slowest fsync 138-1293 ms; before #98 slowest write 17 s
errors: none (performance)
reproduction: scripts/docker-linux-test/run.sh bigwrite [1024] (release build, Docker/OrbStack); rw.py prints "max write N ms"
started: present since bounded writes (Phase 14); 17 s before #98

## Candidates (from task, not yet tested)

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

## Resolution

root_cause:
fix:
verification:
files_changed: []
