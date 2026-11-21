# Requirements: SliceFS v2.2 Concurrency & Store Evolution

**Defined:** 2026-11-20
**Core Value:** High-ratio data deduplication that works transparently as a real, daily-driver POSIX filesystem
**Issues:** #127, #21, #22, #62, #67, #19
**Research:** `.planning/research/SUMMARY.md`

## v2.2 Requirements

### Spikes and baseline (#127)

- [ ] **SPIKE-01**: Spike results S1-S7 are recorded in the threading research doc, and each result names the migration step it decides. S2 and S5 are measured again on the current code (after #108).
- [x] **SPIKE-02**: An in-process stress harness runs N threads of mixed operations (read, write, fsync, create, rename, unlink) on one file and on many files. It checks the data against a model, and it reports latency histograms and lock hold/wait times.
- [x] **SPIKE-03**: S1 (FUSE-T overlap and reply order) is run by hand on a macOS FSKit mount and recorded. The result decides whether the macOS default can be more than one worker.

### Fine-grained locks (#21)

- [ ] **LOCK-01**: No global lock is held on the read or write path during disk I/O, a hash calculation, or a wait (store lock, `readers`, WAL append mutex, `dir_live`/`inode_live`).
- [ ] **LOCK-02**: Per-file state stays in its table behind its own lock; no request takes another file's state out of `open_files` (the take-out pattern is gone, including `keep_budget`).
- [ ] **LOCK-03**: Locks have one fixed rank (gate, rename, namespace stripes, file cell, short maps, store internals; two inodes in increasing inode order). A debug-build checker fails a test on a rank violation.
- [ ] **LOCK-04**: Each log record is appended under the same locks as its apply (the append is a memory copy plus an LSN; disk write and sync happen outside the locks). Replay gives the same state as the apply, including creates by logged inode number. The #21 body is updated to match.
- [ ] **LOCK-05**: A test with parallel readers and writers on one file shows correct data, and that readers do not wait for the I/O of writers. A test with parallel I/O on different files shows no contention between them. The one-file tests use several handles on the same inode with different modes (read-only, read-write, `O_APPEND`), and they show the POSIX rules (POSIX.1 2.9.7). A read through one handle sees a write that has completed through another handle. A read never sees part of a concurrent write: it sees all of the write or none of it. Concurrent `O_APPEND` writes through different handles never overlap and lose no bytes. The end-of-file offset of an append comes from the current size under the inode lock.
- [ ] **LOCK-06**: The shared dirty budget is safe with N writers: a background flusher lowers it, a writer that is over the limit sleeps before it takes any lock, and every wait has a time limit. A test with all workers throttled while a commit waits does not deadlock, and peak RSS stays under the set limit.

### Worker pool (#22)

- [ ] **POOL-01**: One FUSE reader hands each request to a SliceFS worker pool. The number of workers is a mount option; with one worker the handler runs inline as today.
- [ ] **POOL-02**: Unmount drains the queue and joins the workers before `destroy()`. Replies are sent after all locks are released.
- [ ] **POOL-03**: A worker panic aborts the daemon; the write log replays at the next mount (no request is served after a half-done update).
- [ ] **POOL-04**: All tests and pjdfstest pass with 4 workers on Linux (Docker CI). On macOS FSKit CI, tests pass with the macOS default from SPIKE-03.
- [ ] **POOL-05**: Parallel I/O on different files scales with the number of workers, measured on Linux, and the result is recorded.

### Chunk profiles (#67)

- [ ] **PROF-01**: A store has one FastCDC profile (min/avg/max, powers of two, inside the crate limits), set at `seed` and kept in the store format marker. A mount with a different profile is refused. Without a setting, the profile is 4/8/32 KiB.
- [ ] **PROF-02**: Every use of the chunk constants (writer buffers, zero-chunk shortcut, splice, read-ahead) reads the store profile. Two stores with different profiles work in one process. A golden cut-point test protects the cut points.
- [ ] **PROF-03**: A reproducible benchmark reports, for 4/8/32, 8/16/64, 8/32/128, 16/64/256 and 32/128/512 KiB: physical bytes after dedup and zstd, object count, index RAM and cold-open time, ingest throughput, read latency, and rewrite bytes after small edits. The report recommends a default; the default does not change in v2.2.

### Hashes and rehash (#62)

- [ ] **HASH-01**: SHA-256 is a production write hash with its own algorithm tag (hardware-accelerated where the CPU has it). A store can be seeded or mounted with `--hash sha256`. BLAKE3-256 stays the default.
- [ ] **HASH-02**: `slicefs rehash --to <algorithm> <store>` rewrites all objects (file chunks, manifests, metadata nodes, roots and snapshots) under the new algorithm. A rehashed file gets the same key as a fresh write of its bytes.
- [ ] **HASH-03**: Rehash is offline: it refuses while the store is mounted (OS file lock) or while the write log has records after the last root.
- [ ] **HASH-04**: Rehash is crash-safe: a crash at any point leaves a store that mounts with the old or the new root. GC frees the old objects after the switch. Old objects are verified as they are read.
- [ ] **HASH-05**: Rehash reports progress, bytes rewritten and bytes freed, and has `--dry-run`. Its memory is bounded (no in-RAM map of all objects).
- [ ] **HASH-06**: Tests: a BLAKE3-256 store and a SHA-256 store each convert to the other and back. A store with mixed algorithms ends with one algorithm. Files and snapshots do not change, and scrub passes.

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
| `dashmap`, `crossbeam`, `rayon`, `im`, `loom`, prolly crates | See `.planning/research/STACK.md` |

## Traceability

| Requirement | Phase | Status |
|-------------|-------|--------|
| SPIKE-01 | Phase 18 | Pending |
| SPIKE-02 | Phase 18 | Complete |
| SPIKE-03 | Phase 18 | Complete |
| LOCK-01 | Phase 19 | Pending |
| LOCK-02 | Phase 20 | Pending |
| LOCK-03 | Phase 19 | Pending |
| LOCK-04 | Phase 19 | Pending |
| LOCK-05 | Phase 19 | Pending |
| LOCK-06 | Phase 20 | Pending |
| POOL-01 | Phase 21 | Pending |
| POOL-02 | Phase 21 | Pending |
| POOL-03 | Phase 21 | Pending |
| POOL-04 | Phase 21 | Pending |
| POOL-05 | Phase 21 | Pending |
| PROF-01 | Phase 22 | Pending |
| PROF-02 | Phase 22 | Pending |
| PROF-03 | Phase 22 | Pending |
| HASH-01 | Phase 23 | Pending |
| HASH-02 | Phase 23 | Pending |
| HASH-03 | Phase 23 | Pending |
| HASH-04 | Phase 23 | Pending |
| HASH-05 | Phase 23 | Pending |
| HASH-06 | Phase 23 | Pending |
| PROLLY-01 | Phase 24 | Pending |
| PROLLY-02 | Phase 24 | Pending |
| PROLLY-03 | Phase 24 | Pending |

**Coverage:**
- v2.2 requirements: 26 total
- Mapped to phases: 26
- Unmapped: 0

---
*Requirements defined: 2026-11-20*
