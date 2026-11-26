# Phase 23: Hashes and rehash - Context

**Gathered:** 2026-11-26 (auto mode; owner decisions locked, other choices are the recommended defaults and are reversible: no deployments, no store backward compatibility, format not frozen)
**Status:** Ready for planning

<domain>
## Phase Boundary

A store can use SHA-256 as its write hash, and `slicefs rehash --to <algorithm> <store>` converts an offline store from one hash to another without data loss, also after a crash (#62, HASH-01..HASH-06).

In scope:
- A production SHA-256 hasher in `chunk-store` (own tag, hardware-accelerated where the CPU has it), selectable by `--hash sha256` on `seed` and `mount`. BLAKE3-256 stays the default.
- `slicefs rehash --to <algorithm> [--dry-run] <store>`: rewrites every live object (file chunks, file manifests, metadata nodes) under the target hash, then switches the root and every snapshot root to the new keys in one crash-safe step, records the new write hash in the header, and frees the old objects with a GC.
- Refusals: mounted store (store lock), write-log records after the last root, unknown target, unknown object tag.
- Progress lines, bytes rewritten, bytes freed; `--dry-run` that changes nothing; bounded memory.
- Tests (HASH-06): BLAKE3-256 -> SHA-256 -> BLAKE3-256 and SHA-256 -> BLAKE3-256 -> SHA-256 round trips, a mixed store ends with one hash, files and snapshots do not change, scrub passes, crash at each step mounts with the old or the new root.

Out of scope: an online rehash job (offline only, owner decision); re-chunking to another chunk profile (a "rechunk" command); more hashes than SHA-256; segment-format changes beyond what the snapshot switch needs; prolly work (Phase 24).

</domain>

<decisions>
## Implementation Decisions

### Locked by the owner (do not reopen)
- **D-01:** SHA-256 is a production second hash (HASH-01). BLAKE3-256 stays the default. Object IDs name their hash (1 tag byte + 32-byte digest). Full digests, no bit saving (global/decentralized goal).
- **D-02:** Rehash is offline only. It refuses while mounted by the existing exclusive store lock: `header::lock_offline` in `crates/slicefs-cli/src/header.rs` (a writable mount holds it for its whole life since #201; a read-only mount holds it shared). Not the `mount.lock` file. It also refuses while the write log has records after the last root.
- **D-03:** No store backward compatibility (no deployments). The Phase 21.1 header and format policy (`docs/design/store-format.md`) governs any new header line or flag.
- **D-04:** Online GC stays mandatory; a GC frees the old objects after the switch. Memory is bounded: no in-RAM map of all objects; per-file memory never O(file size).
- **D-05:** Rehash keeps the chunk profile of the store (Phase 22): a rehashed file gets the same key as a fresh write of its bytes under the store profile and the target hash.
- **D-06:** Crash safety: a crash at any point leaves a store that mounts with the old or the new root.
- **D-07:** Safety gates (`scripts/safety/*`, release gate) stay green; the unsafe-count doc matches (`scripts/safety/unsafe-count.sh`). Tests are short; benchmarks only by hand. Local gate: `timeout 3600 cargo test --workspace -- --skip a_panic_on_another_thread_does_not_abort --skip a_reader_thread_panic_aborts_and_the_remount_replays --skip a_worker_panic_aborts_and_the_remount_replays --skip a_full_queue_that_stays_full_aborts`.
- **D-08:** Plans run as executors in their own git worktrees (local branch `exec/23-NN`). Executors never push. No local macOS mounts; Linux FUSE tests only in OrbStack Docker (`scripts/docker-linux-test/run.sh`), and only if a path is not reachable in process.

### SHA-256 hasher
- [auto] Q: "Name, tag and place of the SHA-256 hasher?" -> Selected: "`Sha256` in `crates/chunk-store/src/hash.rs`, name `sha256`, tag = multihash code of SHA2-256 (0x12)" (recommended default)
- **D-09:** A `Sha256` type next to `Blake3_256` in `hash.rs`, in `HASHERS` in all builds. Name `sha256` (the word in HASH-01 and the roadmap). Tag 0x12, the multihash code of sha2-256, as 0x1e is the multihash code of BLAKE3. Research confirms the code and that no test or fixture uses 0x12 already.
- **D-10:** Hardware acceleration by a maintained crate with run-time CPU detection (SHA-NI on x86_64, ARMv8 SHA2 on aarch64), no new `unsafe` code of ours, so the unsafe count does not change. Default: RustCrypto `sha2` (its `cpufeatures` detection). Research checks what the workspace dependency `sha2-compress` (in `Cargo.toml` and `crates/slicefs-cli/Cargo.toml`, with no source use found) is, and whether to use or drop it. A golden test checks the digest against a known SHA-256 vector, and `--hash sha256` round-trips through seed and mount in process.

### Header for SHA-256
- [auto] Q: "Does a store with SHA-256 objects need a flag (21.1 rule)?" -> Selected: "Yes: an incompat flag written when the store first uses SHA-256, and kept" (recommended default)
- **D-11:** An older format-7 build that does not know SHA-256 cannot read SHA-256 objects. It already refuses a `hash sha256` line, but a store can have live SHA-256 objects with the hash line back at `blake3-256` (after `mount --hash`, or mid-rehash). So by the 21.1 rule ("it cannot read the store: incompat"), the store gets the flag line `incompat hash-sha256 <release>` when it first gets SHA-256 objects (seed, mount or rehash with `--hash sha256` / `--to sha256`), before any SHA-256 object is written. `hash-sha256` goes into `KNOWN_FLAGS`. A `hash sha256` line without the flag is refused as a bad header (M9); the flag without the line is valid (the store may still hold SHA-256 objects). The flag stays after a rehash back to BLAKE3-256; removing it when a GC proves no SHA-256 object is left is the planner's choice only if it is cheap and exact, else deferred. The `<release>` word comes from one constant (as `CHUNK_PROFILE_RELEASE`). If the byte-level header core proved by Kani changes, the harness is updated and `scripts/safety/kani.sh` stays green. — **Reversibility:** reversible — no deployed stores, format not frozen; one parse rule and one flag row.

### Conversion method
- [auto] Q: "How is a file converted?" -> Selected: "Rebuild through the real write path under the target hasher, not a node-by-node ID translation" (recommended default)
- **D-12:** Manifest node boundaries depend on IDs (`is_cut` in `crates/chunk-store/src/file.rs` looks at the last 4 digest bytes), so new IDs move node cuts, and a node-by-node translation of an old manifest cannot give the key of a fresh write. So a file is converted by streaming its bytes (verified reads, bounded buffer) into a `FileWriter` of a store opened with the target hasher and the store's chunk profile; the result is by construction the key of a fresh write (D-05). Research checks whether the chunk cut points depend only on bytes and profile (they should: FastCDC on the data), so that a faster path (reuse the old chunk boundaries, re-hash each chunk, rebuild the manifest with the real manifest builder) can give the same key without re-cutting; the planner may use it only if a test proves key equality with a fresh write. Zero runs: the zero IDs are per store and per hasher (Phase 22 D-15), so the conversion store must be opened with the header profile and the target hasher, and the sparse shortcut must give the fresh-write key.
- **D-13:** Metadata nodes (directory map, inode map, xattrs, manifests of metadata values) are rebuilt bottom-up: decode a node, replace every content key and child-node key by its new key, encode, and put it under the target hasher. Keys that are not content addresses (a directory-entry key is BLAKE3-256 of the name, see `docs/design/chunk-store.md`) do not change. Research lists every node type and every place a key is stored, and states whether any metadata map uses ID-dependent boundaries (then it must be rebuilt by its real builder, as D-12). The new root must decode to the same tree (same names, inodes, attributes, sizes, file bytes) as the old root.
- **D-14:** An object that already has the target tag and whose whole subtree has the target tag is kept as is (no rewrite). A mixed subtree (for example a manifest of the target tag over chunks of another tag, which splice after `mount --hash` may make) is rebuilt. Research confirms whether mixed manifests can exist.
- **D-15:** Old objects are read with the verifying path (`ChunkStore::read`, which checks each object with the hasher of its tag), never `get`. A bad object or an unknown tag stops the rehash with an error before the switch; nothing is switched.

### Old-to-new map
- [auto] Q: "How does rehash avoid redoing shared subtrees (snapshots share most nodes and files)?" -> Selected: "A disk-backed map in a temp dir of the store, with a bounded RAM cache; correctness never depends on it" (recommended default)
- **D-16:** The map is keyed by old file content key and old metadata node key (not by single chunk: chunks dedup by themselves under the target hash). RAM is bounded by a fixed cache size; the rest is on disk in `<store>/rehash-tmp/` (the `gc-tmp` pattern; a writable open removes what a crash left, as for `gc-tmp`). Research picks the structure (random lookups during a walk: an on-disk hash table of fixed-size records, or sorted runs with `extsort` if the walk can be ordered). A miss only costs a rewrite (the result is the same key), so a lost or partial map after a crash is safe. Memory and disk cost are stated in the docs.

### Write-log and lock checks
- **D-17:** Order: take `header::lock_offline` first (exclusive; refused while any mount or offline command holds it), then the header check, then the write-log check with the streaming `scan_store_with_log` (stop at the first record after the last root; `load_store_with_log` keeps all records in RAM and is not used). Message for log records: the store has write-log records that are not in a root; mount and unmount the store once, then run rehash again. The lock is held to the end, GC included.

### Switch protocol and snapshot supersession
- [auto] Q: "How do the root and all snapshot roots switch at once?" -> Selected: "One atomic switch: new objects synced first, then one durable segment step that holds the new root and a superseding record for every snapshot" (recommended default)
- **D-18:** Order of a run: (1) checks (D-17); (2) if the target is SHA-256, write the flag (D-11) durably; (3) rewrite all live objects under the target hasher and sync the packs and index; (4) switch: the new root and the new snapshot roots become current in one durable step; (5) rewrite the header `hash` line to the target; (6) GC frees the old objects; (7) report. Legal states after a crash: before (4) the store mounts with the old root (new objects are garbage for the next GC; a new rehash redoes the work, mostly as dedup hits); after (4) it mounts with the new root, maybe with the old hash line (a mixed store is legal and readable) and old objects not yet freed. A rerun of `rehash` after a crash at any step completes the job. A crash test with in-process fail points covers each step boundary.
- **D-19:** Snapshot records are append-only (`load_store_from_segments` collects every `SnapshotRecord`; there is no snapshot delete). Rehash must keep each snapshot's version, name and `created_at` and change only its root. Research decides between (a) a superseding `SnapshotRecord` with the same version (the loader keeps the last record of each version), written with the `RootUpdate` in one new segment that becomes visible atomically (temp file, sync, rename, dir sync), and (b) an atomic rewrite of the segment files. Whatever is chosen, every loader of snapshots (mount, gc, online gc, scrub, `snapshot list`/`switch`, segment compaction) sees exactly one root per snapshot, so a GC after the switch does not keep the old snapshot trees alive. Default: (a), because it adds one loader rule and no segment-format change. — **Reversibility:** reversible — no deployed stores.

### GC and report
- **D-20:** Rehash runs the GC itself after the switch (step 6), under the store lock it already holds, and reports the bytes freed from it. `gc_report` takes `lock_offline` itself, so the plan adds a GC entry that runs with the held lock (no second lock). Default GC mode: full (exact mark, no holes) so that all old objects go; research checks its cost and memory bound. `--no-gc` is not added unless research finds a need.
- **D-21:** Report: progress lines on stderr at a fixed interval (objects and bytes done, phase of the run), and a final summary on stdout: objects and bytes read, objects and bytes rewritten (new objects written), objects kept as is, bytes freed by the GC, and the store disk usage before and after (the `gc` usage line). Plain short text; wording is the planner's choice.
- **D-22:** `--dry-run` writes nothing: no temp dir, no flag, no header rewrite, no pack. It walks the live tree read-only (`open_read_only`), verifies objects, and reports what a real run would rewrite (counts and bytes of objects with another tag; an estimate is fine for bytes freed, and the report says it is an estimate). It still takes the store lock and does the write-log check, so it reports the same refusals as a real run.

### CLI
- **D-23:** `slicefs rehash --to <algorithm> [--dry-run] <store>`. `--to` takes a known hasher name (`parse_hash`). The store must exist (`require_store`). If every live object already has the target tag, rehash says so, still makes sure the header hash line is the target, and changes nothing else (a GC run is not needed then).

### Tests
- **D-24:** HASH-06 in process (no mount): a BLAKE3-256 store and a SHA-256 store each convert to the other and back; a mixed store (`TestKeyedBlake3` or `mount --hash`-style hash change) ends with one tag on every live object; files (bytes, sizes, sparse/zero runs, a spliced file) and snapshots (version, name, `created_at`, content) do not change; scrub passes; the key of each rehashed file equals the key of a fresh write of its bytes under the target hash and the store profile, for the default profile and one large profile. Crash tests: fail points at each step boundary of D-18, then reopen: the store loads the old or the new root and a rerun finishes. Refusal tests: lock held (exclusive and shared), write-log records after the last root, unknown target, an object that fails verification. A memory test or bound check shows the map does not grow with the object count beyond the cache size.

### Claude's Discretion
- Type, function and module names; message wording (plain, short); the flag word `hash-sha256` may change if the planner has a better kebab-case name; progress interval; map structure details within D-16; whether the faster chunk-reuse path of D-12 is used.
- Suggested plan split (planner may change it; plans in one wave must not touch the same files): 01 SHA-256 hasher, `--hash sha256`, flag `hash-sha256`, docs (wave 1); 02 snapshot supersession in the segment loader and the atomic switch writer (wave 1, metadata crate); 03 rehash engine: file and metadata rebuild, disk-backed map, bounded memory (wave 2); 04 CLI command, checks, dry-run, GC with the held lock, report, crash and HASH-06 tests, docs (wave 3).

</decisions>

<canonical_refs>
## Canonical References

**Downstream agents MUST read these before planning or implementing.**

### Phase scope and requirements
- `.planning/ROADMAP.md` — Phase 23 goal, success criteria, research flag (old-to-new map strategy, GC interaction, snapshot record supersession)
- `.planning/REQUIREMENTS.md` — HASH-01..HASH-06
- GitHub issue #62 — rehash requirements (read with `gh issue view 62 -R o2alexanderfedin/slicefs`)

### Hashes and the chunk store
- `crates/chunk-store/src/hash.rs` — `ObjectHasher`, `Blake3_256` (tag 0x1e), `TestKeyedBlake3` (0xf0), `HASHERS`, `hasher_by_name`/`hasher_by_tag`, mixed-hash GC test
- `crates/chunk-store/src/lib.rs` — `ObjectId`, `Options` (hasher, profile, gc_tmp_dir), `read` (verified) vs `get`, `gc_begin`/`gc_finish`/`gc_finish_full`
- `crates/chunk-store/src/file.rs` — `FileWriter`, `FileReader`, `file_objects`, `is_cut` (ID-dependent node cuts), zero chunk per store, `splice`
- `crates/chunk-store/src/extsort.rs` — sorted temp files with bounded RAM (candidate for the map)
- `docs/design/chunk-store.md` — hash section (verification by tag, no dedup across hashes, directory-entry keys are BLAKE3 of the name, `is_cut` ignores the tag), memory bound of a stream

### Metadata, segments, snapshots, GC
- `crates/metadata/src/segment/mod.rs` — record types, `load_store_from_segments` (collects every `SnapshotRecord`), `scan_store_with_log`, `load_store_with_log`
- `crates/metadata/src/segment/compaction.rs` — keeps all root and snapshot records
- `crates/metadata/src/snapshot.rs` — `SnapshotEntry` (version, name, root, created_at)
- `crates/metadata/src/store_io.rs`, `crates/metadata/src/store.rs`, `directory.rs`, `inode.rs`, `inode_map.rs`, `xattr.rs`, `manifest.rs` — node types and where keys are stored
- `crates/metadata/src/gc/mod.rs` — `live_objects`, `live_objects_exact`, `collect_content_keys`
- `crates/slicefs-cli/src/gc.rs` — `gc_report` (takes `lock_offline` itself), usage and stats lines
- `crates/slicefs-cli/src/snapshot.rs` — snapshot create/list/switch, `write_root_update`
- `crates/slicefs-cli/src/scrub.rs` — scrub (must pass after rehash)
- `docs/design/write-log.md` — the write log and "records after the last root"

### Header, CLI, safety
- `docs/design/store-format.md` — header grammar, flag classes, rule to add a flag, read-only mode, store lock
- `crates/slicefs-cli/src/header.rs` — `KNOWN_FLAGS`, `parse`, `check`, `lock_offline`, Kani-proved core
- `crates/slicefs-cli/src/content.rs` — `parse_hash`, `select_hash`, `check_write_with`, `open`, `open_read_only`, `GC_TMP_DIR`
- `crates/slicefs-cli/src/cli.rs` — subcommands
- `.planning/phases/22-chunk-profiles/22-CONTEXT.md` — profile per store, per-store zero chunk, header line + flag pattern
- `.planning/phases/21.1-store-header-and-format-policy-154/21.1-CONTEXT.md` — header decisions
- `scripts/safety/kani.sh`, `scripts/safety/unsafe-count.sh`, `docs/design/unsafe-review.md` — release gate

</canonical_refs>

<code_context>
## Existing Code Insights

### Reusable Assets
- Tagged IDs, the `ObjectHasher` trait, per-tag verification, and mixed-hash GC already exist (`hash.rs` tests `a_new_write_hash_keeps_old_objects_readable`, `gc_keeps_live_objects_of_both_hashes`). SHA-256 is one more `ObjectHasher`.
- `content::check_write_with` already rewrites the `hash` line in place; rehash step (5) reuses it.
- `header::lock_offline` gives the exclusive store lock; `content::require_store` refuses a non-store path.
- `scan_store_with_log` streams the write-log records; `load_store_from_segments` gives root and snapshots.
- `extsort.rs` and the `gc-tmp` cleanup give bounded-RAM temp-file patterns.
- `TestKeyedBlake3` (feature `test-hasher`) gives a second tag for mixed-store tests.

### Established Patterns
- Store settings live in `FORMAT`; a new setting line comes with a flag; an unknown hash name is refused.
- Durable writes: temp file, sync, rename or link, dir sync.
- Offline commands take `lock_offline` and hold it to the end.
- Tests are short; long runs are `#[ignore]` or scripts run by hand.

### Integration Points
- `crates/chunk-store/src/hash.rs` (new hasher), workspace `Cargo.toml` (crate).
- `crates/slicefs-cli/src/header.rs` (flag), `content.rs` (open with a given hasher), `cli.rs` + new `rehash.rs` (command), `gc.rs` (GC with a held lock).
- `crates/metadata/src/segment/mod.rs` (snapshot supersession in the loader, switch writer), `compaction.rs`.
- A new rehash engine module (metadata crate or slicefs-cli; planner's choice).

</code_context>

<specifics>
## Specific Ideas

- Command form from the issue: `slicefs rehash --to <algorithm> <store>`; option `--dry-run`.
- Rehash is idempotent by content addressing: a rerun after a crash finds most new objects present and only rebuilds.
- No home paths in committed text; no license talk; plain short English.

</specifics>

<deferred>
## Deferred Ideas

- An online rehash job that works with the online GC (issue #62 allows it; the owner chose offline only).
- Removing the `hash-sha256` flag after a GC proves no SHA-256 object is left (unless cheap and exact in this phase, D-11).
- A rechunk command (another chunk profile for an existing store).
- More hashes beyond SHA-256.

</deferred>

---

*Phase: 23-hashes-and-rehash*
*Context gathered: 2026-11-26*
