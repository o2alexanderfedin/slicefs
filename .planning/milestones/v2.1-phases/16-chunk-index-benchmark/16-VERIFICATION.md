---
phase: 16-chunk-index-benchmark
verified: 2026-11-20T09:38:49Z
status: passed
score: 3/3 must-haves verified
covered_files: [".planning/phases/16-chunk-index-benchmark/16-01-PLAN.md", ".planning/phases/16-chunk-index-benchmark/16-01-SUMMARY.md", ".planning/phases/16-chunk-index-benchmark/16-02-PLAN.md", ".planning/phases/16-chunk-index-benchmark/16-02-SUMMARY.md", ".planning/phases/16-chunk-index-benchmark/16-03-PLAN.md", ".planning/phases/16-chunk-index-benchmark/16-03-SUMMARY.md", "benchmarks/index-bench/Cargo.toml", "benchmarks/index-bench/README.md", "benchmarks/index-bench/src/backend.rs", "benchmarks/index-bench/src/main.rs", "benchmarks/index-bench/tests/cli_tests.rs", "docs/design/chunk-store.md", "scripts/docker-linux-test/run.sh"]
covered_digest: "v4:sha256:0b9075a3fc3dc8bc9634d5599bc1767c9bdab2afb04b931eb0c84b332b29c877"
behavior_unverified: 0
overrides_applied: 0
re_verification:
  previous_status: passed
  previous_score: 3/3
  reason: "stale fingerprint: covered files changed after b4b5494 (index-bench, chunk-store.md, run.sh: #108, #111, #112, #113, cold-cache work)"
  gaps_closed: []
  gaps_remaining: []
  regressions: []
---

# Phase 16: Chunk index benchmark Verification Report

**Phase Goal:** A baseline of the current in-memory index at 1M, 10M and 100M objects exists for #85
**Verified:** 2026-11-20T09:38:49Z (on branch docs/v2.1-close-items = develop a7b29a2 plus planning-doc edits)
**Status:** passed
**Re-verification:** Yes. The earlier report (2026-11-19, at b4b5494) was passed with no gaps. Its fingerprint was stale because later work changed the covered files: `git diff --stat b4b5494..HEAD` gives +1200/-77 lines in index-bench (main.rs, backend.rs, cli_tests.rs, README), `docs/design/chunk-store.md` and `run.sh`. Every truth was checked again against the current tree, not only for regressions.

## Goal Achievement

### Observable Truths (ROADMAP success criteria)

| # | Truth | Status | Evidence |
|---|-------|--------|----------|
| 1 | The benchmark reports cold-open time and peak RSS at 1M, 10M and 100M objects | VERIFIED | `step_open` (main.rs:526) reads `max_rss_bytes()` before (545) and after (552) the timed `backend.open`. It prints `wall_s`, `cpu_s`, `peak_bytes_per_object` and `steady_bytes_per_object`, and checks that the object count equals the marker. `run` starts each step as a new process (`child`, `current_exe`, main.rs:1072-1073). `ChunkStoreBackend::open` calls `ChunkStore::open` (backend.rs:266-275), and that calls `Self::open_with` (crates/chunk-store/src/lib.rs:1135-1141). The doc "Open" table (chunk-store.md, in `### Index baseline (#66)` at line 330) has macOS rows at 1M/10M/100M (0.964 / 12.844 / 141.241 s; peak 390 / 256 / 154 B/object) and Linux rows at 1M/10M. "Cold open" = an open in a new process. The page cache is warm and labelled `warm` in each row. This is the decision in 16-CONTEXT.md, "Decided autonomously". The tool can now also do a real cold-cache open (`--drop-caches` on Linux, `--cache cold` after `sudo purge` on macOS, main.rs:528-541). |
| 2 | It reports hit and miss lookup throughput and the recovery time of a pack without an index file | VERIFIED | `step_lookup` (main.rs:637) makes the fixed-seed hit and miss IDs before any timing (662-664). It times `contains` on hits and misses and the dedup-hit `put`, and it fails if a hit is not found. `step_recover` (main.rs:926) calls `drop_index` (backend.rs:278; moves the `.idx` files to `bench-saved` with `rename`, and puts them back if a move fails) before the timed open (947-951). Then it calls `restore_index` (backend.rs:321) on the success path and on the error path (959). After #112, `restore_index` also checks that a rebuilt `.idx` has the same entries as the saved copy, then puts the exact saved copy back. The doc has "Lookups" and "Recovery" tables: macOS 1M/10M/100M and Linux 1M/10M; scope one/all; mode rw/ro. |
| 3 | The results and the RAM per object are documented | VERIFIED | `docs/design/chunk-store.md:330` `### Index baseline (#66)`: method, machines, commit `321a443`, load, and the Store, Open, Lookups and Recovery tables. It gives peak and steady RSS B/object (100M macOS: 154 / 106) and the scan MB/s from the tool and from CPU time. `### Index on disk (#85)` (line 233) says "about 100 to 150 bytes of RAM" and points to "Index baseline (#66)". `### Index results (#85)` (line 484) compares with the baseline. GitHub: #66 is CLOSED, stateReason COMPLETED (2026-11-18T20:44:19Z). #85 has a comment that names #66, at 2026-11-18T20:44:21Z. |

**Score:** 3/3 roadmap truths verified (0 present, behavior-unverified). Plan truths: 16/16 verified.

### Plan must-haves

`query verify.artifacts`: 16-01 4/4 (exit 0), 16-02 3/3 (exit 0), 16-03 exit 66 (no artifacts declared). `query verify.key-links`: 16-01 4/4 (exit 0), 16-02 2/2 (exit 0), 16-03 exit 66 (no key links declared).

| Plan | Truth (short) | Status | Evidence (current tree) |
|------|---------------|--------|----------|
| 16-01 | `create` makes N unique 8-byte objects through the public API, closes the store, writes the marker | VERIFIED | `object(i)` = 8 bytes of `i` (main.rs:344); `step_create` 462; tests `create_refuses_a_non_empty_dir`, `create_counts_the_whole_store` |
| 16-01 | `open` in its own process reports wall/CPU time, peak RSS (ru_maxrss after - before), current RSS, per object, objects=N | VERIFIED | main.rs:526-597; test `open_reports_rss_per_object` |
| 16-01 | `lookup` reports contains hit/miss and put dedup-hit throughput; the store is unchanged | VERIFIED | main.rs:637-; test `lookup_finds_hits_not_misses_and_does_not_change_the_store` |
| 16-01 | `recover --packs 1\|all [--read-only]` removes .idx, times the scan open, names sealed/active, restores every .idx | VERIFIED | main.rs:926-1000; tests `recover_on_a_multi_pack_store_restores_every_idx` (checks `assert_unchanged` after each of the four modes) and `recover_on_a_one_pack_store_takes_the_active_pack_path`; green in CI on a7b29a2 |
| 16-01 | `run` starts itself once per step and prints markdown tables with an OS column | VERIFIED | `child` / `current_exe` (1072-1073); `table()` (1123); "OS" column (1240, 1263, 1297, 1361); `os_label()` (1106); test `run_creates_and_opens_a_small_store` |
| 16-01 | Every store call goes through the Backend and Store traits | VERIFIED | `trait Backend` (backend.rs:106), `impl Backend for ChunkStoreBackend` (247), `backend_by_name` (385, used at main.rs:150) |
| 16-01 | Every subcommand except create refuses a dir without the marker; create refuses a non-empty dir | VERIFIED | tests `open_refuses_a_dir_without_the_marker`, `lookup_refuses_...`, `recover_refuses_a_dir_without_the_marker`, `create_refuses_a_non_empty_dir` |
| 16-02 | README covers build, 1M/10M/100M stores outside the repo, each step, each key, cold-cache numbers | VERIFIED | README "## Run" (line 9; line 15 "D is a directory OUTSIDE the repo"; line 18 the 100M run), "## Steps", "## Output", "## Cache state" (line 128: Linux `--drop-caches`, macOS `sudo purge` + `--cache cold`) |
| 16-02 | `run.sh index-bench SIZES...` builds and runs on a container volume, with no FUSE device and no mount | VERIFIED | run.sh:28-40: a plain `docker run` with no `--device /dev/fuse`, no SYS_ADMIN, no `RUN`; volume `slicefs-index-bench`. A separate `index-bench-cold` mode (41-69) was added later for the privileged cold run. |
| 16-02 | `### Index baseline (#66)` with open, RSS, lookup, recovery and scan numbers at 1M/10M/100M (macOS) and 1M/10M (Linux) | VERIFIED | chunk-store.md:330-483 |
| 16-02 | The "Index in memory" text gives the measured RAM per object, not the old estimate | VERIFIED (section renamed later) | "57 bytes": 0 hits. Phase 17 renamed the section to `### Index on disk (#85)` (line 233). It says "about 100 to 150 bytes of RAM" and points to "Index baseline (#66)" |
| 16-02 | Each table names machine, OS, commit, file system and cache state; no home path | VERIFIED | Method and machine lines (340-347); Cache column; grep for `/Users/` and `~/` in chunk-store.md: 0 hits |
| 16-03 | Nothing posted before the phase PR is merged and the CI of the merge commit is green | VERIFIED | Unchanged since the earlier report: PR #99 merged into develop at 2026-11-18T20:36:17Z, CI of 6296eb6 green at 20:43:47Z, posts at 20:44:17Z / 20:44:21Z. (The plan says "main"; the project merges phase PRs into develop.) |
| 16-03 | #66 has a results comment with a pointer to the doc, and is closed as completed | VERIFIED | `gh issue view 66`: CLOSED, COMPLETED, 2026-11-18T20:44:19Z |
| 16-03 | #85 has a comment that links the #66 results as its baseline | VERIFIED | `gh issue view 85`: comments that name #66 at 2026-11-18T20:44:21Z and 2026-11-19T07:06:51Z |
| 16-03 | No home path and no words about licenses in the public text | VERIFIED | Unchanged since the earlier report (0 hits in both comments) |

### Required Artifacts

| Artifact | Expected | Status | Details |
|----------|----------|--------|---------|
| `benchmarks/index-bench/src/main.rs` | create/open/lookup/recover/run steps | VERIFIED | 1413 lines; `step_create` 462, `step_open` 526, `step_lookup` 637, `step_recover` 926, `step_run` 1132; later steps `settle`, `rewrite`, `gc` were added by later phases |
| `benchmarks/index-bench/src/backend.rs` | Backend trait (seam for Phase 17) | VERIFIED | 390 lines; `trait Backend` 106, `impl Backend for ChunkStoreBackend` 247 |
| `benchmarks/index-bench/tests/cli_tests.rs` | CI-size tests | VERIFIED | 442 lines, 18 tests |
| `benchmarks/index-bench/Cargo.toml` | `publish = false` bin crate | VERIFIED | gsd verify.artifacts; workspace member (Cargo.toml:9) |
| `benchmarks/index-bench/README.md` | Run, output keys, cache state | VERIFIED | Sections Run, Steps, Output, Cache state, Disk and memory, Recovery paths, Linux, Other backends |
| `scripts/docker-linux-test/run.sh` `index-bench)` | Linux run mode, no mount | VERIFIED | Lines 28-40 |
| `docs/design/chunk-store.md` `### Index baseline (#66)` | Baseline tables | VERIFIED | Lines 330-483 |

### Key Link Verification

| From | To | Via | Status | Details |
|------|----|----|--------|---------|
| workspace `Cargo.toml` | index-bench crate | members entry | WIRED | gsd verify.key-links; Cargo.toml:9 |
| main.rs | backend.rs | `backend_by_name` | WIRED | gsd verify.key-links; main.rs:53, 150 |
| backend.rs | `ChunkStore::open_with` | `ChunkStore::open` -> `open_with` | WIRED | gsd verify.key-links; lib.rs:1135-1141 |
| main.rs | chunk-store hash | `DEFAULT_HASHER.id` for the hit and miss IDs | WIRED | gsd verify.key-links; main.rs:350-351 |
| design note | index-bench README | names the tool | WIRED | gsd verify.key-links; chunk-store.md:332-333 |
| run.sh | index-bench `run` | `index-bench run --dir` | WIRED | gsd verify.key-links; run.sh:38 |
| index-bench tests | CI | `cargo test --workspace` (Linux Docker `run.sh all`, run.sh:24; macOS job, ci.yml:44-45) | WIRED | CI run 44704112946 on a7b29a2 (push to develop): Linux job success, macOS job success |
| #85 | #66 baseline | issue comment | WIRED | Comment at 2026-11-18T20:44:21Z |

### Data-Flow Trace (Level 4)

| Artifact | Data Variable | Source | Produces Real Data | Status |
|----------|---------------|--------|--------------------|--------|
| `step_open` | `peak_bytes_per_object`, `wall_s` | `getrusage` ru_maxrss and `Instant` around a real `ChunkStore::open` | Yes (object count checked against the marker) | FLOWING |
| `step_recover` | `wall_s`, `scanned_bytes` | timed `backend.open` after real `.idx` renames | Yes | FLOWING |
| chunk-store.md baseline tables | numbers | tool output at commit 321a443 (per the doc's method lines) | Yes (static doc of a past run) | FLOWING |

### Behavioral Spot-Checks

| Behavior | Command | Result | Status |
|----------|---------|--------|--------|
| The bench steps run and leave the store whole (incl. the recover restore invariant) | `cargo test --workspace` in CI (includes `-p index-bench`) | CI run 44704112946 on a7b29a2: Linux and macOS jobs success | PASS (CI) |
| 1M/10M/100M runs | not run again (long; needs no mount but much disk and RAM; static doc evidence is enough) | - | SKIP |
| Phase commits are merged | `git merge-base --is-ancestor` for 51ab6e3 4715e94 554e5a4 988f766 a184284 2f6710a 00931b6 | all are ancestors of HEAD | PASS |

No test was run locally (other verifiers share the tree; CI on the same commit is green).

### Probe Execution

Not applicable: the phase declares no `probe-*.sh`.

### Requirements Coverage

| Requirement | Source Plan | Description | Status | Evidence |
|-------------|-------------|-------------|--------|----------|
| BNCH-01 | 16-01, 16-02 | 1M, 10M, 100M small synthetic objects; cold-open time and peak RSS of `ChunkStore::open_with` | SATISFIED | 8-byte objects (104 B/object on disk; 1, 9 and 81 packs). Open time and peak RSS at all three sizes on macOS, 1M and 10M on Linux. "Cold open" = new process, warm labelled cache (CONTEXT decision) |
| BNCH-02 | 16-01, 16-02 | Lookup throughput for dedup hit and miss; recovery time of a pack without a valid index file | SATISFIED | Lookups and Recovery tables |
| BNCH-03 | 16-02, 16-03 | Results (RAM per object, scan throughput) documented as the baseline for #85 | SATISFIED | `### Index baseline (#66)`; #66 closed as completed; #85 links to it; `### Index results (#85)` uses it |

No orphaned requirements: REQUIREMENTS.md maps only BNCH-01..03 to Phase 16, and the plans claim all three. The milestone audit (`.planning/v2.1-MILESTONE-AUDIT.md`) also lists BNCH-01..03 as satisfied.

### Anti-Patterns Found

None. No TBD, FIXME, XXX, TODO, HACK, `todo!` or `unimplemented!` in `benchmarks/index-bench/**` or `scripts/docker-linux-test/run.sh`.

### Human Verification Required

None. The plans have no `<human-check>` block. The 16-03 checkpoints (post on #66, close it) are done.

### Non-critical notes / tech debt

1. **Warm-cache baseline only.** All baseline rows are warm cache. The tool now supports a real cold run, and the doc has a separate `### Index with a cold page cache` section (line 901), but it measures the on-disk index (#85), not the in-memory baseline. The doc says this (last note of the baseline section).
2. **No Linux 100M.** The VM had 12.6 GB available and the run needs about 25 GB. Stated in the doc.
3. **Noisy timings.** The load average was 80-140 on macOS. "Extra s" is negative in several recovery rows, so the tool prints `n/a`; the CPU-based columns were added by hand.
4. **The baseline is tied to commit 321a443.** The index-bench at HEAD drives the on-disk index (Phase 17, #111, #112): a run at HEAD measures the new index, and `recover` now scans only packs at or above the index high-water mark. The baseline numbers cannot be reproduced at HEAD without checking out 321a443.
5. **Raw output is outside the repo** (scratch dir and the Docker volume); only the doc tables are versioned.
6. **SC1 rests on an autonomous CONTEXT decision** that defines "cold open" as a new process with a warm, labelled cache. The user can reverse it.

### Gaps Summary

No gaps. Since the last report, later phases (#108, #111, #112, #113 and the cold-cache work) changed the tool and the doc a lot. The Phase 16 deliverables are all still there and wired: the create/open/lookup/recover/run steps, the Backend seam to `ChunkStore::open` -> `open_with`, the refusal and restore tests, the Linux run mode with no mount, and the `### Index baseline (#66)` section with the 1M/10M/100M numbers. CI on a7b29a2 is green on Linux and macOS. #66 is closed as completed and #85 links to it.

---

_Verified: 2026-11-20T09:38:49Z_
_Verifier: Claude (gsd-verifier)_
