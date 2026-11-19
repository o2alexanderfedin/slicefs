---
phase: 16-chunk-index-benchmark
verified: 2026-11-19T07:22:22Z
status: passed
score: 3/3 must-haves verified
covered_files: [".planning/phases/16-chunk-index-benchmark/16-01-PLAN.md", ".planning/phases/16-chunk-index-benchmark/16-01-SUMMARY.md", ".planning/phases/16-chunk-index-benchmark/16-02-PLAN.md", ".planning/phases/16-chunk-index-benchmark/16-02-SUMMARY.md", ".planning/phases/16-chunk-index-benchmark/16-03-PLAN.md", ".planning/phases/16-chunk-index-benchmark/16-03-SUMMARY.md", "benchmarks/index-bench/Cargo.toml", "benchmarks/index-bench/README.md", "benchmarks/index-bench/src/backend.rs", "benchmarks/index-bench/src/main.rs", "benchmarks/index-bench/tests/cli_tests.rs", "docs/design/chunk-store.md", "scripts/docker-linux-test/run.sh"]
covered_digest: "v4:sha256:6124587e53c532fcce56b5c7f22fd07c1064e9523b0aad598c0796ac36102e4e"
behavior_unverified: 0
overrides_applied: 0
---

# Phase 16: Chunk index benchmark Verification Report

**Phase Goal:** A baseline of the current in-memory index at 1M, 10M and 100M objects exists for #85
**Verified:** 2026-11-19T07:22:22Z (retroactive, at develop b4b5494)
**Status:** passed
**Re-verification:** No (initial verification)

## Goal Achievement

### Observable Truths (ROADMAP success criteria)

| # | Truth | Status | Evidence |
|---|-------|--------|----------|
| 1 | The benchmark reports cold-open time and peak RSS at 1M, 10M and 100M objects | VERIFIED (by an autonomous decision) | `step_open` (main.rs:456) reads `max_rss_bytes()` (ru_maxrss) and the current RSS before and after the timed `backend.open`. It prints wall and CPU time, and peak and steady B/object. `run` starts each step as a new process (`current_exe`, main.rs:758). `ChunkStoreBackend::open` calls `ChunkStore::open`, and that calls `open_with` (lib.rs:845-847). The doc "Open" table has macOS rows at 1M, 10M and 100M (0.964 / 12.844 / 141.241 s; peak 390 / 256 / 154 B/object). Here "cold open" means an open in a new process, with a warm page cache that is labelled. This is the decision [cold vs warm cache] in 16-CONTEXT.md line 23, under "Decided autonomously", and 16-02-PLAN line 216 repeats it. Every row is labelled `warm`. |
| 2 | It reports hit and miss lookup throughput and the recovery time of a pack without an index file | VERIFIED | `step_lookup` (main.rs:557) makes a fixed-seed sample before any timing. It times `contains` on hits and on misses, and `put` on dedup hits. It fails if a hit is missing, if a miss is found, or if the count changes. `step_recover` (main.rs:625) calls `drop_index` (which renames the `.idx` files into `bench-saved`, backend.rs:229, rename at line 258) before the timed open. Then it calls `restore_index` on both the success and the error path. The doc "Lookups" and "Recovery" tables have rows for macOS 1M/10M/100M and Linux 1M/10M, with the scopes one and all and the modes rw and ro. |
| 3 | The results and the RAM per object are documented | VERIFIED | `docs/design/chunk-store.md` line 307 has `### Index baseline (#66)`: the method, the machine, commit 321a443, the load, and the Store, Open, Lookups and Recovery tables. Peak and steady RSS B/object are given (at 100M on macOS: 154 and 106). The scan MB/s comes from the tool and from CPU time, with notes. `### Index results (#85)` (line 461) compares Phase 17 with this baseline. #66 is CLOSED as COMPLETED (2026-11-18T20:44:19Z) with a results comment. #85 has a comment that links to it (2026-11-18T20:44:21Z). |

**Score:** 3/3 roadmap truths verified (0 present, behavior-unverified). Plan truths: 16/16 verified (one is now in reworded text, see below).

### Plan must-haves

`query verify.artifacts`: 16-01 4/4, 16-02 3/3. `query verify.key-links`: 16-01 4/4, 16-02 2/2. 16-03 has no artifacts and no key links.

| Plan | Truth (short) | Status | Evidence |
|------|---------------|--------|----------|
| 16-01 | `create` makes N unique 8-byte objects through the public API, closes the store, writes the marker | VERIFIED | `object(i) = i.to_le_bytes()` (main.rs:285); `step_create` at 403; test `create_refuses_a_non_empty_dir` |
| 16-01 | `open` in its own process reports wall/CPU time, peak RSS (ru_maxrss after - before), current RSS, per object, objects=N | VERIFIED | main.rs:456-500 |
| 16-01 | `lookup` reports contains hit/miss and put dedup-hit throughput; the store is unchanged | VERIFIED | main.rs:557-623; test `lookup_finds_hits_not_misses_and_does_not_change_the_store` |
| 16-01 | `recover --packs 1\|all [--read-only]` removes .idx, times the scan open, names sealed/active, restores every .idx | VERIFIED | main.rs:625-700; tests `recover_on_a_multi_pack_store_restores_every_idx` and `recover_on_a_one_pack_store_takes_the_active_pack_path` |
| 16-01 | `run` starts itself once per step and prints markdown tables with an OS column | VERIFIED | `current_exe` (758); `table()` (808) with an "OS" column (923, 946, 980, 1036); `os_label()` |
| 16-01 | Every store call goes through the Backend and Store traits | VERIFIED | key link `backend_by_name` found; `trait Backend` (backend.rs:91); Phase 17 added a backend option without changing the step structure |
| 16-01 | Every subcommand except create refuses a dir without the marker; create refuses a non-empty dir | VERIFIED | tests `open_refuses_...`, `lookup_refuses_...`, `recover_refuses_a_dir_without_the_marker`, `create_refuses_a_non_empty_dir` |
| 16-02 | README covers build, 1M/10M/100M stores outside the repo, each step, each key, cold-cache numbers | VERIFIED | README line 15 ("D is a directory OUTSIDE the repo"), line 18 (100M run), "## Cache state" (lines 112-119) |
| 16-02 | `run.sh index-bench SIZES...` builds and runs on a container volume, with no FUSE device and no mount | VERIFIED | run.sh:27-39: a plain `docker run` without `--device /dev/fuse`, SYS_ADMIN or `RUN`; volume `slicefs-index-bench` |
| 16-02 | `### Index baseline (#66)` with the open, RSS, lookup, recovery and scan numbers at 1M/10M/100M (macOS) and 1M/10M (Linux) | VERIFIED | Lines 307-460 |
| 16-02 | The "Index in memory" text gives the measured RAM per object, not the old estimate | VERIFIED (text reworded later) | The "57 bytes" estimate is gone (0 hits). Phase 17 renamed the section to `### Index on disk (#85)` (line 229). It says "about 100 to 150 bytes of RAM for each object" and points to "Index baseline (#66)" |
| 16-02 | Each table names the machine, OS, commit, file system and cache state; no home path | VERIFIED | Method and machine lines; Cache column; grep for `/Users/` and `~/`: 0 hits |
| 16-03 | Nothing posted before the phase PR is merged and the CI of the merge commit is green | VERIFIED | PR #99 was merged into develop at 2026-11-18T20:36:17Z, and CI on 6296eb6 succeeded at 20:43:47Z. The posts are at 20:44:17Z (#66) and 20:44:21Z (#85). The plan text says "merged into main", but the project merges phase PRs into develop |
| 16-03 | #66 has a results comment with a pointer to the doc, and is closed as completed | VERIFIED | stateReason COMPLETED; the comment names "Index baseline" |
| 16-03 | #85 has a comment that links the #66 results as its baseline | VERIFIED | Comment at 2026-11-18T20:44:21Z |
| 16-03 | No home path and no words about licenses in the public text | VERIFIED | grep of both comments: 0 hits |

### Required Artifacts

| Artifact | Expected | Status | Details |
|----------|----------|--------|---------|
| `benchmarks/index-bench/src/main.rs` | create/open/lookup/recover/run steps | VERIFIED | 1086 lines; subcommand table at line 77; `step_create` 403, `step_open` 456, `step_lookup` 557, `step_recover` 625, `step_run` 817 |
| `benchmarks/index-bench/src/backend.rs` | Backend trait (seam for Phase 17) | VERIFIED | `trait Backend` line 91, `impl Backend for ChunkStoreBackend` line 208 |
| `benchmarks/index-bench/tests/cli_tests.rs` | CI-size tests | VERIFIED | 365 lines; tests for run, the open RSS keys, lookup hit/miss, recover (the idx files come back), and the refusals |
| `benchmarks/index-bench/Cargo.toml` | `publish = false` bin crate | VERIFIED | gsd verify.artifacts |
| `benchmarks/index-bench/README.md` | Run, output keys, cache state | VERIFIED | Sections Run, Steps, Output, Cache state, Disk and memory, Recovery paths, Linux, Other backends |
| `scripts/docker-linux-test/run.sh` `index-bench)` | Linux run mode, no mount | VERIFIED | Lines 27-39 |
| `docs/design/chunk-store.md` `### Index baseline (#66)` | Baseline tables | VERIFIED | Lines 307-460 |

### Key Link Verification

| From | To | Via | Status | Details |
|------|----|----|--------|---------|
| workspace `Cargo.toml` | index-bench crate | members entry | WIRED | gsd verify.key-links |
| main.rs | backend.rs | `backend_by_name` | WIRED | gsd verify.key-links |
| backend.rs | `ChunkStore::open_with` | `ChunkStore::open` -> `open_with` | WIRED | gsd verify.key-links; lib.rs:845-847 |
| main.rs | chunk-store hash | `DEFAULT_HASHER` for the hit and miss IDs | WIRED | gsd verify.key-links; main.rs:291-292 |
| design note | index-bench README | names the tool | WIRED | gsd verify.key-links |
| run.sh | index-bench `run` | `index-bench run --dir` | WIRED | gsd verify.key-links |
| index-bench tests | CI | `cargo test --workspace` (Linux Docker `run.sh all`; macOS job) | WIRED | PR #114: Linux pass (8m11s), macOS pass (2m9s). Develop CI for b4b5494 (run 44478418274): success |
| #85 | #66 baseline | issue comment | WIRED | Comment at 2026-11-18T20:44:21Z |

### Behavioral Spot-Checks

| Behavior | Command | Result | Status |
|----------|---------|--------|--------|
| The bench steps run and leave the store intact | `cargo test --workspace` in CI (includes `-p index-bench`) | PR #114 and develop b4b5494: green on Linux and macOS | PASS (CI) |
| 1M/10M/100M runs | not run again (long; static evidence is enough for this audit) | - | SKIP |
| Phase commits are merged | `git merge-base --is-ancestor` for 51ab6e3 4715e94 554e5a4 988f766 a184284 2f6710a 00931b6 | all are ancestors of HEAD | PASS |

### Requirements Coverage

| Requirement | Source Plan | Description | Status | Evidence |
|-------------|-------------|-------------|--------|----------|
| BNCH-01 | 16-01, 16-02 | Stores with 1M, 10M, 100M small synthetic objects; cold-open time and peak RSS of `ChunkStore::open_with` | SATISFIED | 8-byte objects (104 B/object on disk; 1, 9 and 81 packs). Open time and peak RSS at all three sizes on macOS; on Linux only 1M and 10M (VM memory). "Cold open" = a new process with a warm, labelled cache (an autonomous CONTEXT decision) |
| BNCH-02 | 16-01, 16-02 | Lookup throughput for a dedup hit and miss; recovery time of a pack without a valid index file | SATISFIED | Lookups table (contains hit/miss ns and per s; put dedup-hit ns). Recovery table (one/all x rw/ro; recovery open s; scan MB/s from the tool and from CPU time) |
| BNCH-03 | 16-02, 16-03 | Results (RAM per object, scan throughput) documented as the baseline for #85 | SATISFIED | `### Index baseline (#66)`; #66 closed as completed with a results comment; #85 links to it; Phase 17 `### Index results (#85)` uses it |

No orphaned requirements. REQUIREMENTS.md maps only BNCH-01..03 to Phase 16, and plans claim all three.

### Anti-Patterns Found

None. There is no TBD, FIXME, XXX, TODO, HACK, `todo!` or `unimplemented!` in `benchmarks/index-bench/**` or `scripts/docker-linux-test/run.sh`.

### Human Verification Required

None. The plans have no `<human-check>` block. The 16-03 checkpoints (post on #66 and close it) are done, and #66 is closed.

### Non-critical gaps / tech debt

1. **No cold-page-cache numbers.** All rows are warm cache. The tool can do a cold run (`run --drop-caches` on Linux as root in a privileged container; on macOS, `sudo purge` and then `open --cache cold` by hand), but nobody ran it. The 16-CONTEXT decision accepts this, and the cache state is labelled.
2. **Linux 100M was not run.** The OrbStack VM had 12.6 GB available, and the run needs about 25 GB. 100M exists only for macOS.
3. **The timings were taken under heavy host load.** The load average was 80-140 on macOS, and the host swap grew during the 100M run. The wall-time "Extra s" is negative in 5 of 12 macOS and 3 of 8 Linux recovery rows, so the tool prints `n/a` there. Several "one" rows are within the noise.
4. **The CPU-based scan columns were added to the doc by hand** ("Extra CPU s", "Scan MB/s (CPU)"). The tool does not print them, so a comparison must compute them the same way by hand.
5. **Create and lookup ran one time only.** Open and recovery are the median of 3 runs. The macOS 1M lookup row is slow because of the load (a later run at lower load gave 430-756 ns per hit).
6. **The baseline is tied to commit 321a443.** The `index-bench` at HEAD includes Phase 17 changes (on-disk index, `--memtable-limit`, index keys, a GC step). A run at HEAD measures the new index, not the in-memory baseline.
7. **The raw output is outside the repo** (per SUMMARY; not checked): a scratch directory and the Docker volume `slicefs-index-bench`. Only the doc tables are versioned.
8. **At 321a443, `disk_bytes` counted only `D/packs`** (a known limit in 16-01). Phase 17 fixed it (commit 0eb63bb, test `create_counts_the_whole_store`). This does not change the in-memory baseline, which had no index outside `packs/`.
9. **Judgement call to note:** the verdict on SC1 and BNCH-01 rests on a CONTEXT decision marked "Decided autonomously". That decision defines "cold open" as a new process with a warm, labelled cache. The user can reverse it if a cold page cache was meant.

### Gaps Summary

There are no blocking gaps. The tool exists, it is wired to `ChunkStore::open_with`, and CI tests it. The baseline is in the design note: 1M/10M/100M on macOS, 1M/10M on Linux. #66 is closed after the merge and green CI, #85 links to it, and Phase 17 has already used it. The open items are about the quality of the measurement (load, warm cache only, no Linux 100M), not about a missing deliverable.

---

_Verified: 2026-11-19T07:22:22Z_
_Verifier: Claude (gsd-verifier)_
