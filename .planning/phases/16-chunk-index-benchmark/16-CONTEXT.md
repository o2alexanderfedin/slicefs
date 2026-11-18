# Phase 16: Chunk index benchmark - Context

**Gathered:** 2026-11-18
**Status:** Ready for planning

<domain>
## Phase Boundary

Add a standalone benchmark binary that builds chunk stores with 1M, 10M and 100M small synthetic objects through the public ChunkStore API. In a fresh process for each run, it measures cold-open time and peak RSS of ChunkStore::open_with. It also measures dedup-hit and dedup-miss lookup throughput, and the recovery time and scan throughput of a sealed pack whose .idx is missing. The results (RAM per object, open time, scan throughput) go into docs/design/chunk-store.md as the #85 baseline. The phase does not change the store code or the format.

</domain>

<decisions>
## Implementation Decisions


### Decided autonomously
- [harness location] Q: "Criterion bench, a benches/ dir in chunk-store, or a bin crate?" → Selected: "A new bin crate benchmarks/index-bench (publish = false), added to the workspace members in the root Cargo.toml next to benchmarks/chunk-bench. Deps: chunk-store (path), libc (workspace). Criterion is already a workspace dependency, but it suits micro-benchmarks, not a 100M-object setup or per-process RSS. chunk-store has no benches/ dir today." — An internal tool with no public contract. No new external crate. It can be moved or removed freely.
- [CLI shape of the bench] Q: "One process or subcommands?" → Selected: "Subcommands: `create --dir D --objects N`, `open --dir D` (one fresh process: time open_with, then report ru_maxrss), `lookup --dir D --hits K --misses K`, `recover --dir D [--packs 1|all]`, plus a `run` driver that spawns itself for each step. The reason: ru_maxrss only goes up, so each peak-RSS reading needs its own process." — Internal tool interface only.
- [synthetic objects] Q: "How to make 100M unique small objects cheaply?" → Selected: "put(&i.to_le_bytes()) for i in 0..N: an 8-byte payload, below MIN_COMPRESS=64 (lib.rs:166), so zstd never runs. Record = REC_HEADER 42 + 8 + CRC 4 = 54 B; idx entry IDX_ENTRY = 50 B (lib.rs:156-159). Use default Options (pack_limit 64 MiB, about 1.24M records per pack, about 81 packs at 100M, under MAX_OPEN_PACKS=64 only for readers, which does not matter at open). Call close() at the end, so every pack is sealed and has a .idx." — A generator detail. It uses only the public API, so it stays valid for the Phase 17 index.
- [disk/time budget] Q: "Is 100M feasible?" → Selected: "Disk is about 104 B/object: 1M ≈ 104 MB, 10M ≈ 1.04 GB, 100M ≈ 10.4 GB (5.4 GB packs + 5 GB idx). The host has about 713 GiB free, 192 GiB RAM and 32 CPUs. Create time is about 1-3 us per put (BLAKE3 of 8 B + mutex + HashMap insert + one pwrite per record via FileIo), so about 2-5 min for 100M on one thread. Build each size once into a --dir outside the repo (a scratch dir passed as an argument, never a home path in docs) and use it again for all measurements." — Only an operational choice.
- [peak RSS measurement] Q: "How to measure peak RSS on macOS and Linux?" → Selected: "getrusage(RUSAGE_SELF).ru_maxrss through libc, the same as the existing cpu_secs() in benchmarks/chunk-bench/src/main.rs:21. Units differ: bytes on macOS, KiB on Linux, so use a cfg(target_os) conversion. Report the baseline maxrss before open and the delta after open, divided by N, as RAM/object. Optionally cross-check with /usr/bin/time -l (macOS) or -v (Linux). Also report the current RSS after open (Linux /proc/self/statm; macOS mach task_info or `ps -o rss`), to show the resize transient." — Measurement method only. It is documented with the results.
- [cold vs warm cache] Q: "What does cold-open mean?" → Selected: "A fresh process, and in the run the page cache is warm unless it is dropped. On Linux, drop it with `echo 3 > /proc/sys/vm/drop_caches` when the container is privileged or root. On macOS, use `sudo purge` only when the operator runs it by hand. Record the cache state with each number. Report warm-cache open as the primary repeatable number." — Reported as a labeled condition. Both can be added later.
- [lookup metric] Q: "Which API to use for hit/miss lookup throughput?" → Selected: "Primary: ChunkStore::contains(&ObjectId) (lib.rs:841), which is one mutex lock plus a HashMap probe. Make the IDs before timing (hits: DEFAULT_HASHER.id(&i.to_le_bytes()) for a random sample of i < N; misses: i >= N), so hashing is not in the timing. Secondary: put() of existing data (dedup-hit path, lib.rs:543, which includes BLAKE3). Run on one thread. Multi-thread is optional." — Bench internals. Phase 17 uses the same calls.
- [recovery scenario] Q: "How to benchmark a pack without a valid index?" → Selected: "Remove one sealed NNNN.idx (and, as a second case, all of them), then time open_with. That runs resync_scan (lib.rs:304), which verifies the CRC and the BLAKE3 of every record, and then write_durable of a new idx (lib.rs:520). Report objects/s and MB/s of the scan. A non-read-only open writes the idx back, so remove it again before each repetition. A read_only open (lib.rs:493) skips the write and can isolate the pure scan cost." — Uses existing on-disk behavior. No format change.
- [where results go] Q: "Which doc holds the baseline?" → Selected: "docs/design/chunk-store.md: replace the wrong '### Index in memory' estimate ('about 57 bytes per object', about lines 158-164) with the measured numbers. Also add a results table in a new '### Index baseline (#66)' section after '### Index in memory' (changed 2026-11-18 by the orchestrator: Phase 15 edits '## Measurements to take' in parallel), with the machine, commit, OS and cache state. Add a short README.md in benchmarks/index-bench with run steps (no home paths, no license text). Post a comment on #66 and link it from #85." — Docs only.
- [Linux run] Q: "Run in Docker as well?" → Selected: "The primary run is on the macOS host (no mount needed, only files). The Linux run uses the scripts/docker-linux-test image. The repo is mounted read-only at /src, so the store dir goes on a container volume or /tmp. Run 1M/10M always, and 100M only if the OrbStack VM has at least 20 GB memory free. No FUSE device is needed." — An operational choice. It does not affect the code.

### Claude's Discretion
- Test names, helper layout and plan split are at Claude's discretion.

</decisions>

<code_context>
## Existing Code Insights

### Reusable Assets
- benchmarks/chunk-bench/src/main.rs (cpu_secs() getrusage pattern at line 21; libc already a dep)
- crates/chunk-store/src/lib.rs ChunkStore::open / open_with (450-532), put (536), contains (841), close (863), stats (894), Options (94-113)
- crates/chunk-store/src/hash.rs DEFAULT_HASHER (line 70), ObjectHasher::id (line 22) to regenerate hit IDs
- crates/chunk-store/src/mem.rs in-memory PackIo (possible no-disk variant, but RSS then includes the file bytes, so use FileIo for real numbers)
- scripts/docker-linux-test/run.sh + Dockerfile (Linux run environment)
- workspace dep criterion 0.5 (root Cargo.toml line 23), unused so far, available if a micro-bench of lookups is wanted

### Established Patterns
- Benchmark tools live in benchmarks/<name>/ as publish=false bin crates with a README.md that has Run/Output sections (see benchmarks/chunk-bench/README.md)
- CPU time comes from getrusage, not wall clock, in chunk-bench. For open/recovery, report both wall and CPU time
- Design docs record measurements with the machine and the commits (Phase 15 uses the same doc, docs/design/chunk-store.md)
- Open path: for each pack number in sorted order, read the .idx fully (FileIo::read = fs::read) -> decode_index into a Vec<(ObjectId,Loc)> -> insert into Inner.index HashMap one by one (Inner::add, lib.rs:977). There is no reserve(), so the map rehashes as it grows
- The last pack without an idx is the active pack: scan_pack cuts it at the first bad record. A sealed pack without an idx: resync_scan + BLAKE3 verify + the idx is written again

### Integration Points
- Root Cargo.toml [workspace] members: add 'benchmarks/index-bench'
- chunk-store public API only (ChunkStore::open, put, contains, close, stats; hash::DEFAULT_HASHER; ObjectId). Phase 17 must reuse the same binary unchanged against the new index
- Pack file naming from pack_io.rs pack_name()/Kind::Idx: the bench deletes <dir>/packs/<16-hex>.idx for the recovery case
- docs/design/chunk-store.md sections 'Index in memory' (about line 158) and 'Measurements to take' (line 313)
- GitHub #66 (close with results) and #85 (baseline link)

### Files likely modified
- Cargo.toml
- Cargo.lock
- benchmarks/index-bench/Cargo.toml
- benchmarks/index-bench/src/main.rs
- benchmarks/index-bench/README.md
- docs/design/chunk-store.md
- .planning/REQUIREMENTS.md (mark BNCH-01..03)

### Risks
- HashMap rehash transient: peak RSS (maxrss) is about 1.5x the steady-state map at 100M. Report both peak and steady values, or the #85 comparison will mislead
- Cold-cache numbers need root (Linux drop_caches) or a manual `sudo purge` on macOS. Without them, open time is warm-cache. Label it
- A non-read-only open rewrites the missing idx, so recovery repetitions must delete it again. The same open also cuts and syncs the active pack. close() after create avoids an active pack
- Docker: the repo is mounted read-only at /src and the 100M store needs about 10.4 GB disk + about 15 GB RAM in the OrbStack VM. Use a volume or /tmp and check VM limits, or run 100M on the macOS host only
- Create at 100M takes minutes and about 15 GB in the creator process. Build each size once into a reusable dir outside the repo. Do not commit it, and do not put home paths in docs
- Do not mount anything. The bench needs no FUSE/FSKit, only pack files

</code_context>

<specifics>
## Specific Ideas

How the in-RAM index is built today (crates/chunk-store/src/lib.rs): Inner.index is a HashMap<ObjectId, Loc> (line 401). ObjectId is [u8;33] (ID_LEN=33, line 42). Loc is {pack u64, offset u64, stored u32, raw u32, flags u8} (line 169), 25 B padded to 32. open_with (456-532) lists the packs and sorts them. For each pack it does io.read(num, Kind::Idx), which reads the whole file into a Vec, then decode_index (358) into a temporary Vec<(ObjectId,Loc)>, then Inner::add inserts each entry (977). The map is never pre-sized. If the idx is missing or bad, the pack is read in full: the last pack (if not punched) becomes the active pack via scan_pack (247) and is cut and synced; a sealed pack goes through resync_scan (304), which runs CRC + decompress + BLAKE3 verify on every record and moves on byte by byte after damage, and then gets a new idx via write_durable (520). Read-only mode skips the idx write (493).

Expected baseline, so the measurements can be checked: a hashbrown bucket is about 72 B (K,V) + 1 ctrl byte, and the bucket count is next_pow2(n*8/7). 1M -> 2^21 buckets ≈ 153 MB (≈153 B/obj). 10M -> 2^24 ≈ 1.22 GB (≈122 B/obj). 100M -> 2^27 ≈ 9.8 GB (≈98 B/obj steady state). Peak RSS during the last rehash is old+new tables, ≈14.7 GB at 100M (≈147 B/obj), plus about 150 MB of transient per-pack idx buffer and decoded Vec (≈62 MB idx bytes + ≈89 MB Vec for a full 64 MiB pack). This agrees with the 85-170 B/obj estimate in #85. The design doc's '57 bytes per object' (docs/design/chunk-store.md about line 160) is wrong, and the phase should correct it.

Disk: about 104 B/object (54 B record + 50 B idx entry), so 100M ≈ 10.4 GB in about 81 packs of 64 MiB. Recovery of one pack re-verifies about 1.24M records (64 MiB).

RSS: libc::getrusage(RUSAGE_SELF).ru_maxrss gives bytes on macOS and KiB on Linux. Measure in a fresh child process for each open, because maxrss only goes up and the creator process already holds the full map. The creator itself needs about 10-15 GB at 100M; the host has 192 GiB.

Do not keep 100M IDs in the bench process while measuring (3.3 GB). Make sampled hit IDs from the counter with DEFAULT_HASHER.id(&i.to_le_bytes()).

Project rules: no macOS mounts in automated tests; use targeted cargo commands in worktrees and one full test run per merge; no store backward compatibility; FSKit is the only macOS backend.

</specifics>

<deferred>
## Deferred Ideas

Any change to the index structure (sorted runs, filters, mmap, HashMap::reserve or compact Loc) is Phase 17 (#85). Write-throughput comparison against the new index is Phase 17. HDD measurements, the multi-thread lookup scaling study and CI integration of the benchmark (benchmarks are not run in CI per benchmarks/BENCHMARKS.md) are out of scope.

</deferred>
