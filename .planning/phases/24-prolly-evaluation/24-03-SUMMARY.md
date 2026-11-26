---
phase: 24-prolly-evaluation
plan: 03
subsystem: prolly benchmark
status: complete
tags: [prolly, benchmark, metadata, chunk-store]
requires:
  - "24-01: prolly Builder, iter, walk, boundary_hash"
  - "24-02: prolly::apply and ApplyStats"
provides:
  - "benchmarks/prolly-bench: gen, seed, whole, batches, flat, prolly and scale"
  - "the review entry and count line of the bench getrusage helper"
affects: [24-04, 24-05]
tech-stack:
  added: []
  patterns:
    - "one process per mode; flat and prolly replay batch files that come from the real inode_data of each commit"
    - "output lines `kind key=value ...`; exit 2 on a bad argument, exit 1 on another failure"
key-files:
  created:
    - benchmarks/prolly-bench/Cargo.toml
    - benchmarks/prolly-bench/build.rs
    - benchmarks/prolly-bench/src/main.rs
    - benchmarks/prolly-bench/src/whole.rs
    - benchmarks/prolly-bench/src/replay.rs
    - benchmarks/prolly-bench/tests/smoke.rs
    - .planning/phases/24-prolly-evaluation/runs/gate-24-03.log
  modified:
    - Cargo.toml
    - Cargo.lock
    - docs/design/unsafe-review.md
decisions:
  - "Module split main.rs, whole.rs, replay.rs; output lines `word key=value`; binary batch files"
  - "The version step compares content keys, not mtimes; CONTENT_BYTES = 4096; default seed 24"
  - "build.rs gives the bench binary the FUSE-T rpath on macOS, as crates/slicefs-cli/build.rs does for slicefs"
  - "The store listing has . and ..; the version sync and the remove skip them"
  - "The hash cost times only the hash loop over pieces of base.bin, not the file reads"
  - "batches, flat and prolly use one digest: BLAKE3 over the 41-byte records (ino LE, key) in ino order, without the count"
metrics:
  duration: "about 95 min"
  completed: 2026-11-26
requirements: [PROLLY-02]
actuals:
  tokens: 15100    # chars/4 over the bench files and the doc lines
  tasks: 3
  commits: 5       # git rev-list --count f12423c..420451a (3f2726c is the first SUMMARY commit)
plan_head_before: f12423c
plan_head_after: 420451a
---

# Phase 24 Plan 03: Prolly benchmark binary Summary

`benchmarks/prolly-bench` is a by-hand tool (release build) that measures the prolly prototype against the metadata commit path. It seeds a generated tree, runs a seeded edit script of small commits and one version commit on the real `DictMetadataStore`, extracts the real `inode_data` change of each commit, and replays the changes through a copy of the current map serializer (`flat`) and through the prototype (`prolly`). Each mode runs in its own process and store. Both replays end with a digest check against the real final map.

## Commits

| Task | Commit | Subject |
|------|--------|---------|
| 1 (tracer) | 2861417 | feat(24-03): prolly bench tracer: seed, whole commit, batches, prolly replay |
| 2 | 157c024 | feat(24-03): full edit script and version step of the whole mode |
| 3 | 32a6f77 | feat(24-03): flat baseline, scale rows, hash cost, tree shape; gate log |
| 3 (fix) | 420451a | fix(24-03): small hash buffer and streamed batch reads in the replays |

The SUMMARY has its own docs commits (3f2726c and the update after the fix).

## Subcommands

| Subcommand | What it does | Main output lines |
|------------|--------------|-------------------|
| `gen tree` | N files in 16 subdirectories (copy of `chunk_profile_bench` `gen tree`, `--max-bytes` 4096 by default) | `gen` |
| `seed` | `run_seed_with(store, tree, None, None)` | `seed` |
| `whole` | Reopens the store (as `snapshot.rs`), setup commit, the edit script, optional version step; `begin_commit` and `finish_commit` timed apart | `info`, `commit mode=whole`, `version`, `disk` |
| `batches` | Streams the `inode_data` nodes of consecutive roots (1 MiB pieces, merge diff) | `batch`, `final` |
| `flat` | In-RAM map of `inode_data` and a copy of `intern_u64_digest_map` through `store_io::put_value` per commit | `commit mode=flat`, `check`, `disk` |
| `prolly` | Bulk build from `base.bin`, then one `prolly::apply` and one `cs.sync()` per batch | `hash`, `commit mode=prolly`, `check`, `disk`, `tree` |
| `scale` | Flat or prolly on N synthetic entries: a build, then one changed middle value | `scale` (two lines) |

The edit script: edit i of commit seq has the kind (i + seq) mod 4 (metadata change, content change, create, delete). A delete takes the oldest created file. A delete with no created file is a create. `scatter` draws each pick. `cluster` draws one start per commit, then takes the next regular files in ino order. Every pick loop is bounded and exits 1 when it finds no file.

## Results

- `cargo test -p prolly-bench`: `smoke_end_to_end` and `bad_arguments_exit_2` pass. The smoke test time is about 1.7 s. It uses only `gen tree` data (200 and 220 files).
- `cargo test -p prolly`: 21 passed.
- `cargo +stable clippy --locked -p prolly-bench -p prolly --all-targets -- -D warnings -D clippy::allow_attributes_without_reason`: `clippy=0`. The allow-reason grep over `benchmarks/prolly-bench crates/prolly`: 0 lines.
- `scripts/safety/unsafe-count.sh`: `UNSAFE files=32 tokens=134 result=ok`. The bench has 2 tokens, all in the `rusage` helper of `main.rs`.
- `cargo tree --locked -i prolly -e normal`: `prolly prolly-bench`. The `prolly-bench` block of `Cargo.lock` has 0 `source` lines. The lock diff is only that block.
- `git diff --quiet a882251 -- crates/metadata crates/slicefs-cli crates/chunk-store docs/design/store-format.md`: `diff_rc=0`. `crates/prolly` did not change in this plan.
- Workspace suite `cargo test --workspace` with the four `--skip` names of the launch message: exit 0, 85 test binaries, 1,694 passed, 0 failed, 13 ignored, about 6 min (19:56 to 20:02 UTC). It ran on the frozen tree of the fix, before the fix commit. An earlier run on the Task 3 tree (before 32a6f77) also gave exit 0: 85 test binaries, 1,694 passed, 0 failed, 13 ignored.
- Tracer gate: the Task 1 verify (`cargo test -p prolly-bench`, the count script, the members and lock greps) passed on the same tree just before the Task 1 commit. A by-hand run of `gen`, `seed`, `whole`, `batches` and `prolly` gave `check mode=prolly ... ok=1`. Task 2 started only after that.
- Rust version: no `rust-toolchain` file and no `rust-version` in the workspace crates. The Docker image of `scripts/docker-linux-test` is `rust:1.100.0-bookworm`, so `as_chunks` (Rust 1.88) builds there too.
- Full log: `runs/gate-24-03.log`. Its first line shows head `3f2726c`, because the gate ran on the work tree of the fix before the fix commit.

### By-hand pass (release build, 2,000-file `gen tree` pair, `--ks "1 10" --commits 2`)

| Step | Wall time (s) |
|------|---------------|
| `gen tree` (version 1) | 0.612 |
| `gen tree` (version 2, `--change-every 10`) | 0.295 |
| `seed` | 0.497 |
| `whole` (setup, 8 small commits, version) | 0.747 |
| `batches` | 0.062 |
| `flat` | 0.546 |
| `prolly` | 0.549 |
| `scale --mode flat --n 100000` | 0.274 |
| `scale --mode prolly --n 100000` | 0.299 |

Some values of that pass (plumbing only, not the result of the phase; plan 24-05 runs Boost):

- 2,017 inodes after seed. The version step changed 212 files. By count, that is the 200 files that version 2 changes and the 12 content edits of the script.
- The `flat` and `prolly` checks gave `entries=2018` with the same digest as `batches` (`ok=1`). Each of the three modes printed 10 commit lines (seq 0 to 9).
- `hash boundary_hash_ns_per_key=101.0`; `tree depth=2 nodes=26 leaves=25 leaf_bytes_mean=3641`.
- `scale` at N = 100,000: flat build 119.4 ms with 24.6 MB peak RSS; flat apply1 45.8 ms with 4,100,008 serialized bytes. Prolly build 163.9 ms with 7.8 MB peak RSS; prolly apply1 40.8 ms with 7,572 serialized bytes and 3 new objects. The CPU time of each apply1 is below 6 ms, far below its wall time; from this, most of the wall time is probably the `cs.sync()` (not measured apart). One pass on a loaded machine, so the times are not results.

### By-hand removal check of the version step

The `gen tree` pair has no removed names, so a second by-hand pass (400 files, `--ks 4 --commits 1 --patterns scatter`) used a next tree with a removed directory (`d03`, 25 files), 2 removed files, 2 new directories with files, a file that becomes a directory and a directory that becomes a file. Result: `version files_changed=1 files_created=4 files_deleted=29 dirs_created=3 dirs_deleted=2`, the version batch `puts=10 dels=31`, and `check ... ok=1` for `flat` and `prolly` (396 entries, the digest of `batches`). The counts match the changes of the tree; `files_changed=1` is the one content edit of the script. The output is in the gate log.

## Deviations from Plan

1. **[Rule 3 - Blocking] `build.rs` with the FUSE-T rpath on macOS.** The bench links the `slicefs-cli` library and so `libfuse-t.dylib`. A link argument of a dependency build script (`crates/slicefs-cli/build.rs`) does not reach the bench binary. Thus the binary did not start outside `cargo` (`dyld: Library not loaded: @rpath/libfuse-t.dylib`). Under `cargo test` it ran, because cargo sets the library path. `benchmarks/prolly-bench/build.rs` gives the same `-rpath /usr/local/lib` as `slicefs-cli` gives `slicefs`. The file is not in the plan file list. Commit 2861417.
2. **[Rule 1 - Bug] `list_directory` gives `.` and `..`.** The version sync first took them as store names that are not in the tree and removed them. `remove` then recursed into `.` with no end (stack overflow). The sync and `remove` now skip both names (`is_dot`). Found by the Task 2 smoke test. Commit 157c024.
3. **[Discretion] The hash cost.** The plan says "time `boundary_hash(0, key)` over all base keys once". To keep the memory bounded, the keys are read from `base.bin` in pieces of 1,024 records (about 41 KB). Only the hash loop of each piece is timed, so the file reads are not in the number. `std::hint::black_box` keeps the release build from removing the loop. In 32a6f77 the piece was about 1 MiB. On Boost (`base.bin` about 3.4 MB) that buffer would add about 1 MiB to the peak RSS of the `prolly` process, which must be the prototype's own. The fix commit uses the small piece.
3b. **[Rule 1 - Bug] Batch files are streamed.** In 32a6f77, `flat` and `prolly` read a whole batch file, then made a vector of records, then (prolly) the edits: three copies of one batch at a time. The fix commit reads each batch file record by record (`for_each_batch`). `prolly` holds only its `Vec<Edit>` (one batch in RAM, D-02); `flat` applies the records straight to its map.
4. **[Discretion] Clippy fixes.** `as_chunks::<N>()` replaces `chunks_exact(N)`, and `checked_div` replaces a manual division check (`clippy::chunks_exact_to_as_chunks`, `manual_checked_div` of the stable toolchain).
5. **[Discretion] `seed` reads the root record with a read-only `ChunkStore::open`.** `batches`, `flat`, `prolly` and `scale` use `ChunkStore::open(dir, Options::default())`, as the plan says. The seeded store opens without problems that way.
6. **[Discretion] `--next-tree` must be a directory**, else exit 2. `gen tree` refuses `--max-bytes` above 1 MiB, because it holds one file in RAM.
7. **[Discretion] Scratch folder.** The launch message gave a fixed private scratch folder, so the plan's `mktemp -d` was not used. The gate log went through sed for the scratch path, the repo path and the home path.
8. **[Process] The GSD (Get Shit Done workflow) state steps were not done.** The launch message did not permit changes to `STATE.md` and `ROADMAP.md`. The commits are on `exec/24-03`. The plan-commit ledger file was not written, because the worktree tool guard refused the compound git command. The base `f12423c` is known from the start step.

## Notes for plans 24-04 and 24-05

- The `whole` mode changes the store that it gets. The driver must give it a copy of the seeded store.
- File mtimes go into the inode digests. Two runs on two `gen tree` outputs thus give other digests, but the same counts.
- The version step of a `gen tree` pair with the same file count creates no file. Use more files in the second tree (as the smoke test does) to get creates.
- On Linux, the bench needs the system FUSE library at run time, as `slicefs` does.

## Known Stubs

None.

## Threat Flags

None. The bench writes only under `--store`, `--out`, `--dir` and `--roots` (T-24-06). The only FFI is the `rusage` helper, with its review entry (T-24-07). No new registry package (T-24-SC).

## Open owner questions

None from this plan. Q-1 to Q-3 of 24-CONTEXT.md stay open.

## Self-Check: PASSED

- The six bench files and `runs/gate-24-03.log` exist. The commits 2861417, 157c024, 32a6f77 and 420451a are on `exec/24-03` (`git log f12423c..HEAD`).
