# Requirements: SliceFS v2.2 Concurrency & Store Evolution

**Defined:** 2026-11-20
**Core Value:** High-ratio data deduplication that works transparently as a real, daily-driver POSIX filesystem
**Issues:** #127, #21, #22, #62, #67, #19
**Research:** `.planning/research/SUMMARY.md`

## v2.2 Requirements

### Spikes and baseline (#127)

- [x] **SPIKE-01**: Spike results S1-S7 are recorded in the threading research doc, and each result names the migration step it decides. S2 and S5 are measured again on the current code (after #108).
- [x] **SPIKE-02**: An in-process stress harness runs N threads of mixed operations (read, write, fsync, create, rename, unlink) on one file and on many files. It checks the data against a model, and it reports latency histograms and lock hold/wait times.
- [x] **SPIKE-03**: S1 (FUSE-T overlap and reply order) is run by hand on a macOS FSKit mount and recorded. The result decides whether the macOS default can be more than one worker.

### Fine-grained locks (#21)

- [x] **LOCK-01**: No global lock is held on the read or write path during disk I/O, a hash calculation, or a wait (store lock, `readers`, WAL append mutex, `dir_live`/`inode_live`).
- [x] **LOCK-02**: Per-file state stays in its table behind its own lock; no request takes another file's state out of `open_files` (the take-out pattern is gone, including `keep_budget`).
- [x] **LOCK-03**: Locks have one fixed rank (gate, rename, namespace stripes, file cell, short maps, store internals; two inodes in ascending stripe order (inode number hashed to a stripe)). A debug-build checker fails a test on a rank violation.
- [x] **LOCK-04**: Each log record is appended under the same locks as its apply (the append is a memory copy plus an LSN; disk write and sync happen outside the locks). Replay gives the same state as the apply, including creates by logged inode number. The #21 body is updated to match.
- [x] **LOCK-05**: A test with parallel readers and writers on one file shows correct data, and that readers do not wait for the I/O of writers. A test with parallel I/O on different files shows no contention between them. The one-file tests use several handles on the same inode with different modes (read-only, read-write, `O_APPEND`), and they show the POSIX rules (POSIX.1 2.9.7). A read through one handle sees a write that has completed through another handle. A read never sees part of a concurrent write: it sees all of the write or none of it. Concurrent `O_APPEND` writes through different handles never overlap and lose no bytes. The end-of-file offset of an append comes from the current size under the inode lock.
- [x] **LOCK-06**: The shared dirty budget is safe with N writers: a background flusher lowers it, a writer that is over the limit sleeps before it takes any lock, and every wait has a time limit. A test with all workers throttled while a commit waits does not deadlock, and peak RSS stays under the set limit.

### Worker pool (#22)

- [x] **POOL-01**: One FUSE reader hands each request to a SliceFS worker pool. The number of workers is a mount option; with one worker the handler runs inline as today.
- [x] **POOL-02**: Unmount drains the queue and joins the workers before `destroy()`. Replies are sent after all locks are released.
- [x] **POOL-03**: A worker panic aborts the daemon; the write log replays at the next mount (no request is served after a half-done update).
- [x] **POOL-04**: All tests and pjdfstest pass with 4 workers on Linux (Docker CI). On macOS FSKit CI, tests pass with the macOS default from SPIKE-03.
- [x] **POOL-05**: Parallel I/O on different files scales with the number of workers, measured on Linux, and the result is recorded.

### Store format (#154)

- [x] **FMT-01**: `init` (the first open of a new store) writes a store header at the store root with a store format number and compat, ro-compat and incompat flag sets.
- [x] **FMT-02**: A store with an unknown incompat flag is refused; with an unknown ro-compat flag it mounts read-only. The message names the flag and the lowest release that knows it.
- [x] **FMT-03**: A store with data and no header is refused with a clear message.
- [x] **FMT-04**: `slicefs stats` prints the format number and the flags.
- [x] **FMT-05**: `docs/design/store-format.md` gives the header layout, the flag classes, the flag list, the error messages and the format policy, and matches the code; the README links it.

### Thread-safety verification (Phase 21.2)

- [x] **TSAFE-01**: A CI workflow (Safety) runs as the release gate (release.yml runs it and creates the tag only when every job passes), on a v* tag push, and by manual run on any branch, and fails on any finding: `cargo clippy --workspace --all-targets -- -D warnings`, Miri on the unit tests that reach `unsafe` in chunk-store and metadata, ThreadSanitizer (pinned nightly, Linux) on the multi-worker tests and a short stress run, the model-checking job, Kani proofs, and a `deadlock-detect` stress run. Each job has a time limit.
- [x] **TSAFE-02**: loom or shuttle models cover every thread order (with at most 3 preemptions in CI; unbounded for the `*_all_orders` models) of the worker pool, the shared dirty budget and the commit gate.
  - Models (loom): every thread order with at most 3 preemptions (CI bound), and every order for the models named `*_all_orders`; see 21.2-01-SUMMARY.
- [x] **TSAFE-03**: The two Phase 19 races are fixed, each with a test that failed before the fix and a model or stress test that stays in CI: (a) a size change of a closed file against unlink or setattr; (b) a cross-directory rename with no ancestor check. If either cannot be fixed in this phase, the Linux default is `--workers 1` until it is.
- [x] **TSAFE-04**: Kani proves small pure code (the header parser, size limits) free of panic and overflow.
- [x] **TSAFE-05**: Line coverage is measured in CI, also in Linux Docker with the FUSE checks, and the numbers are in the CI log. A drop below the set floor fails the Safety gate.
- [x] **TSAFE-06**: The `unsafe` blocks that Miri cannot run (FFI in slicefs-cli and vendor/fuser, and any other block Miri does not execute) have a written review in `docs/design/unsafe-review.md`.

### Chunk profiles (#67)

- [x] **PROF-01**: A store has one FastCDC profile (min/avg/max, powers of two, inside the crate limits), set at `seed` and kept in the store format marker. A mount with a different profile is refused. Without a setting, the profile is 4/8/32 KiB.
- [x] **PROF-02**: Every use of the chunk constants (writer buffers, zero-chunk shortcut, splice, read-ahead) reads the store profile. Two stores with different profiles work in one process. A golden cut-point test protects the cut points.
- [x] **PROF-03**: A reproducible benchmark reports, for 4/8/32, 8/16/64, 8/32/128, 16/64/256 and 32/128/512 KiB: physical bytes after dedup and zstd, object count, index RAM and cold-open time, ingest throughput, read latency, and rewrite bytes after small edits. The report recommends a default; the default does not change in v2.2.

### Hashes and rehash (#62)

- [x] **HASH-01**: SHA-256 is a production write hash with its own algorithm tag (hardware-accelerated where the CPU has it). A store can be seeded or mounted with `--hash sha256`. BLAKE3-256 stays the default.
- [x] **HASH-02**: `slicefs rehash --to <algorithm> <store>` rewrites all objects (file chunks, manifests, metadata nodes, roots and snapshots) under the new algorithm. A rehashed file gets the same key as a fresh write of its bytes.
- [x] **HASH-03**: Rehash is offline: it refuses while the store is mounted (OS file lock) or while the write log has records after the last root.
- [x] **HASH-04**: Rehash is crash-safe: a crash at any point leaves a store that mounts with the old or the new root. GC frees the old objects after the switch. Old objects are verified as they are read.
- [x] **HASH-05**: Rehash reports progress, bytes rewritten and bytes freed, and has `--dry-run`. Its memory is bounded (no in-RAM map of all objects).
- [x] **HASH-06**: Tests: a BLAKE3-256 store and a SHA-256 store each convert to the other and back. A store with mixed algorithms ends with one algorithm. Files and snapshots do not change, and scrub passes.

### Prolly-tree evaluation (#19)

- [ ] **PROLLY-01**: A prolly-tree prototype for one metadata map (for example the directory map) is a separate crate, not wired into the root record. Its node boundaries depend only on the keys, with a size-aware rule. Its nodes are chunk-store objects.
- [ ] **PROLLY-02**: A benchmark on the Boost trees (about 78,000 files per version) compares the current commit path with the prototype: commit time, CPU, peak memory, bytes written per commit, and on-disk size after several small commits.
- [ ] **PROLLY-03**: A written decision (adopt, later, or drop) is in the design docs and on #19. It also states that file manifests are already content-cut trees.

## Future Requirements

Deferred; tracked, not in this roadmap.

- WAL group commit (spike S6 decides).
- One pack per worker (spike S3 decides).
- `FUSE_WRITEBACK_CACHE` trial and a persistent extent map (spike S4 decides).
- More than one worker on macOS, if SPIKE-03 shows that FUSE-T does not overlap requests.
- Online rehash with online GC.
- Prolly-tree metadata migration and snapshot diff.
- A rechunk tool to change the profile of a populated store.
- Rebase of the vendored fuser onto 0.19.

## Out of Scope

| Feature | Reason |
|---------|--------|
| fuser `n_threads` / `clone_fd` | The vendored fuser refuses `n_threads` outside Linux; a SliceFS worker pool gives one model on both platforms |
| Change of the default chunk profile | FastCDC stays 4/8/32 KiB; the benchmark only recommends |
| Profile change on a populated store | Breaks dedup and splice keys; needs a rechunk tool |
| Async runtime (tokio etc.) | fuser callbacks are synchronous; threads are enough |
| `dashmap`, `crossbeam`, `rayon`, `im`, `loom`, prolly crates | See `.planning/research/STACK.md`. Runtime dependencies only: Phase 21.2 may add loom or shuttle as a dev-only test dependency (21.2-CONTEXT D-19) |

## Traceability

| Requirement | Phase | Status |
|-------------|-------|--------|
| SPIKE-01 | Phase 18 | Complete |
| SPIKE-02 | Phase 18 | Complete |
| SPIKE-03 | Phase 18 | Complete |
| LOCK-01 | Phase 19 | Complete |
| LOCK-02 | Phase 20 | Complete |
| LOCK-03 | Phase 19 | Complete |
| LOCK-04 | Phase 19 | Complete |
| LOCK-05 | Phase 19 | Complete |
| LOCK-06 | Phase 20 | Complete |
| POOL-01 | Phase 21 | Complete |
| POOL-02 | Phase 21 | Complete |
| POOL-03 | Phase 21 | Complete |
| POOL-04 | Phase 21 | Complete |
| POOL-05 | Phase 21 | Complete |
| FMT-01 | Phase 21.1 | Complete |
| FMT-02 | Phase 21.1 | Complete |
| FMT-03 | Phase 21.1 | Complete |
| FMT-04 | Phase 21.1 | Complete |
| FMT-05 | Phase 21.1 | Complete |
| TSAFE-01 | Phase 21.2 | Complete |
| TSAFE-02 | Phase 21.2 | Complete |
| TSAFE-03 | Phase 21.2 | Complete |
| TSAFE-04 | Phase 21.2 | Complete |
| TSAFE-05 | Phase 21.2 | Complete |
| TSAFE-06 | Phase 21.2 | Complete |
| PROF-01 | Phase 22 | Complete |
| PROF-02 | Phase 22 | Complete |
| PROF-03 | Phase 22 | Complete |
| HASH-01 | Phase 23 | Complete |
| HASH-02 | Phase 23 | Complete |
| HASH-03 | Phase 23 | Complete |
| HASH-04 | Phase 23 | Complete |
| HASH-05 | Phase 23 | Complete |
| HASH-06 | Phase 23 | Complete |
| PROLLY-01 | Phase 24 | Pending |
| PROLLY-02 | Phase 24 | Pending |
| PROLLY-03 | Phase 24 | Pending |

**Coverage:**
- v2.2 requirements: 37 total
- Mapped to phases: 37
- Unmapped: 0

---
*Requirements defined: 2026-11-20*
