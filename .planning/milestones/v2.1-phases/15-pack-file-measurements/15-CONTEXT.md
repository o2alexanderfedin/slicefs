# Phase 15: Pack-file measurements - Context

**Gathered:** 2026-11-18
**Status:** Ready for planning

<domain>
## Phase Boundary

This phase adds one bash script. It measures `slicefs seed` wall time and throughput, the store's apparent bytes, allocated bytes and file count, for three inputs: the Boost 1_85_0 tree, the Boost tar, and a fixed random file. The script runs the same way on any build. It runs once on a pre-pack build and once on the current build. The numbers go into docs/design/chunk-store.md 'Measurements to take' (line 313), with the machine and the commit hashes. Then #8 is closed with a comment that points to them. Nothing is mounted. The FSKit Boost round trip stays a manual step for the user (scripts/macos-mount-test/boost-test.sh).

</domain>

<decisions>
## Implementation Decisions


### Decided autonomously
- [Baseline commit] Q: "Which commit is the 'before packs' build?" → Selected: "Primary baseline: c2f82fa, the merge of #44 and the last first-parent commit before the chunk-store crate (#45 adf15f4) and file content in packs (#46 616eef2). At c2f82fa every object is a vt0/ file written by blockset, which is what #8 calls 'before'. Second baseline: 7016bb6, the merge of #55 and the first parent of e038e9f (merge of #56). There, file content is in packs but metadata nodes are still in vt0/. It shows the #56 step literally. Current build: develop HEAD (c447551 at scout time). The doc records all three hashes." — The script takes a binary path, so measuring one more commit only costs a build. It changes no persisted format.
- [Same CLI on all builds] Q: "Does `slicefs seed` have the same CLI on all builds?" → Selected: "Yes, as `slicefs seed <store> <source_dir>` (positional). c2f82fa has cli.rs:90 `Seed { store, source_dir }` and 7016bb6 has cli.rs:95. The current build has crates/slicefs-cli/src/cli.rs:100 and adds an optional `--hash`, which the script must NOT pass. `seed` accepts only a directory (seed.rs run_seed walks source_dir), so the tar and random cases each seed a directory with one file in it." — This was read from the code. Nothing needs a decision.
- [Script language/location] Q: "What language is the script in, and where does it go?" → Selected: "Bash, like all existing scripts (scripts/docker-linux-test/*.sh, scripts/macos-mount-test/*.sh), with inline python3 for arithmetic as boost-test.sh does in rate(). New file: scripts/pack-measure/seed-measure.sh. Usage: `seed-measure.sh <slicefs-binary> <data-dir> [runs]`. Output: CSV on stdout plus a results file in the work dir. Env: WORK, RUNS (default 3)." — A new script file can be moved or renamed freely.
- [Boost input] Q: "Where does the Boost tree come from?" → Selected: "Reuse scripts/macos-mount-test/fetch-boost.sh <dir> 1_85_0, which downloads boost_1_85_0.tar (uncompressed) and expands x/boost_1_85_0/ (about 77,655 files). This matches the CI default in .github/workflows/macos-mounts.yml (versions default 1_85_0). The script takes BOOST_DIR the same way boost-test.sh does. No Boost data exists locally yet." — It reuses an existing script, and the version is a parameter.
- [Random input] Q: "How is the random data made so that runs can be repeated?" → Selected: "Make one 1 GiB file once in the data dir (`head -c 1073741824 /dev/urandom`) if it does not exist yet. All builds and runs then use that same file, so the input is the same everywhere and needs no seeded PRNG." — Size and generation are script parameters.
- [Metrics] Q: "What does the script measure, and how?" → Selected: "For each input, build and run: (1) seed wall time, from `/usr/bin/time -l` on macOS, which also gives peak RSS. (2) Throughput = input apparent bytes / seconds. (3) Store apparent bytes = sum of `stat -f %z` (the fbytes() pattern from boost-test.sh). Allocated bytes = `du -sk` × 1024. (4) Store file count = `find <store> -type f | wc -l`, plus a per-top-dir split (vt0/, packs/, segments/). Do NOT parse `slicefs stats` text, because its fields differ between builds (the c2f82fa stats.rs reports vt0/ 'physical bytes'). Each run seeds a fresh store dir. The script reads the source once to warm the page cache, then reports the median of RUNS=3." — This is measurement code only and can be changed at any time.
- [Where to run] Q: "Run on the macOS host or in the Linux Docker container?" → Selected: "Run on the macOS host (APFS), because #8 is about APFS 4 KiB allocation waste and the syscall count per object. `seed` does not mount, so the no-mounts rule does not apply. Record the machine as CPU model, RAM, macOS version and filesystem (APFS). The scouted host is an Intel Xeon W-3245 with 192 GiB RAM. A Linux run in the Docker container is optional and out of scope." — A host can be added later with the same script.
- [Building the old builds] Q: "How are the pre-pack binaries built without touching the main tree?" → Selected: "Use `git worktree add <scratch>/wt-c2f82fa c2f82fa` and `git worktree add ... 7016bb6`, then `git submodule update --init crates/data-id` in each worktree. Both commits need blockset from the crates/data-id submodule (pinned at ec08e9b; the repo is private and reachable with the user's gh/git auth). Build with `cargo build --release -p slicefs-cli` and a separate CARGO_TARGET_DIR for each worktree, so the current target/ is not rebuilt. Both builds use vendor/fuser from the repo. The three builds can run in parallel (map). Measurements must run one after the other (reduce), so the timings do not interfere." — The worktrees and target dirs are temporary.
- [Store file count after Boost copy (PACK-03)] Q: "Is 'after a Boost copy' the seeded store, or a store after an FSKit copy?" → Selected: "Record the file count after seeding the Boost tree (and the tar) on each build. That is the automated proxy. Add a line to the doc that the FSKit round-trip count (the user's manual run of boost-test.sh) can be added when the user runs it. #8 closes on the seed-based numbers, as PACK-03 allows." — This is only doc wording.
- [Doc format] Q: "How are the numbers written in chunk-store.md?" → Selected: "Under '## Measurements to take' (docs/design/chunk-store.md:313), keep the existing list and add a '### Results (date)' subsection. It has a machine line, a commits line (c2f82fa, 7016bb6, HEAD) and one markdown table per input with these columns: build, seconds, MB/s, apparent MB, allocated MB, files, peak RSS. Then one line that names the script command. No home paths: write BOOST_DIR=<dir>, not ~/boost." — Docs can be edited later.

### Claude's Discretion
- Test names, helper layout and plan split are at Claude's discretion.

</decisions>

<code_context>
## Existing Code Insights

### Reusable Assets
- scripts/macos-mount-test/fetch-boost.sh (downloads boost_<v>.tar + x/boost_<v>/)
- scripts/macos-mount-test/boost-test.sh (patterns: fbytes() apparent-size sum via stat -f %z, rate() python3 MB/s, step() timing, perf.csv format, SLICEFS/BOOST_DIR/WORK env convention)
- scripts/docker-linux-test/read-bench.sh and dedup-probe.sh (short bash measurement style, `$B seed $R/store $R/src`)
- benchmarks/chunk-bench (FastCDC chunking only, no disk writes; not a substitute for seed but its README shows the Boost tar/tree inputs)
- .github/workflows/macos-mounts.yml (Boost 1_85_0 default, cache key)

### Established Patterns
- Scripts are plain bash (`#!/bin/bash`), with short variable names (B, R, T), env overrides with defaults, and python3 one-liners for floating-point math
- Binary path is passed as $1 or SLICEFS env, defaulting to target/release/slicefs
- Store sizes as apparent bytes (stat -f %z on macOS) plus allocated bytes (du)
- CSV output (version,step,mb,files,secs) for results
- Plain-English, STE-style docs with no home paths and no license text

### Integration Points
- crates/slicefs-cli/src/seed.rs:36 run_seed(store, source_dir, hash): only directories are accepted; symlinks are skipped (walk_dir); it writes packs/ + segments/segment-000001.seg on the current build
- crates/slicefs-cli/src/cli.rs:100 Seed { store, source_dir, hash } (current) vs cli.rs:90 at c2f82fa and cli.rs:95 at 7016bb6 (no --hash)
- c2f82fa:crates/slicefs-cli/src/seed.rs uses blockset FileStorageAdd -> vt0/ batch files; needs crates/data-id submodule ec08e9b (private repo o2alexanderfedin/data-id)
- docs/design/chunk-store.md:313 '## Measurements to take' (results go here)
- GitHub issue #8: the closing comment cites the doc section and commits (the user or orchestrator does the closing; the scout must not write)

### Files likely modified
- scripts/pack-measure/seed-measure.sh (new)
- scripts/pack-measure/README.md (new, short usage; or a row added to an existing README)
- docs/design/chunk-store.md (Results subsection under 'Measurements to take', ~line 313)

### Risks
- Old builds need the private crates/data-id submodule (ec08e9b). The submodule update needs the user's git/gh credentials; if auth fails, the c2f82fa and 7016bb6 builds cannot be made.
- Old commits may not compile cleanly on the current Rust toolchain (there is no rust-toolchain file to pin it). The fallback is `cargo +<older>` or small local-only patches in the worktree, which must not be committed.
- The c2f82fa seed writes about one vt0/ file per ~4 KiB object (more than 100k files for Boost). This is slow and fills the scratch disk, but it is exactly the thing being measured. Run it on a disk with plenty of space.
- Boost download (archives.boost.io) needs network access; nothing is cached locally yet (~/boost is absent).
- macOS-only tools (stat -f %z, /usr/bin/time -l): the script should detect GNU stat/time if it ever runs on Linux.
- Timing noise from other processes on the host: use median of 3 runs and note warm page cache.

</code_context>

<specifics>
## Specific Ideas

The git history does not match how REQUIREMENTS words the baseline. The '#56' named in PACK-02 is e038e9f 'meta-nodes-in-packs'. File content had already moved into packs at #46 (616eef2; commit f2f9c15 'file content in the chunk store (packs), not in vt0/'). The true blockset or vt0-only baseline is therefore c2f82fa (the #44 merge). The literal 'before #56' commit is 7016bb6 (first parent of e038e9f). Blockset was removed in #57 (65db7c3). Measure all three; the cost is small.

Seed CLI: the positional `seed <store> <dir>` is the same on all three builds. Only the current build has `--hash`; do not pass it.

Running the tar and random cases: put the single file in its own directory, because seed only walks directories.

Measuring sizes: do not parse `slicefs stats`, because its output differs between builds. Use find, stat and du.

Build plan (map/reduce): three parallel worktree release builds (map). Then measurements run one after the other on the macOS host (reduce), because timings run in parallel would disturb each other. Then one doc edit and closing #8. Store dirs and data go in a scratch or work dir, never in the repo.

Machine: Intel Xeon W-3245 @ 3.20GHz, 192 GiB RAM, macOS (Darwin 25.2). Record the CPU, RAM and OS version, not the hostname or paths.

Project rules: no macOS mounts in automated tests; use targeted cargo commands in worktrees and one full test run per merge; no store backward compatibility; FSKit is the only macOS backend.

</specifics>

<deferred>
## Deferred Ideas

A Linux/OrbStack run of the same script. The FSKit Boost round-trip numbers (time per step, fsync and write time per call), which the user runs by hand with boost-test.sh and adds to the doc later. Measurements after each separate step from 2 to 4 (#46, #56, the GC PRs), beyond the three builds. Cold-cache timing (macOS `purge` needs sudo).

</deferred>
