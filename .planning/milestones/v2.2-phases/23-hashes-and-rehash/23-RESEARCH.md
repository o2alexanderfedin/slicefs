# Phase 23: Hashes and rehash - Research

**Researched:** 2026-11-26
**Domain:** content-addressed store (chunk-store, metadata, CLI): a second production hash (SHA-256) and an offline, crash-safe conversion of a whole store between hashes (issue #62)
**Confidence:** HIGH for the code facts (every discrete value below was read in a source file this session); MEDIUM for performance and size estimates (marked `[ASSUMED]`)

<user_constraints>
## User Constraints (from CONTEXT.md)

### Locked Decisions

#### Locked by the owner (do not reopen)
- **D-01:** SHA-256 is a production second hash (HASH-01). BLAKE3-256 stays the default. Object IDs name their hash (1 tag byte + 32-byte digest). Full digests, no bit saving (global/decentralized goal).
- **D-02:** Rehash is offline only. It refuses while mounted by the existing exclusive store lock: `header::lock_offline` in `crates/slicefs-cli/src/header.rs` (a writable mount holds it for its whole life since #201; a read-only mount holds it shared). Not the `mount.lock` file. It also refuses while the write log has records after the last root.
- **D-03:** No store backward compatibility (no deployments). The Phase 21.1 header and format policy (`docs/design/store-format.md`) governs any new header line or flag.
- **D-04:** Online GC stays mandatory; a GC frees the old objects after the switch. Memory is bounded: no in-RAM map of all objects; per-file memory never O(file size).
- **D-05:** Rehash keeps the chunk profile of the store (Phase 22): a rehashed file gets the same key as a fresh write of its bytes under the store profile and the target hash.
- **D-06:** Crash safety: a crash at any point leaves a store that mounts with the old or the new root.
- **D-07:** Safety gates (`scripts/safety/*`, release gate) stay green; the unsafe-count doc matches (`scripts/safety/unsafe-count.sh`). Tests are short; benchmarks only by hand. Local gate: `timeout 3600 cargo test --workspace -- --skip a_panic_on_another_thread_does_not_abort --skip a_reader_thread_panic_aborts_and_the_remount_replays --skip a_worker_panic_aborts_and_the_remount_replays --skip a_full_queue_that_stays_full_aborts`.
- **D-08:** Plans run as executors in their own git worktrees (local branch `exec/23-NN`). Executors never push. No local macOS mounts; Linux FUSE tests only in OrbStack Docker (`scripts/docker-linux-test/run.sh`), and only if a path is not reachable in process.

#### SHA-256 hasher
- [auto] Q: "Name, tag and place of the SHA-256 hasher?" -> Selected: "`Sha256` in `crates/chunk-store/src/hash.rs`, name `sha256`, tag = multihash code of SHA2-256 (0x12)" (recommended default)
- **D-09:** A `Sha256` type next to `Blake3_256` in `hash.rs`, in `HASHERS` in all builds. Name `sha256` (the word in HASH-01 and the roadmap). Tag 0x12, the multihash code of sha2-256, as 0x1e is the multihash code of BLAKE3. Research confirms the code and that no test or fixture uses 0x12 already.
- **D-10:** Hardware acceleration by a maintained crate with run-time CPU detection (SHA-NI on x86_64, ARMv8 SHA2 on aarch64), no new `unsafe` code of ours, so the unsafe count does not change. Default: RustCrypto `sha2` (its `cpufeatures` detection). Research checks what the workspace dependency `sha2-compress` (in `Cargo.toml` and `crates/slicefs-cli/Cargo.toml`, with no source use found) is, and whether to use or drop it. A golden test checks the digest against a known SHA-256 vector, and `--hash sha256` round-trips through seed and mount in process.

#### Header for SHA-256
- [auto] Q: "Does a store with SHA-256 objects need a flag (21.1 rule)?" -> Selected: "Yes: an incompat flag written when the store first uses SHA-256, and kept" (recommended default)
- **D-11:** An older format-7 build that does not know SHA-256 cannot read SHA-256 objects. It already refuses a `hash sha256` line, but a store can have live SHA-256 objects with the hash line back at `blake3-256` (after `mount --hash`, or mid-rehash). So by the 21.1 rule ("it cannot read the store: incompat"), the store gets the flag line `incompat hash-sha256 <release>` when it first gets SHA-256 objects (seed, mount or rehash with `--hash sha256` / `--to sha256`), before any SHA-256 object is written. `hash-sha256` goes into `KNOWN_FLAGS`. A `hash sha256` line without the flag is refused as a bad header (M9); the flag without the line is valid (the store may still hold SHA-256 objects). The flag stays after a rehash back to BLAKE3-256; removing it when a GC proves no SHA-256 object is left is the planner's choice only if it is cheap and exact, else deferred. The `<release>` word comes from one constant (as `CHUNK_PROFILE_RELEASE`). If the byte-level header core proved by Kani changes, the harness is updated and `scripts/safety/kani.sh` stays green. — **Reversibility:** reversible — no deployed stores, format not frozen; one parse rule and one flag row.

#### Conversion method
- [auto] Q: "How is a file converted?" -> Selected: "Rebuild through the real write path under the target hasher, not a node-by-node ID translation" (recommended default)
- **D-12:** Manifest node boundaries depend on IDs (`is_cut` in `crates/chunk-store/src/file.rs` looks at the last 4 digest bytes), so new IDs move node cuts, and a node-by-node translation of an old manifest cannot give the key of a fresh write. So a file is converted by streaming its bytes (verified reads, bounded buffer) into a `FileWriter` of a store opened with the target hasher and the store's chunk profile; the result is by construction the key of a fresh write (D-05). Research checks whether the chunk cut points depend only on bytes and profile (they should: FastCDC on the data), so that a faster path (reuse the old chunk boundaries, re-hash each chunk, rebuild the manifest with the real manifest builder) can give the same key without re-cutting; the planner may use it only if a test proves key equality with a fresh write. Zero runs: the zero IDs are per store and per hasher (Phase 22 D-15), so the conversion store must be opened with the header profile and the target hasher, and the sparse shortcut must give the fresh-write key.
- **D-13:** Metadata nodes (directory map, inode map, xattrs, manifests of metadata values) are rebuilt bottom-up: decode a node, replace every content key and child-node key by its new key, encode, and put it under the target hasher. Keys that are not content addresses (a directory-entry key is BLAKE3-256 of the name, see `docs/design/chunk-store.md`) do not change. Research lists every node type and every place a key is stored, and states whether any metadata map uses ID-dependent boundaries (then it must be rebuilt by its real builder, as D-12). The new root must decode to the same tree (same names, inodes, attributes, sizes, file bytes) as the old root.
- **D-14:** An object that already has the target tag and whose whole subtree has the target tag is kept as is (no rewrite). A mixed subtree (for example a manifest of the target tag over chunks of another tag, which splice after `mount --hash` may make) is rebuilt. Research confirms whether mixed manifests can exist.
- **D-15:** Old objects are read with the verifying path (`ChunkStore::read`, which checks each object with the hasher of its tag), never `get`. A bad object or an unknown tag stops the rehash with an error before the switch; nothing is switched.

#### Old-to-new map
- [auto] Q: "How does rehash avoid redoing shared subtrees (snapshots share most nodes and files)?" -> Selected: "A disk-backed map in a temp dir of the store, with a bounded RAM cache; correctness never depends on it" (recommended default)
- **D-16:** The map is keyed by old file content key and old metadata node key (not by single chunk: chunks dedup by themselves under the target hash). RAM is bounded by a fixed cache size; the rest is on disk in `<store>/rehash-tmp/` (the `gc-tmp` pattern; a writable open removes what a crash left, as for `gc-tmp`). Research picks the structure (random lookups during a walk: an on-disk hash table of fixed-size records, or sorted runs with `extsort` if the walk can be ordered). A miss only costs a rewrite (the result is the same key), so a lost or partial map after a crash is safe. Memory and disk cost are stated in the docs.

#### Write-log and lock checks
- **D-17:** Order: take `header::lock_offline` first (exclusive; refused while any mount or offline command holds it), then the header check, then the write-log check with the streaming `scan_store_with_log` (stop at the first record after the last root; `load_store_with_log` keeps all records in RAM and is not used). Message for log records: the store has write-log records that are not in a root; mount and unmount the store once, then run rehash again. The lock is held to the end, GC included.

#### Switch protocol and snapshot supersession
- [auto] Q: "How do the root and all snapshot roots switch at once?" -> Selected: "One atomic switch: new objects synced first, then one durable segment step that holds the new root and a superseding record for every snapshot" (recommended default)
- **D-18:** Order of a run: (1) checks (D-17); (2) if the target is SHA-256, write the flag (D-11) durably; (3) rewrite all live objects under the target hasher and sync the packs and index; (4) switch: the new root and the new snapshot roots become current in one durable step; (5) rewrite the header `hash` line to the target; (6) GC frees the old objects; (7) report. Legal states after a crash: before (4) the store mounts with the old root (new objects are garbage for the next GC; a new rehash redoes the work, mostly as dedup hits); after (4) it mounts with the new root, maybe with the old hash line (a mixed store is legal and readable) and old objects not yet freed. A rerun of `rehash` after a crash at any step completes the job. A crash test with in-process fail points covers each step boundary.
- **D-19:** Snapshot records are append-only (`load_store_from_segments` collects every `SnapshotRecord`; there is no snapshot delete). Rehash must keep each snapshot's version, name and `created_at` and change only its root. Research decides between (a) a superseding `SnapshotRecord` with the same version (the loader keeps the last record of each version), written with the `RootUpdate` in one new segment that becomes visible atomically (temp file, sync, rename, dir sync), and (b) an atomic rewrite of the segment files. Whatever is chosen, every loader of snapshots (mount, gc, online gc, scrub, `snapshot list`/`switch`, segment compaction) sees exactly one root per snapshot, so a GC after the switch does not keep the old snapshot trees alive. Default: (a), because it adds one loader rule and no segment-format change. — **Reversibility:** reversible — no deployed stores.

#### GC and report
- **D-20:** Rehash runs the GC itself after the switch (step 6), under the store lock it already holds, and reports the bytes freed from it. `gc_report` takes `lock_offline` itself, so the plan adds a GC entry that runs with the held lock (no second lock). Default GC mode: full (exact mark, no holes) so that all old objects go; research checks its cost and memory bound. `--no-gc` is not added unless research finds a need.
- **D-21:** Report: progress lines on stderr at a fixed interval (objects and bytes done, phase of the run), and a final summary on stdout: objects and bytes read, objects and bytes rewritten (new objects written), objects kept as is, bytes freed by the GC, and the store disk usage before and after (the `gc` usage line). Plain short text; wording is the planner's choice.
- **D-22:** `--dry-run` writes nothing: no temp dir, no flag, no header rewrite, no pack. It walks the live tree read-only (`open_read_only`), verifies objects, and reports what a real run would rewrite (counts and bytes of objects with another tag; an estimate is fine for bytes freed, and the report says it is an estimate). It still takes the store lock and does the write-log check, so it reports the same refusals as a real run.

#### CLI
- **D-23:** `slicefs rehash --to <algorithm> [--dry-run] <store>`. `--to` takes a known hasher name (`parse_hash`). The store must exist (`require_store`). If every live object already has the target tag, rehash says so, still makes sure the header hash line is the target, and changes nothing else (a GC run is not needed then).

#### Tests
- **D-24:** HASH-06 in process (no mount): a BLAKE3-256 store and a SHA-256 store each convert to the other and back; a mixed store (`TestKeyedBlake3` or `mount --hash`-style hash change) ends with one tag on every live object; files (bytes, sizes, sparse/zero runs, a spliced file) and snapshots (version, name, `created_at`, content) do not change; scrub passes; the key of each rehashed file equals the key of a fresh write of its bytes under the target hash and the store profile, for the default profile and one large profile. Crash tests: fail points at each step boundary of D-18, then reopen: the store loads the old or the new root and a rerun finishes. Refusal tests: lock held (exclusive and shared), write-log records after the last root, unknown target, an object that fails verification. A memory test or bound check shows the map does not grow with the object count beyond the cache size.

### Claude's Discretion
- Type, function and module names; message wording (plain, short); the flag word `hash-sha256` may change if the planner has a better kebab-case name; progress interval; map structure details within D-16; whether the faster chunk-reuse path of D-12 is used.
- Suggested plan split (planner may change it; plans in one wave must not touch the same files): 01 SHA-256 hasher, `--hash sha256`, flag `hash-sha256`, docs (wave 1); 02 snapshot supersession in the segment loader and the atomic switch writer (wave 1, metadata crate); 03 rehash engine: file and metadata rebuild, disk-backed map, bounded memory (wave 2); 04 CLI command, checks, dry-run, GC with the held lock, report, crash and HASH-06 tests, docs (wave 3).

### Deferred Ideas (OUT OF SCOPE)
- An online rehash job that works with the online GC (issue #62 allows it; the owner chose offline only).
- Removing the `hash-sha256` flag after a GC proves no SHA-256 object is left (unless cheap and exact in this phase, D-11).
- A rechunk command (another chunk profile for an existing store).
- More hashes beyond SHA-256.
</user_constraints>

<phase_requirements>
## Phase Requirements

| ID | Description | Research Support |
|----|-------------|------------------|
| HASH-01 | SHA-256 is a production write hash with its own algorithm tag (hardware-accelerated where the CPU has it). A store can be seeded or mounted with `--hash sha256`. BLAKE3-256 stays the default. | Q1 (sha2 0.11.2, tag 0x12, `HASHERS`), Q2 (flag `hash-sha256`, header change points) |
| HASH-02 | `slicefs rehash --to <algorithm> <store>` rewrites all objects (file chunks, manifests, metadata nodes, roots and snapshots) under the new algorithm. A rehashed file gets the same key as a fresh write of its bytes. | Q3 (file rebuild by `FileWriter`), Q4 (metadata inventory and per-kind translation), Q5 (disk map) |
| HASH-03 | Rehash is offline: it refuses while the store is mounted (OS file lock) or while the write log has records after the last root. | Q6 (flock semantics), Q8 (`scan_store_with_log`) |
| HASH-04 | Rehash is crash-safe: a crash at any point leaves a store that mounts with the old or the new root. GC frees the old objects after the switch. Old objects are verified as they are read. | Q7 (switch segment, crash table), Q6 (GC entry), Q9 (verified reads), Q10 (fail points) |
| HASH-05 | Rehash reports progress, bytes rewritten and bytes freed, and has `--dry-run`. Its memory is bounded (no in-RAM map of all objects). | Q5 (map), Q6 (bytes freed, GC memory), Q11 (dry-run), Memory bound table |
| HASH-06 | Tests: a BLAKE3-256 store and a SHA-256 store each convert to the other and back. A store with mixed algorithms ends with one algorithm. Files and snapshots do not change, and scrub passes. | Q12 (test infrastructure), Validation Architecture |
</phase_requirements>

## Summary

SHA-256 is a small change: one `ObjectHasher` in `crates/chunk-store/src/hash.rs`, the RustCrypto crate `sha2` 0.11.2 (its backends for SHA-NI and ARMv8 SHA2 are selected at run time by `cpufeatures`, with no `unsafe` of ours), one header flag `incompat hash-sha256`, and one new parse rule. The Kani-proved byte core of `header.rs` does not change. The unused workspace dependency `sha2-compress` (a pure-Rust compress function of a v1 helper, rated SUS by the legitimacy check, no use in any source file) should be removed.

Rehash is the larger part. Three findings shape it. (1) Every metadata "node" and every file is a value in the chunk store built by the same `FileWriter` (`put_value`). So the engine has two primitives: a byte copy of a value under the target hasher (files, inode nodes, directory nodes, xattr nodes) and a key translation of the few value kinds that embed keys (manifest lists, six flat maps, the root record). There is no tree of metadata nodes with ID-dependent cuts; only the generic value manifest has them, and `FileWriter` rebuilds it. (2) The switch reuses the shape of every commit: one new segment that starts with the `RootUpdate` and carries every `SnapshotRecord`, then older segments are deleted. Rehash writes it as a temp file and renames it, and the two segment loaders keep the last record of each snapshot version. That needs no change in any caller. (3) The old GC entry `gc_report` cannot be reused as is: it takes the lock again (refused in the same process), opens a second `ChunkStore` on the same packs, and runs a segment "compaction" that leaves a copy of every segment file. Rehash needs a GC entry that takes the open `ChunkStore` and only does the chunk-store part.

**Primary recommendation:** Add `sha2 = { version = "0.11", default-features = false }`; build the rehash engine in the `metadata` crate (it needs the private `root_rec` layout) as "walk the roots in tree order, stream each blob, translate each key through an on-disk open-addressing hash table in `<store>/rehash-tmp/`"; switch with one `segment-<id>.seg` written as `.seg.tmp`, synced and renamed, with last-record-per-version in both loaders; then run a full GC on the already open `ChunkStore`.

## Project Constraints (from CLAUDE.md)

No `./CLAUDE.md` or `./.claude/CLAUDE.md` exists in the repository. The constraints come from CONTEXT.md and the user rules for this task:

- Safety gates stay green (D-07): `scripts/safety/unsafe-count.sh` counts the word `unsafe` in every `.rs` file under `crates`, `benchmarks` and `vendor/fuser`, comments included. A new file or comment with that word changes the count and needs a doc line. New counting-allocator tests would add such tokens; use structural bound checks instead.
- Tests are short; benchmarks only by hand. No local macOS mount. Linux FUSE only in OrbStack Docker, and only if a path cannot run in process.
- Writing rules: no home-directory paths in committed files; plain short English.
- Every cargo gate script uses `--locked`, so the new `Cargo.lock` must be committed with the dependency change.

## Architectural Responsibility Map

| Capability | Primary Tier | Secondary Tier | Rationale |
|------------|-------------|----------------|-----------|
| SHA-256 digest, tag, registry | `chunk-store` crate (`hash.rs`) | — | `ObjectHasher` and `HASHERS` live there; `zero_chunk` also iterates `hashers()` |
| Header flag, `--hash` handling | `slicefs-cli` (`header.rs`, `content.rs`) | — | `check_write_with` is the single place that writes the hash line for `seed` and `mount` |
| Snapshot dedup in loaders, switch segment | `metadata` crate (`segment/`) | `slicefs-cli` calls it | The loaders and `SegmentWriter` are there; callers keep their signatures |
| Rehash engine (file and metadata rebuild, disk map) | `metadata` crate | `chunk-store` (`FileReader`, `FileWriter`, `extsort`) | Needs `root_rec`, `MAP_ENTRY`, `RECORD` layouts that are private to `metadata` |
| Lock, header check, log check, switch order, GC, report, `--dry-run` | `slicefs-cli` (`rehash.rs`, `cli.rs`, `gc.rs`) | — | The store lock, header and GC entry are CLI-level |
| Free of old objects | `chunk-store` (`gc_begin_full`, `gc_finish_full`) | `metadata::gc::live_objects_exact` (mark) | Existing exact full GC |

## Standard Stack

### Core
| Library | Version | Purpose | Why Standard |
|---------|---------|---------|--------------|
| `sha2` (RustCrypto) | 0.11.2, `default-features = false` | SHA-256 digest with run-time CPU detection | The maintained RustCrypto crate; 32.8M downloads per week. 0.11.2 has the aarch64 SHA2 backend in the crate itself (0.10.9 gates it behind feature `asm`, which pulls the `sha2-asm` assembly crate). |
| `cpufeatures` | 0.3.1 (transitive of `sha2` 0.11) | Run-time detection of `sha`+`sse4.1` (x86/x86_64) and `sha2` (aarch64: `getauxval` on Linux, `sysctlbyname` on Apple) | Part of `sha2`; has a Miri shim that reports no features, so Miri uses the soft path |

Existing and unchanged: `blake3`, `fastcdc`, `zstd`, `crc32c`, `tempfile` (tests).

### Alternatives Considered
| Instead of | Could Use | Tradeoff |
|------------|-----------|----------|
| `sha2` 0.11.2 | `sha2` 0.10.9 | 0.10.9 needs `features = ["asm"]` (a C/asm crate and a build script) for ARM acceleration. 0.10 would reuse `cpufeatures` 0.2.17 that is already in `Cargo.lock`, but it adds `generic-array`, `digest` 0.10 and `typenum` anyway. Not recommended. |
| `sha2` | `ring`, `openssl` | A large native dependency for one hash; not needed. |
| keep `sha2-compress` | — | It is unused and has no hardware support (its README lists SHA-NI and ARM as a TODO). Remove it. |

**Installation (workspace `Cargo.toml`, `[workspace.dependencies]`):**
```toml
sha2 = { version = "0.11", default-features = false }   # replaces the line: sha2-compress = "0.7.1"
```
`crates/chunk-store/Cargo.toml` `[dependencies]`: `sha2.workspace = true`. Remove `sha2-compress = { workspace = true }` from `crates/slicefs-cli/Cargo.toml` (line 31). `default-features = false` drops `alloc` and `oid` (and the `const-oid` crate); a scratch build with it compiled and gave the golden vectors.

**Version verification:** `cargo info sha2` printed `version: 0.11.2` (latest) with `rust-version: 1.88`; `cargo info sha2@0.10` printed `0.10.9`. `[VERIFIED: cargo info]`

**Tree added to `Cargo.lock`** (none of these is in the lock now): `sha2` 0.11.2, `digest` 0.11.3, `crypto-common` 0.2.2, `block-buffer` 0.12.1, `hybrid-array` 0.4.15, `typenum` 1.20.1, `cpufeatures` 0.3.1 (a second copy next to 0.2.17), `cfg-if` 1.0.x (one copy exists). `sha2-compress`, `fixed-array` and `uints` leave the lock. `[VERIFIED: cargo tree in a scratch crate]`

**Toolchains checked with a scratch crate that builds `sha2` 0.11.2 with `--locked`:** Rust 1.100.0 (`RUST_STABLE` of `safety.yml`), nightly-2026-02-11 (the `NIGHTLY` pin of Miri and TSan), and the compiler bundled with Kani 0.68.0 (`cargo kani` built the crate and verified a trivial harness). Rust 1.85 fails (MSRV 1.88); no gate uses it. `[VERIFIED: builds run this session]`

### Dependency policy
No `deny.toml`, `cargo audit` or `cargo vet` step exists for the workspace (`deny.toml` exists only in `vendor/fuser`, which is excluded from the workspace). The CI rule is `--locked` everywhere (`scripts/safety/*.sh`). `[VERIFIED: find and grep of .github/workflows and scripts/safety]`

### Speed
On this machine (Intel Xeon W-3245; `sysctl machdep.cpu.leaf7_features` lists AVX2 and AVX512F and no `SHA`), the software path ran SHA-256 on 8 KiB inputs at about 247 MB/s, and BLAKE3 at about 4,000 MB/s (about 16 times faster). `[VERIFIED: scratch benchmark, 100,000 runs of 8 KiB, three runs agree]` Expected with SHA-NI or ARMv8 SHA2: about 1.5 to 2 GB/s on one core. `[ASSUMED]` This machine and OrbStack on it cannot show the accelerated path. See Validation Architecture for where it can be checked.

## Package Legitimacy Audit

| Package | Registry | Age | Downloads | Source Repo | Verdict | Disposition |
|---------|----------|-----|-----------|-------------|---------|-------------|
| sha2 | crates.io | first published 2016 | about 32.8M/wk | github.com/RustCrypto/hashes | OK | Approved |
| cpufeatures | crates.io | 2021 | about 38.9M/wk | github.com/RustCrypto/utils | OK | Approved (transitive) |
| digest | crates.io | — | — | RustCrypto | OK | Approved (transitive) |
| crypto-common | crates.io | — | about 34.8M/wk | github.com/RustCrypto/traits | OK | Approved (transitive) |
| block-buffer | crates.io | — | about 35.3M/wk | github.com/RustCrypto/utils | OK | Approved (transitive) |
| hybrid-array | crates.io | — | about 13.2M/wk | github.com/RustCrypto/hybrid-array | OK | Approved (transitive) |
| typenum | crates.io | — | about 26.3M/wk | github.com/paholg/typenum | OK | Approved (transitive) |
| cfg-if | crates.io | — | about 37.9M/wk | github.com/rust-lang/cfg-if | OK | Approved (transitive) |
| sha2-compress | crates.io | 2021 | 57/wk | github.com/natfoam/lib | SUS | REMOVED from the workspace (unused; see below) |

`[VERIFIED: gsd-tools query package-legitimacy check, crates ecosystem]`. `sha2`, `cpufeatures` and the RustCrypto names were also confirmed in `cargo info` and in the source of `sha2-0.11.2` (backend selection in `src/sha256.rs`). There is no postinstall concept for crates; `sha2` 0.11.2 has no build script (`build = false` in its manifest).

**Packages removed due to SLOP verdict:** none.
**Packages flagged SUS:** `sha2-compress`. It is not recommended, it is removed. It was added in Phase 02 only to satisfy workspace inheritance of a v1 path dependency that no longer exists (`.planning/milestones/v1.0-phases/02-metadata-engine/02-01-SUMMARY.md`), and `grep` finds no `sha2_compress` or `sha2-compress` in `crates/`, `benchmarks/` or `docs/` other than the line in `crates/slicefs-cli/Cargo.toml`. No `checkpoint:human-verify` is needed because nothing is installed that is flagged.

## Architecture Patterns

### System Architecture Diagram

```
slicefs rehash --to H [--dry-run] <store>
        |
        v
  require_store -> parse_hash(H) -> header::lock_offline (exclusive, held to the end)
        |                                  (refused: mounted, read-only mount, other command)
        v
  header::check (Want::Write)  --> unknown flag/format refused
        |
  scan_store_with_log(segments) --> log.is_empty() ? else refuse (mount and unmount once)
        |                          also: load roots = last RootUpdate + snapshots (last record per version)
        v
  [dry-run? --> open_read_only, walk + verify + count, print estimate, stop]
        |
  S2  target sha256 and no flag? write header with flag (atomic rename)         <-- before any SHA-256 object
        |
  S3  open ChunkStore(hasher = H, profile = header profile, gc_tmp_dir)
      create <store>/rehash-tmp/ (map file, refcount sort files)
      for each root (current, then snapshots by version):
          read root record (230 B) --> 6 keys
          stream inode_map, inode_data, dir_data, xattr_data  --> byte-copy each node value
          stream manifest_data  --> per manifest node: translate content keys, put_value
          refcounts             --> translate keys, external sort by new key, merge, put_value
          put_value(new root record)
          every key goes through  DiskMap.get(old) or rewrite + DiskMap.insert(old,new)
      cs.sync()                                                 (all new objects durable)
        |
  S4  write_switch_segment: segment-<max+1>.seg.tmp = RootUpdate(new) + SnapshotRecord x N
      sync file -> rename to .seg -> sync dir                   (ATOMIC POINT: new root visible)
      remove older segment files -> sync dir
        |
  S5  header: hash line = H (atomic rename)
        |
  S6  full GC on the open ChunkStore with the new roots:        gc_begin_full, live_objects_exact, gc_finish_full
        |
  S7  remove rehash-tmp, print summary (objects, bytes, kept, freed, disk usage before/after)
```

### Recommended Project Structure
```
crates/chunk-store/src/hash.rs            # + Sha256, HASHERS (all builds)
crates/chunk-store/src/lib.rs             # extsort made public (refcount sort)
crates/metadata/src/segment/mod.rs        # last-record-per-version in both loaders
crates/metadata/src/segment/switch.rs     # write_switch_segment (new)
crates/metadata/src/rehash/mod.rs         # engine: roots -> new roots, Stats, step hooks (new)
crates/metadata/src/rehash/map.rs         # DiskMap (open addressing, fixed RAM cache) (new)
crates/slicefs-cli/src/header.rs          # flag, KNOWN_FLAGS, parse pairing rule, marker_with
crates/slicefs-cli/src/content.rs         # check_write_with adds the flag; open_with_hasher; rehash-tmp cleanup
crates/slicefs-cli/src/gc.rs              # gc on an open ChunkStore (no lock, no second open)
crates/slicefs-cli/src/rehash.rs          # command: checks, order, dry-run, report (new)
crates/slicefs-cli/src/cli.rs, main.rs    # `Rehash { to, dry_run, store }`
```

### Pattern 1: SHA-256 hasher
**What:** One more `ObjectHasher`; `parse_hash`, `--hash`, `--to` and `zero_chunk()` pick it up from `hashers()`.
**Example** (compiled in a scratch crate; the digest is `[u8; 32]` through `.into()`):
```rust
use sha2::{Digest, Sha256 as Sha2};

/// SHA-256. Tag 0x12: the multihash code of sha2-256.
#[derive(Clone, Copy, Debug, Default)]
pub struct Sha256;

impl ObjectHasher for Sha256 {
    fn name(&self) -> &'static str { "sha256" }
    fn tag(&self) -> u8 { 0x12 }
    fn digest(&self, data: &[u8]) -> [u8; DIGEST_LEN] { Sha2::digest(data).into() }
}
// HASHERS: add `&Sha256,` after `&Blake3_256,` (no cfg: all builds).
```
The tag code `sha2-256 ... 0x12` and `blake3 ... 0x1e` are rows of the multicodec table. `[CITED: github.com/multiformats/multicodec table.csv]`

### Pattern 2: Header flag (Q2)
`parse` gets one pairing rule next to the existing `chunks`/`chunk-profile` pair; `marker_with` and `check_write_with` write the flag with the hash line in the same header write. Sketch:
```rust
pub const HASH_SHA256_FLAG: &str = "hash-sha256";
pub const HASH_SHA256_RELEASE: &str = "v2.2";            // placeholder, as CHUNK_PROFILE_RELEASE
pub const KNOWN_FLAGS: &[&str] = &[CHUNK_PROFILE_FLAG, HASH_SHA256_FLAG];
// parse(), after the chunks/flag pair check:
//   if a `hash sha256` line exists: the flag HASH_SHA256_FLAG must exist and be Incompat, else bad_at(j, "hash sha256 with no hash-sha256 flag")
// marker_with(): if hasher.name() == "sha256", push "incompat hash-sha256 <release>\n".
// check_write_with(): if the new hasher is sha256 and the flag is missing, push Line::Flag(..) before write_header (one atomic write).
```

### Pattern 3: Snapshot loader rule and switch segment (Q7)
```rust
// segment/mod.rs, end of load_store_from_segments and of scan_store_with_log:
fn last_per_version(all: Vec<SnapshotEntry>) -> Vec<SnapshotEntry> {
    let mut by_version = std::collections::BTreeMap::new();
    for s in all { by_version.insert(s.version, s); }   // file order: the later record wins
    by_version.into_values().collect()                  // ascending version
}
```
```rust
// segment/switch.rs
/// Write the segment that makes `root` and `snapshots` current; return its id.
pub fn write_switch_segment(dir: &Path, root: ObjectKey, snapshots: &[SnapshotEntry]) -> io::Result<u64> {
    let id = max_segment_id(dir) + 1;                              // same rule as mount::next_segment_id
    let fin = dir.join(format!("segment-{:06}.seg", id));
    let tmp = dir.join(format!("segment-{:06}.seg.tmp", id));
    let mut w = SegmentWriter::new(&tmp, id)?;                     // create + truncate: a stale .tmp is replaced
    w.write_entry(&SegmentEntry::RootUpdate { root })?;
    for s in snapshots { w.write_entry(&SegmentEntry::SnapshotRecord { version: s.version, root: s.root, created_at: s.created_at, name: s.name.clone() })?; }
    w.sync()?;                                                     // data durable
    std::fs::rename(&tmp, &fin)?;                                  // visible
    std::fs::File::open(dir)?.sync_all()?;                         // rename durable
    remove_segments_before(dir, id)?;                              // cleanup; a crash here is harmless
    std::fs::File::open(dir)?.sync_all()?;
    Ok(id)
}
```

### Pattern 4: One streaming primitive for files and byte-copy nodes (Q3)
```rust
const WINDOW: usize = 1 << 20;        // content::READ_BUF

fn rehash_value(cs: &ChunkStore, old: ObjectKey, target_tag: u8) -> Result<ObjectKey> {
    let id = to_id(&old);
    if old.tag() == target_tag && all_objects_have_tag(cs, id, target_tag)? { return Ok(old); }   // kept as is (D-14)
    let mut r = FileReader::open(cs, id)?;                       // every read is verified (zero chunks need no read)
    let mut w = FileWriter::new();                               // cs has the target hasher and the store profile
    let mut off = 0u64;
    while off < r.len() {
        let b = r.read(cs, off, WINDOW)?;
        if b.is_empty() { return Err(Error::Corrupt(format!("short read of {id:?}"))); }
        w.write(cs, &b)?;
        off += b.len() as u64;
    }
    Ok(to_key(w.finish(cs)?))                                    // the key of a fresh write of these bytes
}
// all_objects_have_tag: chunk_store::file::for_each_file_object (nodes are read, chunks are not).
```
The same function converts a file, an inode node, a directory node and an xattr node.

### Anti-Patterns to Avoid
- **Node-by-node translation of a file manifest:** new IDs move `is_cut`, so the key would differ from a fresh write (D-12 stands).
- **`file_objects` for a file:** it returns a `Vec` of every object (O(file size) memory). Use `for_each_file_object`.
- **Scanning node bytes for "keys with another tag":** directory lists hold BLAKE3 name keys (tag 0x1e) that must stay. Parse each node kind by its own format.
- **Treating "root tag is the target" as "kept" for key-carrying nodes:** a manifest list, a flat map or a root record of the target tag can embed old keys (a mixed store). Always translate these; the result equals the old key when nothing changed.
- **Calling `gc_report` or `GarbageCollector::run_gc` from rehash** (Q6).

## Don't Hand-Roll

| Problem | Don't Build | Use Instead | Why |
|---------|-------------|-------------|-----|
| SHA-256 | own implementation or own CPU detection | `sha2` 0.11.2 | hardware paths, constant tested code, no `unsafe` of ours |
| File and node tree under a new hash | a manifest builder or a copy of `is_cut` | `FileWriter` (`write`, `finish`) | by construction the fresh-write key; handles zero runs, mixed tags and every profile |
| Chunk cut points | any own chunker | `ChunkProfile::ranges` (inside `FileWriter`) | pure function of bytes and profile |
| External sort of the refcount rows | a sort in RAM | `chunk_store::extsort::Sorter<41>` (make it `pub`) | fixed RAM (4 MiB sort chunk, 32 inputs of 64 KiB), tested |
| Segment file write | own framing | `SegmentWriter::new` + `write_entry` + `sync` | CRC and sequence numbers |
| Header rewrite | own file replace | `header::write_header` (temp file, sync, rename, dir sync) | already crash-safe |
| Lock | own `flock` | `header::lock_offline` | the only store lock |
| Free of old objects | own sweep | `gc_begin_full`, `live_objects_exact`, `gc_finish_full` | exact mark, no holes, bounded RAM |

**Key insight:** every value the engine writes is a `FileWriter` stream, and the only new structure is the old-to-new map. Anything else that looks new (a metadata tree, a node cut rule) already exists in `chunk_store::file`.

## Research Answers

### Q1. SHA-256 tag, `sha2-compress`, crate, policy

- **Tag.** `sha2-256` is multihash code 0x12 and `blake3` is 0x1e `[CITED: multicodec table.csv]`. The existing tags are `0x1e` (`Blake3_256::tag`, `crates/chunk-store/src/hash.rs:42`: `0x1e`) and `0xf0` (`TestKeyedBlake3::tag`, `hash.rs:62`: `0xf0`). The tests use `0x77` (`Unknown` in `hash.rs:126`) and tag 0 (`ObjectKey::ZERO`, `crates/slicefs-traits/src/key.rs:20-21`: "All zero bytes: no object (tag 0 is no hash)"). `grep` over `crates/` and `benchmarks/` for `ObjectKey::new(`, `ObjectId::new(`, `tag() ==` and `0x12` finds `ObjectId::new(1, ...)` in index and mark tests, `ObjectId::new(0x1e, ...)` in `run.rs`, and one `[0x12; 4096]` that is file data in `write_log_replay_tests.rs:895`. A proptest in `inode_map.rs:262` uses any `u8` as a tag in a serialization round trip only (no hasher). No test or fixture uses 0x12 as a hash tag. The repo has no `.seg`, `.pack` or `FORMAT` fixture files (`find` outside `target`, `.git`, `.claude`). `[VERIFIED: Read of hash.rs, key.rs; grep]`
- **`sha2-compress`.** `Cargo.toml` line `sha2-compress = "0.7.1"` and `crates/slicefs-cli/Cargo.toml:31` `sha2-compress  = { workspace = true }`. The crate is "SHA2 compress function", pure Rust; its README lists "SHA256, SHA224, SHA512, ..." as done and "Intel SHA extensions" and "ARM cryptography extension" as a TODO to investigate. It has 57 weekly downloads (SUS). No source file uses it. It is the leftover of Phase 02. **Drop it.** `[VERIFIED: crate README read, cargo info, grep]`
- **Hardware crate.** Use `sha2` 0.11.2. In `sha2-0.11.2/src/sha256.rs` the default branch declares `cpufeatures::new!(shani_cpuid, "sha", "sse4.1")` on x86/x86_64 and `cpufeatures::new!(sha2_hwcap, "sha2")` on aarch64 and calls the intrinsics backend only when the check is true, else the soft code. `cpufeatures` 0.3.1 detects aarch64 `sha2` with `getauxval` (Linux) and `sysctlbyname` (Apple). The crate's own `unsafe` is inside the dependency and is not counted by `unsafe-count.sh` (it scans `crates`, `benchmarks`, `vendor/fuser` only). `[VERIFIED: source read]`
- **Is it in `Cargo.lock`?** No: `Cargo.lock` has `blake3` 1.8.3 with `cpufeatures` 0.2.17 and `cfg-if` 1.0.4 only; no `sha2`, `digest`, `generic-array` or `typenum`.
- **Golden vectors** (independent source: macOS `shasum -a 256`, equal to the crate output): `""` = `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`; `"abc"` = `ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad`; `"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"` = `248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1`; one million `a` = `cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0`. `[VERIFIED: shasum -a 256 and scratch crate agree]`
- **Where `--hash` is parsed.** `parse_hash` (`content.rs:96-100`) uses `hasher_by_name`, so `seed --hash`, `mount --hash` (both `value_parser = crate::content::parse_hash` in `cli.rs`) and a future `rehash --to` accept `sha256` as soon as it is in `HASHERS`. `zero_chunk()` (`lib.rs:3006-3009`) builds `hashers().iter().map(|h| h.id(&z))`, so the store gets the zero-chunk ID of SHA-256 for free (one hash of 4 to 1024 KiB).
- **Miri, Kani, TSan.** `scripts/safety/miri.sh` runs only the modules `pack_io::tests:: sync::tests:: lock_tail_tests:: run::tests:: index::tests::` of `chunk-store` and `wal:: inode::tests:: xattr::tests::` of `metadata`; the `hash::tests` module is not in the list. `cpufeatures` has a Miri shim (`src/miri.rs`: `__detect_target_features` is `false`). The Kani run compiles `slicefs-cli` as a lib; the scratch check above shows the new dependency builds under Kani's compiler. Still run `KANI_HARNESS=name_bytes_total scripts/safety/kani.sh` once after the dependency change (Wave 0 check).

### Q2. Header

- **Constants.** `crates/slicefs-cli/src/header.rs:38`: `pub const KNOWN_FLAGS: &[&str] = &[CHUNK_PROFILE_FLAG];` and `:32` `pub const CHUNK_PROFILE_FLAG: &str = "chunk-profile";`, `:35` `pub const CHUNK_PROFILE_RELEASE: &str = "v2.2";`. `FORMAT_NUMBER` is `7` (`:24`). The class words are `compat`, `ro_compat`, `incompat` (`Class::word`, `:73-79`).
- **How the pieces work.** `Line::Hash(String)`, `Line::Chunks(ChunkProfile)`, `Line::Flag(Flag)`, `Line::Other(String)` (`:108-119`). `parse` (`:329-393`) keeps each later line in order, refuses two hash lines, two chunks lines, a flag listed twice, and a flag line that is not exactly three words with a valid name and release. After the loop it checks the pair `chunks` line + `chunk-profile` flag and that the flag is `ro_compat` (`:379-391`). `decide` (`:444-470`) takes the flags whose name is not in `KNOWN_FLAGS`: an unknown `incompat` flag refuses (M5) for every `Want`; an unknown `ro_compat` flag gives read-only access (or refuses `Want::Write`); an unknown `compat` flag changes nothing.
- **Kani core.** The proved functions are `valid_name_bytes`, `valid_release_bytes`, `first_line_number` and `Class::from_word_bytes` (comment `:404-408`; harnesses `name_bytes_total`, `release_bytes_total`, `first_line_number_total`, `class_word_total` at `:1047-1096`). `parse` itself is not proved. The name `hash-sha256` is 11 bytes of `a-z`, `-`, `0-9`, no `--`, letters at both ends, so `valid_name_bytes` accepts it with no change. **The new flag changes none of the four proved functions and adds no harness**; `kani.sh` counts `#[kani::proof]` in `crates/slicefs-cli/src` (4 now) and still matches.
- **Smallest change.** (1) the two constants and `KNOWN_FLAGS`; (2) one pairing rule in `parse` (see Pattern 2); (3) `marker_with` appends `incompat hash-sha256 <release>` when the hasher is `sha256`; (4) `check_write_with` (`content.rs:135-157`) adds the flag line when the new hasher is `sha256` and the flag is missing, in the same `write_header` call that sets the hash line; (5) `docs/design/store-format.md` flag list ("Store format 7 has one flag" becomes two), the M9 reason list, and the stats example; (6) `stats.rs` needs no change (it prints any flag; `known` comes from `KNOWN_FLAGS`).
- **Where `--hash` is applied.** `seed.rs:65` `content::check_write_with(store_path, hash, profile)` (before `content::store_io`); `mount.rs:133` `crate::content::select_hash(store_path, hash)` (after `lock_write_mount`, before `store_io` at `:135`). For a new store the header is made by `header::check_with(store, Want::Write, hasher, profile)` → `create_with` → `marker_with(hasher, profile)` before `packs/` exists (`header.rs:486-499`, `:618-637`). So the flag is on disk before the first SHA-256 object in all three paths. Rehash writes it itself at step S2.
- **Rule for the stray cases.** After `mount --hash blake3-256` on a SHA-256 store the hash line returns to `blake3-256` and the flag stays (by D-11). The flag without the line is valid. A hand-edited header with `hash sha256` and no flag is refused at parse (M9 family).
- **Remove the flag after GC (deferred in CONTEXT).** Not exact enough: it would need proof that the index holds no object of tag 0x12 (also `dead_left`, touched packs, a crash between GC and the header write). Keep it deferred.

### Q3. Conversion of a file

- **Cut points depend on bytes and profile only.** `ChunkProfile::ranges` is `fastcdc::v2020::FastCDC::new(data, self.min, self.avg, self.max)` (`lib.rs:181-185`). `FileWriter::cut` calls `store.profile().ranges(&self.buf)` and keeps the last chunk until `finish` (`file.rs:156-167`); the buffer is drained to a chunk end, so every chunk starts at a cut. The doc says "the key of a file does not depend on the sizes of the writes that made it" (`file.rs:13-15`), and `put_reader_key_equals_put_all_key` (`content.rs:464-482`) tests read sizes of 1 MiB and odd tails.
- **Manifest cuts depend on IDs.** `file.rs:44-47`: `fn is_cut(id: &ObjectId) -> bool { let w = u32::from_le_bytes(id.digest()[DIGEST_LEN - 4..].try_into().unwrap()); w & (NODE_CUT_ONE_IN - 1) == 0 }` with `LEAF_ENTRIES: usize = 4096`, `NODE_MIN: usize = 64`, `NODE_CUT_ONE_IN: u32 = 512` (`:33-40`). The tag is ignored, the digest is not. Under a new hash the same chunk list gets other node cuts, so only the real builder gives the fresh-write key. D-12 holds.
- **"Reuse old boundaries, rehash each chunk, rebuild the manifest" path.** It would use `FileWriter::push_entry` (private, `file.rs:185-189`; `splice` uses it). It gives the fresh-write key only if the old chunk list is itself the FastCDC cut of the bytes. `splice` keeps that property (`file.rs:601-612`: "The key is the key of a `FileWriter` that writes all the new bytes"; tests `a_splice_gives_the_same_key_as_a_full_write` at `:939`, `a_splice_over_zeros_gives_the_key_of_a_full_write_for_each_profile` at `:1082`), but a tree that comes from a peer need not (`file.rs:453-455`: "A tree that breaks this rule, from a peer, still gives the right bytes; only its key may differ from a full write"). The path saves only the FastCDC time per byte; the read, decompress, hash, compress and write costs stay. **Recommendation: do not use it.** Stream the bytes through `FileWriter` (Pattern 4): correct by construction for peers, mixed tags and every profile, and it needs no new API in `file.rs`.
- **Zero runs and sparse files.** `FileReader::read` returns zeros from the store's zero buffer for a chunk whose ID is in `zero_chunk()` and whose length is the profile max, with no read (`file.rs:297-304`); `zids` holds one ID per known hasher, so an old BLAKE3 zero chunk is recognized. `FileWriter::write` then cuts and hashes those zeros like any bytes (a zero run of the profile max is one chunk), which gives the same chunk ID as a fresh write; `write_zeros` is also only `write` (`file.rs:144-152`). `splice` does the same: "A full write puts the zero chunk: put it once" (`file.rs:690-691`: `store.put(zeros)`). So the key equals the fresh-write key; the cost is O(zero bytes) hashing (SHA-256 at 250 MB/s on a CPU without SHA extensions makes a 10 GiB sparse file take about 40 s). A hash-free zero path in the engine is an optimization for later, not needed for correctness.
- **Writer buffer.** `FileWriter::write` cuts when `buf.len() >= 2 * store.profile().max()` (`file.rs:137`), so `buf` stays below 2 x max (at most 2 MiB for the 1 MiB profile limit, `ChunkProfile::MAX_LIMIT = 1 << 20`).
- **Mixed-tag manifests exist.** `splice` copies old level-0 entries and whole old nodes with `push_entry` (`file.rs:585` and `:594`), which keeps their old tags, while the re-cut chunks (`:725` `store.put`) and the new nodes get the write hasher. After `mount --hash X` and an edit of an old file, the tree has nodes and chunks of both tags at any level. Existing tests do not build one (`hash_change_tests.rs` writes whole new files), so HASH-06 must: write a file under BLAKE3, reopen with the second hash, `test_write` a middle range, release.
- **Bounded streaming.** `FileReader` holds the root node (at most 4096 entries of 41 bytes), a cache of 16 nodes (`NODE_CACHE: usize = 16`) and the last chunk; `read(offset, size)` allocates `size` bytes, so use a window (`content::READ_BUF` is `1 << 20`). `file_objects` returns every object in a `Vec` (O(file size): 33 bytes per chunk); `for_each_file_object` keeps "one path of nodes and the pending siblings" (`file.rs:329-346`). The engine uses only the second.

### Q4. Metadata inventory (D-13)

Every node is a value built by `put_value` = `FileWriter` over the serialized bytes (`store_io.rs:61-65`), and read by `get_value` = `FileReader` (`:69-73`). All reads verify.

| Node (file) | Wire format (read this session) | Keys inside | Translation |
|-------------|---------------------------------|-------------|-------------|
| Root record (`store.rs:36-52`) | 230 bytes: `INODE_MAP` at 0, `ROOT_DIR_INO` (u64), `NEXT_INO` (u64), then keys `INODE_DATA`, `DIR_DATA`, `MANIFEST_DATA`, `XATTR_DATA`, `REFCOUNTS`, then `LOGICAL_BYTES` (u64), `INODE_COUNT` (u64). Offsets are `INODE_MAP = 0`, `ROOT_DIR_INO = INODE_MAP + D`, `NEXT_INO = ROOT_DIR_INO + 8`, `INODE_DATA = NEXT_INO + 8`, `DIR_DATA = INODE_DATA + D`, `MANIFEST_DATA = DIR_DATA + D`, `XATTR_DATA = MANIFEST_DATA + D`, `REFCOUNTS = XATTR_DATA + D`, `LOGICAL_BYTES = REFCOUNTS + D`, `INODE_COUNT = LOGICAL_BYTES + 8`, with `D = KEY_BYTES` (33) | 6 keys, to blobs below | replace the 6 keys, copy the other fields, `put_value` |
| Inode map (`inode_map.rs:88-96`) | `const RECORD: usize = 8 + KEY_BYTES;` records `ino u64 LE` + key, sorted by `ino`, no count | 1 key per record: the inode node | stream, translate each key, same order |
| `inode_data`, `dir_data`, `manifest_data`, `xattr_data` (`store.rs:1625-1676`) | `[count u64 LE]` then records `ino u64 LE` + key; `const MAP_ENTRY: usize = 8 + KEY_BYTES;` sorted by `ino` (BTreeMap order) | 1 key per record: an inode, directory, manifest or xattr node | stream, translate, same order; write the same `count` |
| Refcounts (`store.rs:1757-1801`) | `[count u64 LE]` then records key + `u64 LE`, sorted by key bytes (`ObjectKey` derives `Ord`, `key.rs:16`) | 1 key per record: a file content key | translate, **re-sort by new key**, merge equal new keys by saturating sum, write the new count (see below) |
| Inode node (`inode.rs:30-72`) | 56 or 72 bytes of integers | none | byte copy (Pattern 4) |
| Directory list (`directory.rs:15-26`, `:54-67`) | `[count u32]`, then key (33) + `ino u64` + `name_len u32` + name | the key is `name_key(name)` = BLAKE3 of the name: `crate::store_io::to_key(chunk_store::Blake3_256.id(name.as_bytes()))` (`directory.rs:254-257`: "not a content address ... always BLAKE3-256, never the store's write hash") | byte copy; the 0x1e keys stay |
| Xattr node (`xattr.rs:24-34`) | list of name and value with u32 lengths | none | byte copy |
| Manifest list (`manifest.rs:22-28`) | `blocks.len() * 33` bytes: the content keys of the inode (a file has one; a symlink stores its target as content, `filesystem.rs:5051-5058`) | content keys | read, translate each key, `put_value` |
| File content (`chunk_store::file`) | `SFM1` nodes of (ID, length) entries, chunks | IDs inside the tree | `rehash_value` (Pattern 4) |
| Segment records (`segment/mod.rs`) | `RootUpdate` payload = one key; `SnapshotRecord` = version (8) + root key + `created_at` (8) + name | root keys | new switch segment |

- **ID-dependent boundaries.** Only the manifest trees of values (`is_cut`) and the FastCDC chunking of each value (a byte function; the embedded keys are part of those bytes, so a translated node is chunked anew by `put_value`). The six flat maps and all lists are flat blobs, not trees. No node type needs a builder other than `put_value`. Prolly nodes are Phase 24.
- **Order of translation.** Content values first (they are leaves), then the node kinds; per root: byte-copy nodes and manifests, then the 4 maps and the inode map, then refcounts, then the root record. The reference graph has a fixed depth (`referenced` reads the root record, the maps, and for each manifest its keys: `store.rs:1580-1603`).
- **Refcounts policy.** The refcount keys are content keys (`increment_refcount(&content_digest)`, `filesystem.rs:5060`); the table only feeds `stats` and `scrub` (project note: remove #165 in the next milestone). Recommended: translate each key through the same content lookup (a hit in the map costs nothing); if the object of a key that no manifest of that root uses is missing (stale count), drop the entry and count it in the report; any other error stops the run. A missing object that a manifest uses is an error (D-15). Because two old keys can map to one new key (the same bytes written under two hashes), merge their counts by saturating sum (`u64::MAX` is "immortal", `store.rs:509-517`).
- **`referenced`/`collect_content_keys`/`live_objects`.** `referenced` (`store.rs:1580-1603`, `pub(crate)`) loads the three maps of a root into `BTreeMap`s, returns the node keys (the root, the inode map, the six map blobs, every value of `inode_data`, `dir_data`, `xattr_data`, and every manifest key) and the content keys (the entries of all manifests). `collect_content_keys` (`gc/mod.rs:26-32`) returns a `Vec` of content keys of all roots. Both hold O(inodes) in RAM, which the GC mark already accepts. **Do not reuse them for the rehash walk**; use them only in tests and in the GC. The walk of the engine streams the blobs.

### Q5. Old-to-new map (research flag 1)

- **What to key it by.** The 33-byte old key; the value is the 33-byte new key. One table for all kinds (content, inode, directory, xattr and manifest nodes, and root records), so that a root shared by the current state and a snapshot, an unchanged node, and a file used by many inodes or many snapshots are done once. Chunks are not keyed: they dedup by themselves under the target hash.
- **Entry count.** Each root has its own flat maps, so the work per root is the number of records of its maps (roots x entries lookups); the table holds the distinct keys over all roots. Order of magnitude: the Boost trees have about 78,000 files per version (`.planning/ROADMAP.md`, Phase 24 criteria), so one version has about 250,000 distinct keys (inode, directory, manifest, xattr nodes and content), and ten mostly equal versions about 0.4 to 0.8 million. At 66 bytes per entry and load factor 0.5, that is about 50 to 100 MB of disk. `[ASSUMED]` (derived from the 78,000 files figure, not measured)
- **Structure (recommended): an on-disk open-addressing hash table of fixed 66-byte slots.**
  - File `<store>/rehash-tmp/map`; slot = old key (33) + new key (33); an empty slot has tag byte 0 (a real key never has tag 0: `key.rs:20-21`). Index = first 8 digest bytes `& (slots - 1)` (digests are uniform); linear probing; reads and writes with `std::os::unix::fs::FileExt` (`read_exact_at`, `write_all_at`), no `mmap`, no `unsafe`.
  - Start with 2^16 slots (4.3 MiB). At load 0.5 write a table of twice the size by one sequential scan and rename it over the old one. Nothing is durable: the directory is removed at the start of every run, so a crash never leaves a trusted map.
  - RAM: a direct-mapped cache of a fixed number of slots (suggest 2^14 slots, about 1.1 MB) in front of the file; the OS page cache is outside the process RAM. A cache entry is a copy of a slot, so the cache can never be wrong.
  - Correctness does not depend on the map (a miss means a rewrite with the same result), but a wrong hit would give a wrong key, so only the engine inserts, after a successful rewrite.
- **Why not sorted runs (`extsort`).** It can work in principle: a discovery pass over all roots pushes the old keys of each kind into sorted temp files, then content, byte-copy nodes, manifests, flat maps and root records are done in key order with binary-search lookups (`SortedFile::contains` reads one record per step). The costs: two passes over every root; old objects are read and new objects are written in the order of their hash, which breaks the write-order locality of the store (files of one directory sit near each other in the packs today); extsort is `pub(crate)` (`lib.rs:28` `mod extsort;`) and `contains` compares whole records, so a key-prefix lookup is also needed. The hash table walks the roots in tree (inode) order, which keeps the layout, and needs one pass.
- **Where extsort is still needed.** The refcount rows need a sort by the new key (the output order of the map changes with the hash). Make `extsort` public (`Sorter`, `SortedFile::reader`, `SortCfg`) and use `Sorter<41>`: rows `new key (33) + count (8, big endian)`. It dedups only identical rows, so equal keys with other counts are adjacent after the sort and the merge is one streaming pass. RAM: `SORT_CHUNK_BYTES = 4 << 20` and `MERGE_FAN_IN = 32` inputs of `READ_BUF = 64 << 10`.
- **Location and cleanup.** `<store>/rehash-tmp/` as D-16 says. The `gc-tmp` cleanup is in `ChunkStore::open_with`: `if let (Some(d), false) = (&opts.gc_tmp_dir, opts.read_only) { match std::fs::remove_dir_all(d) ... }` (`lib.rs:1448-1454`), driven by `Options::gc_tmp_dir` and `content::GC_TMP_DIR = "gc-tmp"` (`content.rs:161`). There is no such code for another directory, so: remove `rehash-tmp` in `content::open` (the writable open; add one `remove_dir_all` line next to `GC_TMP_DIR`), at the start of rehash (after the lock, before the map is made) and at the end. The rehash opens the chunk store first, so its own `open` removes a stale directory before the new one is made. Do **not** put the map under `gc-tmp`: a second `ChunkStore` open in the same process (what `gc_report` does) would remove it.
- **Dry-run.** No temp directory (D-22): use the RAM cache alone (a miss re-walks the object); the report says "estimate".
- **Memory and disk statement for the docs.** RAM is the fixed cache, the window buffers, one chunk and the writer buffers (below), not a function of the object count. Disk: about 132 bytes per distinct key at load 0.5, plus the refcount sort files (about 41 bytes per content key per root, twice during a merge), plus up to the live data size again in `packs/` until the GC runs (new objects are written beside the old; no dedup across hashes).

### Q6. GC interaction (research flag 2)

- **Lock.** `try_store_lock` opens the store directory (`std::fs::File::open(store)`) and calls `try_lock` (`header.rs:547-559`), i.e. a `flock` on that open file description. A second `File::open(store)` is a second description, so `lock_offline` in the same process is refused after the 250 ms retry (`LOCK_RETRY`). The existing test `a_second_writable_load_is_refused_while_the_first_runs` asserts `slicefs_cli::header::lock_offline(store_dir.path()).is_err()` while a load holds the lock (`mount_lifecycle_tests.rs:675`). So `gc_report` (which calls `lock_offline` at `gc.rs:104`) cannot run inside rehash.
- **What `gc_report` does that rehash must avoid** (`gc.rs:90-165`): (1) `lock_offline` (above); (2) `crate::content::store_io(store_path)` at `:123` opens a second `ChunkStore` on the same `packs/` directory and its `open_with` removes `gc-tmp`; (3) it branches on `mount.lock` and asks a live mount (`:96-100`); (4) it ends with `GarbageCollector::new(segs_dir).run_gc(...)` (`:151-152`) which copies every segment (below); (5) it takes its roots from `load_store_from_segments` (all snapshot records).
- **GC entry for rehash.** Split `gc_report` so that the middle part (lines `:126-149`) is `fn gc_on_open_store(cs: &Arc<ChunkStore>, roots: &[ObjectKey], full: bool) -> Result<GcStats, ...>`: `gc_begin_full`, `live_objects_exact`, `gc_finish_full` (on a mark error `gc_abort`). `gc_report` calls it after its own lock and open; rehash calls it with the `ChunkStore` it already has, with the new current root and the new snapshot roots, and does **not** call `GarbageCollector`.
- **Segment "compaction" copies, not compacts.** `GarbageCollector::run_gc_inner` calls `compact_segment(&seg_path, &self.segments_dir, seg_id + 100_000)` for each `.seg` file (`gc/mod.rs:247-257`), and `compact_segment` keeps every record, writes `segment-{:03}.seg` of the new id and "does NOT delete" the source (`compaction.rs:27-39`). Observed: after `slicefs seed st src` and `slicefs gc st` the directory has `segment-000001.seg` and `segment-100001.seg`. `[VERIFIED: ran the binary in a scratch directory]` So after any offline GC the loaders already see every record twice, including same-version snapshot records. Consequences: the last-record-per-version rule must be by file order, and tests must not assert "one segment file" or "one record per version" after a GC. A copy of an old segment has a higher id than a switch segment only after a second fault (a crash between the rename and the removal of the old segments, then a GC that stops after copying an old segment but before copying the switch segment). The rehash removes all lower ids right after the rename, which closes the window in the normal case. This behavior is outside Phase 23; report it to the owner (Open Question 4).
- **Stale `mount.lock` while the lock is free.** A writable mount holds the exclusive store lock for its life (#201) and removes `mount.lock` on a clean end (`mount_lifecycle_tests.rs:678`: "mount.lock is removed when the first load ends"), so a `mount.lock` with a free lock is a crashed mount. `snapshot create` and `switch` refuse in that case (`snapshot.rs:49-55` and `:153-159`: "Store is mounted (mount.lock found at ...)"). Default for rehash: also refuse with a message that says to mount and unmount once (the same advice as for log records). It costs one mount cycle after a crash and covers file systems with no `flock` (`lock_result` returns a no-op lock on `Unsupported`, `header.rs:565`). Reversible; the planner may drop it.
- **Full GC cost and memory.** `gc_finish_full` (`lib.rs:2137-2160`): the exact mark and the touched IDs are sorted in temp files (chunks of `Options::gc_sort_chunk_ids`, merges of `gc_merge_fan_in` inputs), pass 1 joins the index in ID order with the sorted live file, each pack with a dead record is compacted or deleted, never punched. RAM for the sort and the join is a few MiB and does not grow with the object count. The mark walk is different: `mark()` (`gc/mod.rs:103-143`) collects the node keys and content keys of **all roots** into one `Vec` (`values`), sorts and dedups it, and `referenced` loads the maps of each root into `BTreeMap`s. So the GC RAM grows with the number of metadata nodes and files (33 bytes each), not with the number of chunks. State exactly that in the docs; changing `mark` is outside this phase. After a rehash nearly every old object is dead, so most old packs are deleted whole (cheap); a pack that holds old and new objects (the active pack at the start of the run) is compacted.
- **No false "touched" survivors.** `gc_begin_full` starts the touched set after the rewrite; the mark reads only new objects (`mark` opens the new roots), so no old object is touched and `dead_left` should be `Some(0)`. Test it.
- **Bytes freed.** Use `GcStats.removed_bytes` ("Stored bytes of the removed objects", `lib.rs:893`) and `removed_objects`; also print `usage_line(before, after)` (`gc.rs:51-56`). Do not report the disk delta as "freed": the new objects are written first, so a tiny store can even grow (observed above: 308,824 to 312,737 bytes after an ordinary GC). Bytes rewritten = growth of `cs.stats().stored_bytes` and `.objects` (`Stats`, `lib.rs:329-338`) over the rewrite phase.
- **Online GC and superseded snapshot records.** The mount builds `snapshots_by_version` with `set_snapshots` (`store.rs:1233-1241`: `snapshots.iter().map(|s| (s.version, s.clone())).collect()`, so a later record wins), and the online GC marks `snapshot_roots()` from that map (`store.rs:1317-1325`, `snapshot_roots`). A running mount never sees a superseded record. The Vec consumers of the loaders are the risk: `gc.rs:110-119` (every snapshot record becomes a GC root), `scrub.rs:91-105` (verifies every record), `snapshot.rs:63, 108, 167` (`list` prints and counts every record), `stats.rs:265`, `mount.rs:124` and `:181`. Rehash makes all of them correct with one change inside the loaders (Q7). Without it, a GC after the switch keeps the old snapshot trees alive and frees nothing: the HASH-06 tests must assert that no object of another tag is left after the GC, not only that the run succeeds.

### Q7. Snapshot supersession and the switch (research flag 3)

- **Loaders of roots and snapshots** (all call `load_store_from_segments` or `scan_store_with_log` in `crates/metadata/src/segment/mod.rs`): `mount.rs:124`, `:181` (read-only mount), `snapshot.rs:63`, `:108`, `:167`, `gc.rs:110`, `scrub.rs:91`, `stats.rs:265`, `oplog.rs:218` (log replay, uses only the log), and many tests. Compaction: `compact_segment` is called only by `GarbageCollector` and tests (`crates/metadata/tests/gc_tests.rs`). The in-memory path is `set_snapshots`/`snapshot_roots`/`rotate_wal`.
- **Shape of every commit today.** `finish_inner` builds `seed = [RootUpdate { root }] + one Snapshot entry per snapshot` and calls `wal.install_root(cut, &seed)` or `wal.rotate(&seed)` (`store.rs:1191-1211`); `write_root_segment` and `rotate_segment` write that segment, sync it and the directory, delete every older segment, and sync the directory again (`wal/mod.rs:347-402`). The rehash switch is the same operation, so (a) is the existing shape. `snapshot switch` already ends with `commit_root` → `rotate_wal` (`store.rs:1335-1341`).
- **Recommendation: option (a), written as a temp file and renamed.** `rotate_segment` and `write_root_segment` create the segment at its final name, so a crash in the middle can leave a new segment with the `RootUpdate` and only some snapshots (new root, mixed snapshots: still mountable, but not all-or-nothing). Write `segment-<id>.seg.tmp`, sync, `rename`, sync the directory: one atomic step. A name that ends in `.seg.tmp` fails every filter: `load_store_from_segments` (`name.ends_with(".seg")`, `segment/mod.rs:72`), `segment_paths` (`:329`), `mount::next_segment_id` (`mount.rs:207`) and `GarbageCollector` (`extension() == Some("seg")`, `gc/mod.rs:244`). `compact_segment` already uses `.tmp` + rename (`compaction.rs:46-47`, `:84`).
- **Segment id and names.** Files are `segment-{:06}.seg` (`wal/mod.rs:392`), id = max existing id + 1 (`next_segment_id`, `mount.rs:199-220`; also counts files shorter than the 16-byte header, which the loaders skip). The record format is version 3 (`SEGMENT_VERSION: u32 = 3`); `RootUpdate = 0x02`, `SnapshotRecord = 0x03` (`RecordType`). No format change.
- **Loader rule.** Collect snapshots in file order and keep the last record of each `version`: a `BTreeMap<u64, SnapshotEntry>` insert, then `into_values()` (ascending version). Put it at the end of both loaders (`segment/mod.rs:111` and `:204`), so no caller changes (the return types stay `Vec<SnapshotEntry>`). It also fixes the duplicates that offline GC copies already create and that a crash between sync and removal creates in every commit today.
- **What the mount does with the switch segment.** `scan_store_with_log`: the segment's first record is a `RootUpdate`, not a `Cut`, so `broken = false` (`:151-156`); the `RootUpdate` sets `last_root`, `root_seg`, clears `spans` and `ops` (`:158-168`); later segments continue the log. The mount then makes its own WAL segment `next_segment_id` = switch id + 1 (`mount.rs:199`) with no `Cut`. `snapshot create` and `snapshot switch` already do the same after an offline commit, so appending a fresh segment offline is safe for the next mount's write log.
- **Crash states** (all with all new objects durable first: `cs.sync()` returns before the temp file is made):

| Step | Crash leaves | Mount sees |
|------|--------------|------------|
| writing `segment-N.seg.tmp` | a partial temp file, ignored by every filter | old root, old snapshots |
| after temp sync, before rename | a complete temp file | old |
| rename done, directory not yet synced | the file system decides: no `segment-N.seg`, or a complete one | old, or new (both legal: objects of both roots are durable) |
| after the directory sync | `segment-N.seg` plus all older segments | new root; both records of each version exist, the later one wins (new) |
| while removing older segments | `segment-N.seg` plus some older ones | new (the new segment holds every snapshot, so no snapshot is lost) |
| all older removed | `segment-N.seg` only | new, one record per version |
| header hash line not yet rewritten | new root, header line of the old hash | new; legal mixed store; rerun fixes the line |
| GC not done or interrupted | new root, old objects still stored | new; `gc_finish_full` is crash-safe by its own design; rerun runs the GC again |

  A stale `segment-N.seg.tmp` of a crashed run is removed (or truncated by `SegmentWriter::new`, which uses `create + truncate`) by the next run.
- **Option (b)** (rewrite all segments atomically) needs more than one file to be replaced at once, which POSIX does not give; (a) gives one rename. Not recommended.
- **Rule for later:** keep the new segment as the only holder of the switch; do not write Ops into it.

### Q8. Write-log check

`scan_store_with_log(&segments_dir)` (`segment/mod.rs:130-205`) reads every segment record by record (CRC checked), keeps only `LogSpan { path, pos, seq, end_seq }` per segment (cleared at each `RootUpdate`), a `counts` list of one `(id, records)` per segment, and counts `ops`; it does not keep the records. `WriteLog::is_empty()` is `self.ops == 0` (`:230-232`). Memory is O(number of segment files + snapshot records). It cannot stop at the first record after the last root, because it does not know a root is the last until it has read all segments; its time is the size of the segment files (small, since each commit deletes older segments). So D-17 is met by `scan_store_with_log` plus `log.is_empty()`; the words "stop at the first record" in D-17 should be dropped. Ops after a broken chain (`Cut` that does not match) are not counted and are not replayed by a mount either (`:174-186`), so they do not block rehash; the switch deletes those segments. A segment file with a bad 16-byte header makes `SegmentReader::open` fail (`invalid segment header`); refuse with that error.

### Q9. Verified reads

`ChunkStore::read` checks the record CRC, the stored ID and the hash of the decoded bytes with the hasher of the ID's tag, and returns `Missing`, `Corrupt`, `UnknownHash` or `Io` (`lib.rs:2885-2959`; `read_record` ends with `if hasher.id(&data) != *id { return Err(damaged()); }`). **`get` is `self.read(id).ok()` (`lib.rs:2881-2883`): it verifies too, it only hides the cause.** The CONTEXT sentence "never `get`" is right for a different reason: D-15 wants the error kind, and `get` returns `None` for a missing, damaged or unknown-hash object alike. `FileReader` reads manifest nodes through `load_node` → `store.read` and chunks through `store.read(&id)` (`file.rs:100-103`, `:306`), so it verifies everything it reads. The one exception is a zero chunk (ID in `zero_chunk()` and length = profile max): it is served from the store's zero buffer with no read (`file.rs:297-304`), which is exact because the bytes are known. `get_value` and `NodeReader` use `FileReader`. No unverified read API exists (`grep unverified` finds none). An unknown tag gives `Error::UnknownHash` (`lib.rs:2900`), which stops the run before the switch.

### Q10. Crash testing

- Patterns that exist: `CommitFault` and `DictMetadataStore::test_fail_next_commit_at` (`store.rs:261-275`, `:1026-1035`), used by `write_log_replay_tests.rs:958-962`; `MemIo` with `crash()`, `set_fail_syncs`, `fail_sync_after`, `flip` (`mem.rs`), used by `crates/metadata/tests/node_crash_tests.rs`; `chunk-store/tests/index_crash_tests.rs`.
- **Recommended hook for rehash:** an enum `Step { Checked, FlagWritten, Rewritten, Synced, SegmentTmpWritten, Renamed, OldSegmentsRemoved, HeaderWritten, GcDone }` and a `stop_after: Option<Step>` field of the run options (a `#[doc(hidden)]` pub field, like `CommitFault`). The run returns `Err(Injected(step))` right after the step, dropping the lock and the `ChunkStore` without cleanup. Add `stop_after_values: Option<u64>` to stop inside the rewrite. The test then checks the loaded root (old or new), mounts it (`load_from_root`), reads the files, runs `scrub`, and reruns without a hook to completion.
- **Torn states by file edits** in the test: truncate the new segment, leave a `.seg.tmp`, delete a subset of the older segments, keep the old header line.
- **Durability order test at engine level** with `MemIo`: run the engine and `cs.sync()`, call `mem.crash()` (drops unsynced bytes), reopen, `load_from_root(new_root)` must work. This checks that every new object is synced before the switch.

### Q11. Dry-run writes nothing

`content::open_read_only` (`content.rs:199-205`) runs `header::check(store, Want::Read)` and opens `ChunkStore` with `Options { read_only: true, ... }` and no `gc_tmp_dir`. `ChunkStore::open_with` removes `gc-tmp` only when `!opts.read_only` (`lib.rs:1448`), `FileIo::prepare(true)` makes nothing (`pack_io.rs:402-405`: "With `read_only`, make nothing and return whether the pack directory exists"), and `Options::read_only` is documented as "no cut of the active pack, no index writes, and `put` fails". So dry-run on this path writes no file. Two traps: (1) `header::check` with `Want::Write` on a store that has no header and an empty `segments/` would create a header (`check_with`, `header.rs:490-497`): for the dry-run, check with `Want::Read` and apply `decide(.., Want::Write)` to the parsed header so the refusals match a real run; (2) the dry-run must not open a map file or `rehash-tmp`.

### Q12. Test infrastructure for HASH-06

- **Seed in process:** `slicefs_cli::seed::run_seed_with(store, source_dir, hash, profile)` (`seed.rs:46`) from a temp source tree; sparse files by `File::set_len` in the source tree (read as zeros); large profile `ChunkProfile::new(16384, 65536, 262144)` as in `chunk_profile_tests.rs:32-34`.
- **Files, splice, snapshots, second hash:** the `hash_change_tests.rs` pattern: `open_fs(store)` (chunk store with the recorded hash, metadata from the last root), `test_create`, `test_write(fh, offset, bytes)`, `test_release`, `test_mkdir_full`, `test_setattr_size`, `content::select_hash(store, Some("test-keyed-blake3"))` to change the hash, then a partial `test_write` for a spliced mixed-tag file; `slicefs_cli::snapshot::run_snapshot_create(store, Some(name))` (takes `lock_offline`, so no lock may be held by the test) for snapshots. `TestKeyedBlake3` is registered by the `test-hasher` feature, on in the dev-dependencies of `slicefs-cli` and `metadata` (`name = "test-keyed-blake3"`, tag `0xf0`). A store with live objects of three tags (blake3, keyed, sha256) is possible.
- **Compare trees:** `tests/common/mod.rs` has `dump(fs)` (path → inode fields, xattrs, a content fingerprint read in 64 KiB pieces) and `reload_and_replay`. Load each snapshot root with `DictMetadataStore::load_from_root(io, &snap.root)` and `dump` it before and after.
- **Scrub in process:** `slicefs_cli::scrub::run_scrub(store, json)` returns `Err` on any error (`scrub.rs:57`, report at `:154-167`); it uses `load_store_from_segments`, so it also checks the loader rule.
- **Key equality with a fresh write:** for each file, read the bytes, then `content::put_all(&cs_target, &bytes)` must equal the new content key (a dedup hit).
- **One tag on every live object:** `metadata::store::referenced_roots` and `content_keys` are `pub` (`store.rs:1567`, `:1576`); `file_objects` over each key; assert the tag. Also assert that no object of another tag remains in the index after the GC (for example `cs.contains(&old_id)` is false for the old roots).
- **Canonical form check (to try in Wave 0):** `load_from_root(new_root)` then `commit()` with no change returns `new_root` if the engine wrote the canonical bytes; if it holds it is a cheap strong test of the flat maps and of the refcount sort. `[ASSUMED]`
- **Short:** `hash_change_tests` ran in 1.55 s here; keep the data below a few MB; use `ChunkProfile` large only on a small file.

## Memory bound (HASH-05)

| Part | RAM | Basis |
|------|-----|-------|
| One value being copied | read window (1 MiB) + reader (root node up to 4096 x 41 B, 16 cached nodes, one chunk of at most the profile max) + writer (`buf` < 2 x profile max, entries of one node per level) | `file.rs` (read) |
| A flat map blob | a window of records, not the map | streaming (design) |
| Old-to-new map | fixed cache (about 1 MB) | design |
| Refcount sort | 4 MiB sort chunk, 32 x 64 KiB inputs | `extsort.rs:22-28` |
| Chunk store (index memtable, caches) | as for any writable open | existing |
| Full GC | sort and join in a few MiB; the mark `Vec` of all node and content keys of all roots (33 B each) | `gc/mod.rs:103-143` (existing bound) |
| Snapshot list | one entry per snapshot record | existing |

The roots are read as streams. The metadata design loads whole maps into RAM at mount (`load_from_root`) and in the GC mark; rehash does not add to that, and its own walk does not use them.

## Runtime State Inventory

| Category | Items Found | Action Required |
|----------|-------------|------------------|
| Stored data | The store itself: packs, index runs, segments, `FORMAT` hold the old IDs | The conversion is the phase. Old objects are freed by the GC; segment records are replaced by the switch segment; `FORMAT` hash line (and flag) rewritten |
| Live service config | None: no external service keeps the hash name or IDs | None, verified by the layout of the store directory |
| OS-registered state | `flock` on the store directory (a running mount holds it); `mount.lock` file (stale after a crash) | Take `lock_offline`; default: refuse on a present `mount.lock` |
| Secrets / env vars | None | None |
| Build artifacts | `<store>/gc-tmp`, new `<store>/rehash-tmp`; `Cargo.lock` | Remove temp dirs at start and end; commit `Cargo.lock` |

## Common Pitfalls

### Pitfall 1: A second lock or a second ChunkStore in one process
**What goes wrong:** `gc_report` inside rehash is refused ("is in use"), or it opens a second writer on the same packs and removes `gc-tmp`.
**Why:** `flock` is per open file description; `gc_report` calls `lock_offline` and `store_io` itself.
**How to avoid:** a GC entry on the open `ChunkStore` (Q6).
**Warning signs:** the test of the GC step fails with "is in use" after 250 ms.

### Pitfall 2: Superseded snapshot records keep the old trees alive
**What goes wrong:** GC frees nothing; `snapshot list` and scrub show doubled snapshots.
**Why:** the loaders return every record; `gc.rs:117` and `scrub.rs:103` use all of them.
**How to avoid:** last record per version inside both loaders; assert "no object of another tag is left" after the GC.

### Pitfall 3: Offline GC leaves segment copies
**What goes wrong:** a test that counts segment files or records fails after a GC; old records can sort after the switch segment after a double fault.
**Why:** `compact_segment` writes `segment-{id+100000}.seg` and keeps the source.
**How to avoid:** rehash does not call `GarbageCollector`; tests do not assert segment counts after a GC; delete older segments in the switch.

### Pitfall 4: The word `unsafe`
**What goes wrong:** `scripts/safety/unsafe-count.sh` fails.
**Why:** it counts the word in every `.rs` file, comments included. A counting-allocator test needs doc entries.
**How to avoid:** do not write the word in new code or comments; check the memory bound structurally (cache size constant, file size grows).

### Pitfall 5: Scanning for foreign tags in node bytes
**What goes wrong:** the BLAKE3 name keys of directory lists are taken for old objects, or the 33-byte stride is wrong in a manifest.
**How to avoid:** decode by node kind only.

### Pitfall 6: A target-tag key that embeds old keys
**What goes wrong:** after `mount --hash`, a manifest or a map node has the target tag but lists old keys; "kept as is" leaves old objects reachable and the GC cannot free them.
**How to avoid:** always translate key-carrying nodes; only byte-copy values are skipped by a tag check (and only if every object in their tree has the target tag, D-14).

### Pitfall 7: Refcount blob order
**What goes wrong:** the blob is not sorted by the new keys, so the root differs from a fresh commit (and a canonical-form check fails).
**How to avoid:** extsort by new key and merge equal keys.

### Pitfall 8: `get` is not the unverified read
**What goes wrong:** a plan "uses `get` for speed" or "avoids `get` because it skips verification". Both wrong.
**How to avoid:** use `read` for its error kinds; there is no unverified read.

### Pitfall 9: Rehash time on a CPU with no SHA extensions
**What goes wrong:** about 250 MB/s per core for the SHA-256 target (about 16 times slower than BLAKE3 here), plus zstd. A large store takes hours.
**How to avoid:** print progress; tell the user in the docs; tests use small data. A rehash to BLAKE3 is fast.

### Pitfall 10: Disk space
**What goes wrong:** the run needs room for a second copy of the live data until the GC.
**How to avoid:** say it in the docs and in the report ("store disk usage before and after"); an up-front free-space check is optional.

## Code Examples

### Translate a flat `u64 -> key` map by streaming
```rust
// blob layout: [count u64 LE][ino u64 LE + key (33)]*   MAP_ENTRY = 8 + KEY_BYTES = 41
let mut r = FileReader::open(cs, to_id(&map_key))?;               // verified
let head = r.read(cs, 0, 8)?;  let count = u64::from_le_bytes(head[..8].try_into().unwrap());
let mut w = FileWriter::new();  w.write(cs, &head)?;
let mut last_ino: Option<u64> = None;
let mut off = 8u64;
for _ in 0..count.div_ceil(RECS_PER_WINDOW) {                      // RECS_PER_WINDOW records = one window
    let buf = r.read(cs, off, RECS_PER_WINDOW as usize * 41)?;     // whole records only; check the length
    let mut out = Vec::with_capacity(buf.len());
    for rec in buf.as_chunks::<41>().0 {
        let ino = u64::from_le_bytes(rec[..8].try_into().unwrap());
        if last_ino.is_some_and(|p| ino <= p) { return Err(corrupt("map not strictly ascending")); }
        last_ino = Some(ino);
        let new = translate(ObjectKey(rec[8..].try_into().unwrap()))?;   // DiskMap hit, or rewrite + insert
        out.extend_from_slice(&rec[..8]); out.extend_from_slice(&new.0);
    }
    w.write(cs, &out)?;  off += buf.len() as u64;
}
let new_map_key = to_key(w.finish(cs)?);   // equals intern_u64_digest_map of the same entries
```

### GC entry on an open store (shape)
```rust
pub fn gc_on_open_store(cs: &Arc<ChunkStore>, roots: &[ObjectKey], full: bool) -> Result<chunk_store::GcStats, Box<dyn Error>> {
    let no_stop = AtomicBool::new(false);
    if full {
        let boundary = cs.gc_begin_full()?;
        match live_objects_exact(cs, roots, &[], &no_stop) {
            Ok(Some(ids)) => Ok(cs.gc_finish_full(ids, boundary)?),
            Ok(None) => { cs.gc_abort(); Err("GC mark stopped, nothing deleted".into()) }
            Err(e) => { cs.gc_abort(); Err(format!("GC mark failed, nothing deleted: {e}").into()) }
        }
    } else { /* live_objects + gc_begin + gc_finish, as gc.rs:142-146 */ }
}
```

## State of the Art

| Old Approach | Current Approach | When Changed | Impact |
|--------------|------------------|--------------|--------|
| `sha2` 0.10 with `asm` feature for ARM | `sha2` 0.11 with intrinsics backends and `cpufeatures` detection | `sha2` 0.11 line (0.11.2 current) | No assembly crate; run-time choice on x86_64 and aarch64 |
| Unused `sha2-compress` | removed | this phase | one dependency and two transitive crates less |

**Deprecated/outdated:** none beyond the above.

## Assumptions Log

| # | Claim | Section | Risk if Wrong |
|---|-------|---------|---------------|
| A1 | SHA-256 with SHA-NI or ARMv8 SHA2 runs at about 1.5 to 2 GB/s per core | Standard Stack, Speed | Only the docs wording; no code depends on it |
| A2 | Hosted CI runners (Linux x86_64, macOS arm64) have the SHA extensions, so the accelerated path runs there | Validation Architecture | The accelerated path would be untested; the soft path still passes the golden vectors |
| A3 | A tree of 78,000 files per version gives about 250,000 distinct keys per version and 50 to 100 MB of map file for ten versions | Q5 | Only the disk estimate in the docs and the initial table size |
| A4 | A refcount key that no manifest of its root uses and whose object is missing is stale bookkeeping and may be dropped | Q4 | If wrong, a store loses a stat count; no data. Owner may prefer to abort |
| A5 | `load_from_root(new_root)` then `commit()` returns `new_root` (canonical form) | Q12 | Only a test idea; check in Wave 0 |
| A6 | A stale `mount.lock` with a free store lock means a crashed mount | Q6 | Rehash refuses when it need not; reversible |
| A7 | The old-to-new table has about 66 bytes per slot at load 0.5 and a cache of 2^14 slots is enough | Q5 | Tuning only |

## Open Questions

1. **Refcount policy (Q4).** Translate and re-sort with `extsort` (recommended, meets D-04 to the letter) or hold one root's refcounts in RAM like a mount does (simpler, O(files of one root) RAM) until #165 removes the table? Default: extsort. Drop-or-abort for a stale key: default drop and count.
2. **GC when every object already has the target tag (argues against D-23).** D-23 says no GC is needed then, but D-18 says a rerun after a crash between the switch and the GC must finish the job, and that state is exactly "all live objects have the target tag, old objects still stored". Recommendation: always run the full GC in a non-dry-run, also when nothing was rewritten (the report says it). Alternative: a marker file; more code for less value.
3. **Refuse on a present `mount.lock` too (Q6)?** Default yes (as `snapshot create`); reversible.
4. **Offline GC copies every segment (Q6).** Out of scope; file an issue or leave it. Rehash avoids it.
5. **GC mark RAM.** The existing mark holds one 33-byte key per metadata node and file of all roots. Is that acceptable as the bound of the GC step under D-04? Default: yes, stated in the docs; changing `mark` is a separate task.
6. **Engine in `metadata` or in `slicefs-cli`.** Recommended: `metadata` (private `root_rec`, `MAP_ENTRY`, `RECORD`; `MemIo` tests there). If it goes to the CLI crate, those items must become `pub`.

**Owner questions (true one-way doors not covered by the locked list):** none. The tag value 0x12, the flag name and its release word are format items, but the format is not frozen and nothing is deployed (D-03, D-11). The tag byte becomes a one-way door at the format freeze, which is the owner's call.

**Where the reversible defaults of CONTEXT are off, by evidence:**
- D-17 "stop at the first record after the last root": not possible with `scan_store_with_log`; use `log.is_empty()` (Q8). Also run `require_store` before the lock, so that a missing path gives the store message and not an I/O error.
- D-15 "never `get`": `get` also verifies; use `read` for the error kinds (Q9).
- D-16 "a writable open removes what a crash left, as for `gc-tmp`": that code is `ChunkStore::open_with` for `gc_tmp_dir` only; add the removal for `rehash-tmp` in `content::open` (Q5).
- D-20 "a GC entry that runs with the held lock": it must also run on the open `ChunkStore` and skip the segment copy step (Q6).
- D-23 "a GC run is not needed then": see Open Question 2.
- D-13 "decode a node, replace every key": only manifest lists, six flat maps and the root record carry keys; inode, directory and xattr nodes are byte copies (Q4).
- D-12 faster path: not recommended (Q3).

## Environment Availability

| Dependency | Required By | Available | Version | Fallback |
|------------|------------|-----------|---------|----------|
| cargo / rustc | all | yes | stable, nightly-2026-02-11 (pinned), nightly-2026-08-21, 1.100.0 | — |
| Kani | header proofs (compile check of `sha2`) | yes | 0.68.0 (`cargo kani`) | — |
| `shasum` | independent golden vectors | yes | macOS | — |
| SHA extensions on this CPU | accelerated-path check | no (Xeon W-3245: AVX2, AVX512F, no SHA) | — | CI runners, a newer x86 or an Apple Silicon machine; hand check |
| Docker / OrbStack | Linux FUSE tests | yes (same CPU) | — | not needed: all HASH-06 tests run in process |
| `gh` | issue #62 | yes | — | — |

**Missing dependencies with no fallback:** none.
**Missing dependencies with fallback:** hardware SHA (see above).

## Validation Architecture

### Test Framework
| Property | Value |
|----------|-------|
| Framework | Rust `cargo test` (libtest), proptest where present |
| Config file | none per crate; features: `test-hasher` (dev-dependencies of `slicefs-cli` and `metadata`) |
| Quick run command | `timeout 600 cargo test -p chunk-store --lib hash::` (ran in 2.3 s here) |
| Full suite command | `timeout 3600 cargo test --workspace -- --skip a_panic_on_another_thread_does_not_abort --skip a_reader_thread_panic_aborts_and_the_remount_replays --skip a_worker_panic_aborts_and_the_remount_replays --skip a_full_queue_that_stays_full_aborts` |

### Phase Requirements to Test Map
| Req ID | Behavior | Test Type | Automated Command | File Exists? |
|--------|----------|-----------|-------------------|-------------|
| HASH-01 | `Sha256` golden vectors (`""`, `abc`, the 56-byte NIST text, one million `a`), tag 0x12, name `sha256`, in `hashers()` | unit | `cargo test -p chunk-store --lib hash::` | no, Wave 0 (new tests in `hash.rs`) |
| HASH-01 | `--hash sha256` on seed (new store: header has the flag before packs) and on mount (`select_hash`), round trip read | integration | `cargo test -p slicefs-cli --test hash_change_tests` (extend) and `--lib header::` | partly (file exists) |
| HASH-01 | parse: `hash sha256` without flag refused; flag without line valid; flag stays after hash back to `blake3-256`; class must be incompat | unit | `cargo test -p slicefs-cli --lib header::` | yes (extend) |
| HASH-02 | file key after rehash equals `put_all` of its bytes (default and large profile, sparse, spliced mixed file) | integration | `cargo test -p slicefs-cli --test rehash_tests` | no, Wave 0 |
| HASH-02 | engine on `MemIo`: byte-copy and key-carrying nodes, refcount sort and merge, root decodes to the same tree | unit | `cargo test -p metadata --lib rehash::` | no, Wave 0 |
| HASH-03 | refusals: lock held exclusive, held shared, log records after the last root, `mount.lock` present, unknown target | integration | `cargo test -p slicefs-cli --test rehash_tests refuse` | no, Wave 0 |
| HASH-04 | switch segment crash states; loader keeps last record per version; GC after the switch frees all old objects (none of the old tag remains) | unit + integration | `cargo test -p metadata --lib segment::` and `--test rehash_tests crash` | no, Wave 0 |
| HASH-04 | corrupted old object stops the run before the switch (segments unchanged) | integration | `cargo test -p slicefs-cli --test rehash_tests verify` | no, Wave 0 |
| HASH-05 | report fields; `--dry-run` leaves every file of the store byte-equal (hash of the tree) and makes no `rehash-tmp`; map cache constant while entries grow 10x | integration + unit | `cargo test -p slicefs-cli --test rehash_tests dry_run` and `cargo test -p metadata --lib rehash::map` | no, Wave 0 |
| HASH-06 | BLAKE3 -> SHA-256 -> BLAKE3, SHA-256 -> BLAKE3 -> SHA-256, mixed store (keyed hash and splice) ends with one tag; files and snapshots (version, name, `created_at`, content) equal; scrub passes | integration | `cargo test -p slicefs-cli --test rehash_tests round_trip` | no, Wave 0 |

### Sampling Rate
- **Per task commit:** the targeted command of the task above (each under 30 s).
- **Per wave merge:** `timeout 900 cargo test -p chunk-store -p metadata -p slicefs-cli` and `scripts/safety/unsafe-count.sh`.
- **Phase gate:** the full suite command above green, plus `scripts/safety/unsafe-count.sh`, `scripts/safety/clippy.sh` and `KANI_HARNESS=name_bytes_total scripts/safety/kani.sh` (the full `kani.sh` is the release gate); then `/gsd-verify-work`.

### Where the accelerated SHA-256 path is checked
Tests cannot see which backend ran (`sha2` does not expose it). The golden vectors pass on every backend. The accelerated path runs on CI runners and developer machines with the extensions; this machine and OrbStack on it have none. Add one `#[ignore]` test (run by hand, `cargo test --release -p chunk-store -- --ignored sha256_speed`) that prints MB/s for 8 KiB blocks: about 250 MB/s means the soft path, above 1,000 MB/s means a hardware path. `[ASSUMED]` thresholds (A1, A2).

### Wave 0 Gaps
- [ ] `crates/chunk-store/src/hash.rs` tests: golden vectors, tag and name lookup, `hashers()` list, ignored speed test
- [ ] `crates/slicefs-cli/src/header.rs` tests: pairing rule, flag kept, `stats` shows the flag
- [ ] `crates/metadata/src/segment/` tests: last record per version in both loaders, `write_switch_segment` states, `.seg.tmp` ignored by loaders, `next_segment_id`
- [ ] `crates/metadata/src/rehash/` tests (engine on `MemIo` with `TestKeyedBlake3` and `Sha256`), `map.rs` tests (hit, miss, growth, cache bound)
- [ ] `crates/slicefs-cli/tests/rehash_tests.rs`: HASH-06, refusals, crash steps, dry-run, verification failure
- [ ] Wave 0 checks: `cargo build --locked` after the dependency change; `KANI_HARNESS=name_bytes_total scripts/safety/kani.sh`; the canonical-form check (A5)

## Security Domain

`security_enforcement` is not set in `.planning/config.json` (absent means enabled).

### Applicable ASVS Categories

| ASVS Category | Applies | Standard Control |
|---------------|---------|-----------------|
| V2 Authentication | no | — |
| V3 Session Management | no | — |
| V4 Access Control | no | the store lock; files stay in the store directory |
| V5 Input Validation | yes | header `parse` (size cap `MAX_HEADER`, name and release byte rules); node decoders check lengths (`map_size`, `MAX_PAYLOAD`); the engine checks strictly ascending `ino` and whole records, never trusts a count for an allocation |
| V6 Cryptography | yes | `sha2` for the digest; no own implementation. SHA-256 here is content addressing and integrity, not secrecy |
| V12 Files and Resources | yes | temp files only inside the store directory (`rehash-tmp`, `*.seg.tmp`), made under the exclusive lock, fixed names, removed at start and end |

### Known Threat Patterns

| Pattern | STRIDE | Standard Mitigation |
|---------|--------|---------------------|
| Tampered or damaged old object | Tampering | every read is verified with the hasher of its tag; any error stops the run before the switch |
| Hash confusion (an ID with an unexpected tag) | Spoofing | the tag selects the verifier; an unknown tag is `UnknownHash`; tag 0 is refused |
| Torn or partial switch | Tampering | temp file, sync, rename, directory sync; loaders ignore `.tmp` and torn tails |
| Old build opens a store with SHA-256 objects | Denial of service / corruption | incompat flag `hash-sha256` before the first object |
| Huge counts in a crafted node | Denial of service | length checks before any allocation (existing `map_size`, `MAX_PAYLOAD`); engine reads in windows |
| Stale temp data reused | Tampering | the map directory is removed at start; the map is never trusted across runs |
| Second writer on the store | Tampering | exclusive `lock_offline`; refuse a present `mount.lock` |

## Sources

### Primary (HIGH confidence)
- Source files read in this session: `crates/chunk-store/src/hash.rs`, `lib.rs` (Options, read, gc_begin_full, gc_finish_full, open_with, zero_chunk, ChunkProfile), `file.rs` (all but test tails), `extsort.rs`; `crates/metadata/src/segment/{mod,reader,writer,compaction}.rs`, `wal/mod.rs`, `store.rs` (root_rec, commit, snapshots, referenced), `store_io.rs`, `manifest.rs`, `inode.rs`, `inode_map.rs`, `directory.rs`, `xattr.rs`, `gc/mod.rs`, `gc/background.rs`; `crates/slicefs-cli/src/{header,content,gc,snapshot,scrub,seed,mount,cli,main,lib}.rs`; `crates/slicefs-traits/src/key.rs`; tests `hash_change_tests.rs`, `memory_bound_tests.rs` (head), `tests/common/mod.rs`, `mount_lifecycle_tests.rs`; `docs/design/store-format.md`, `docs/design/unsafe-review.md`, `scripts/safety/*.sh`, `.github/workflows/safety.yml`; `.planning/ROADMAP.md`, `REQUIREMENTS.md`, `STATE.md`, `config.json`.
- `sha2-0.11.2` and `cpufeatures-0.3.1` source (backend selection, detection, Miri shim), `sha2-compress-0.7.2` README.
- Runs: `cargo info sha2`, scratch builds on 1.100.0 and nightly-2026-02-11, `cargo kani` on a scratch crate, `slicefs seed` and `slicefs gc` on a scratch store, `cargo test -p chunk-store --lib hash::`, `cargo test -p slicefs-cli --test hash_change_tests`, `shasum -a 256`.
- `gsd-tools query package-legitimacy check --ecosystem crates` for the packages in the audit table.

### Secondary (MEDIUM confidence)
- multicodec table (`github.com/multiformats/multicodec`, `table.csv`) for the 0x12 and 0x1e codes.
- Issue #62 text (`gh issue view 62`).

### Tertiary (LOW confidence)
- Speed of hardware SHA-256 (A1) and CI runner features (A2): not measured here.

## Metadata

**Confidence breakdown:**
- Standard stack: HIGH - versions, tree, backends and builds checked on three toolchains.
- Architecture: HIGH for the metadata layout, loaders, lock, GC and segment facts (all read); MEDIUM for the map sizing (A3, A7).
- Pitfalls: HIGH - each one traced to a line or a run.

**Research date:** 2026-11-26
**Valid until:** 2026-12-26 for the code facts if Phases 22 to 24 touch other files; re-read `gc/mod.rs`, `segment/mod.rs` and `header.rs` if another plan changes them first.
