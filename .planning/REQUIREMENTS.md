# Requirements: SliceFS

**Defined:** 2026-11-26
**Milestone:** v2.3 Prolly Metadata & Robustness (release `v2.4.0`)
**Core Value:** High-ratio data deduplication that works transparently as a real, daily-driver POSIX filesystem

## v2.3 Requirements

### Prolly metadata maps (#215)

- [ ] **PMAP-01**: The map layout is decided and written down: one prolly map per metadata map, or one combined namespace map (Q-2 of #19).
- [ ] **PMAP-02**: The `inode_map` / `inode_data` duplication is removed; the root record holds only one of them.
- [ ] **PMAP-03**: The commit and read paths use prolly maps; a commit with one change does work proportional to the change, not to the store size.
- [ ] **PMAP-04**: The boundary rule uses a fixed-point table (no float test) and carries a version byte; the rule and its constants are in `docs/design/store-format.md`, together with the prolly node format.
- [ ] **PMAP-05**: Prolly stores carry a new store format flag; an old release refuses such a store; no backward compatibility with old stores.
- [ ] **PMAP-06**: GC mark reaches all prolly nodes with bounded memory (no whole map in RAM); online GC keeps working.
- [ ] **PMAP-07**: The Phase 24 benchmark on the product code shows: a one-change commit at 1,000,000 inodes or more takes at most 1/5 of the flat time; Boost numbers stay within the rubric in `docs/research/prolly-trees.md`; the insert/delete-heavy regression is measured and recorded with a mitigation or a written acceptance.
- [ ] **PMAP-08**: Crash tests: a commit cut at any point replays to a consistent tree.

### Store format migration (#155)

- [ ] **MIGR-01**: `scripts/migrate-store.sh` copies a store to a new format: old binary mounts the old store read-only, new binary mounts the new store, `rsync -aHAX` copies, then the script compares `slicefs stats` and a file checksum list.
- [ ] **MIGR-02**: The copy path is tested in Docker between two formats (old binary from a release, new binary from the branch, for example flat maps to prolly maps), or by hand with the log recorded if CI cannot get the old binary.
- [ ] **MIGR-03**: `docs/user/upgrade.md` explains how to see the store format, which releases read which format, the copy steps, the disk space needed, how to check the result, and what to do on failure.

### Crash and soak tests (#157)

- [ ] **CRSH-01**: A Docker Linux test runs a mixed load (create, write in place, append, rename, unlink, fsync, snapshot) from several processes on a live mount, kills the daemon with `kill -9` at a random time (and, in a second mode, stops the container), remounts, and checks: `slicefs scrub` exits 0, every fsynced file has its synced content, no file has content that was never written.
- [ ] **CRSH-02**: The test repeats N rounds with a recorded seed; a 50-round run passes; each run writes a short log (seed, rounds, result). It runs by hand and nightly on `ci/**` branches, not on every PR.
- [ ] **CRSH-03**: A soak mode runs the same load for hours with periodic unmount and mount and checks VmHWM against the memory cap; a 2-hour run passes.
- [ ] **CRSH-04**: `docs/design/write-log.md` has a "Crash tests" section: what the test checks and how to run it.

### Scrub damage report (#159)

- [ ] **SCRB-01**: When scrub finds a bad object, it prints the paths that reference it, in the current root and in each snapshot; memory stays bounded (manifests are streamed, no whole file and no full key set in RAM, see #210).
- [ ] **SCRB-02**: `slicefs scrub --json` gives the same report for scripts.
- [ ] **SCRB-03**: Exit codes are 0 (clean), 1 (corruption found), 2 (scrub could not run); a test plants a bad chunk that two files share, and scrub lists both files and exits 1.
- [ ] **SCRB-04**: `docs/user/scrub.md` explains when to run scrub, its run time, the output, the exit codes, and what to do after corruption; `slicefs scrub --help` matches it.

### Limits page (#158)

- [ ] **DOCS-01**: `docs/user/limits.md` gives the status (preview; the format can change until #156), the crash guarantees (from the write log and the crash tests), the limits (1 TiB EFBIG cap, FastCDC 4/8/32 KiB default, memory options, the 64 MiB reorder window, one mount per store), the platforms (Linux libfuse; macOS FUSE-T with FSKit only, default 1 worker; no Windows), and known issues with links.
- [ ] **DOCS-02**: Each fact on the page is checked against the code or a test, and the README has a short "Status and limits" section with a link.

## Future Requirements

- Format freeze (#156): an owner decision after #155, #157 and #161.
- Scrub key set on disk (#210) beyond what SCRB-01 needs; GC mark memory (#207).
- Release publishing (#178), refcount removal (#165).

## Out of Scope

| Feature | Reason |
|---------|--------|
| Reading old (flat-map) stores with the new binary | No store backward compatibility; the copy path (#155) is the upgrade |
| In-place `slicefs upgrade` command | #155 says "later, only if needed" |
| Prolly trees for the chunk index | Separate question (#110) |
| Change of the default FastCDC profile | Owner decision; stays 4/8/32 KiB |
| Crash tests on every PR | Keeps CI short; nightly on `ci/**` and by hand |

## Traceability

| Requirement | Phase | Status |
|-------------|-------|--------|
| PMAP-01 | Phase 25 | Pending |
| PMAP-02 | Phase 25 | Pending |
| PMAP-03 | Phase 26 | Pending |
| PMAP-04 | Phase 25 | Pending |
| PMAP-05 | Phase 25 | Pending |
| PMAP-06 | Phase 26 | Pending |
| PMAP-07 | Phase 26 | Pending |
| PMAP-08 | Phase 26 | Pending |
| MIGR-01 | Phase 27 | Pending |
| MIGR-02 | Phase 27 | Pending |
| MIGR-03 | Phase 27 | Pending |
| CRSH-01 | Phase 28 | Pending |
| CRSH-02 | Phase 28 | Pending |
| CRSH-03 | Phase 28 | Pending |
| CRSH-04 | Phase 28 | Pending |
| SCRB-01 | Phase 29 | Pending |
| SCRB-02 | Phase 29 | Pending |
| SCRB-03 | Phase 29 | Pending |
| SCRB-04 | Phase 29 | Pending |
| DOCS-01 | Phase 30 | Pending |
| DOCS-02 | Phase 30 | Pending |

**Coverage:**
- v2.3 requirements: 21 total
- Mapped to phases: 21
- Unmapped: 0

---
*Requirements defined: 2026-11-26*
*Traceability updated: 2026-11-26 after roadmap creation*
