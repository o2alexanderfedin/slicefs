# Requirements: DedupFS (SliceFS)

**Defined:** 2026-11-17
**Core Value:** High-ratio data deduplication that works transparently as a real, daily-driver filesystem.

## Milestone v2.1 Requirements

### Write log close-out (#20)

- [ ] **WLOG-01**: Each acceptance item of #20 is mapped to a test that passes, or to a follow-up issue. The `kill -9` items: a file survives after its `fsync` returns, a rename survives after `fsync` of the directory, and a torn last record does not stop the mount.
- [ ] **WLOG-02**: The out-of-date item "786 MB copy over FUSE-T NFS" is restated for the current backends (FSKit on macOS, libfuse on Linux in Docker), and the restated check passes with bounded daemon memory.
- [ ] **WLOG-03**: The item "commit time proportional to the changed data" points to #93. #20 is closed with a comment that lists the evidence.

### Pack-file measurements (#8)

- [ ] **PACK-01**: A repeatable script measures `slicefs seed` throughput and on-disk store size for the Boost tree, a tar file and random data.
- [ ] **PACK-02**: The script runs on a build from before packs (#56) and on the current build. The numbers go into `docs/design/chunk-store.md` ("Measurements to take"), with the machine and the commits.
- [ ] **PACK-03**: The store file count after a Boost copy is recorded (target: tens, not hundreds of thousands). #8 is closed when the numbers are recorded. The FSKit Boost round trip is run by the user (no macOS mounts in automated tests).

### Chunk index benchmark (#66)

- [ ] **BNCH-01**: A benchmark makes stores with 1M, 10M and 100M objects (small synthetic objects, to keep disk use low) and measures cold-open time and peak RSS of `ChunkStore::open_with`.
- [ ] **BNCH-02**: The benchmark measures lookup throughput for a dedup hit and for a dedup miss, and the recovery time of a pack without a valid index file.
- [ ] **BNCH-03**: The results (RAM per object, scan throughput) are documented and are the baseline for #85.

### On-disk chunk index (#85)

- [ ] **IDX-01**: The index is sorted runs of `(ObjectId, Loc)` on disk with a sparse key table, a memtable for the entries since the last flush, and one filter per run in RAM. A lookup does at most one disk read on a hit and usually none on a miss.
- [ ] **IDX-02**: Cold open reads only the run footers and filters. The open time does not grow linearly with the object count.
- [ ] **IDX-03**: The pack records stay the source of truth. After a crash, entries not yet in a run are found again from the packs. A crash during a run flush or a merge loses no object and gives no wrong `Loc` (crash tests).
- [ ] **IDX-04**: Runs merge in the background, so their number stays small. No lookup returns an object that online GC removed (GC tests with `gc_begin` / mark / `gc_finish`).
- [ ] **IDX-05**: The #66 benchmark runs with the new index and shows RAM per object (target: a few bytes), cold-open time, hit and miss lookup throughput, and write throughput, compared with the baseline.

### Commit cost of open Buffered files (#93)

- [ ] **BUFR-01**: A commit with a large open Buffered file writes to the write log an amount that depends on the bytes changed since the last commit, not on the file size.
- [ ] **BUFR-02**: Replay after a crash still gives the same content, size and times (tests in `write_log_replay_tests`, for PerOp and FlushOnFsync).
- [ ] **BUFR-03**: The commit gate is not held for the hash of the whole Buffered file.

## Future Requirements

- Several roots, commits and refs (#86, #87-#92) — waits on the naming decision in #86.
- Prolly trees for metadata maps and manifests (#19).
- `slicefs rehash` (#62) — needs a second real hash algorithm first.

## Out of Scope

| Feature | Reason |
|---------|--------|
| Migration of old stores to the new index format | No deployments yet; no store backward compatibility |
| macOS mounts in automated tests | Mounts hang the kernel; FSKit runs are by hand |
| A second hash algorithm | Not needed for this milestone; #62 stays deferred |

## Traceability

| Requirement | Phase | Status |
|-------------|-------|--------|

**Coverage:**
- v2.1 requirements: 17 total
- Mapped to phases: 0 (set by the roadmap)

---
*Requirements defined: 2026-11-17*
*Last updated: 2026-11-17 after initial definition*
