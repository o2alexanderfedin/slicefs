# Phase 24: Prolly evaluation - Context

**Gathered:** 2026-11-26
**Status:** Ready for planning
**Mode:** `--auto` (recommended option taken for each gray area; each choice is logged below and in 24-DISCUSSION-LOG.md)

<domain>
## Phase Boundary

The owner gets measured numbers and a written decision (adopt, later or drop) on prolly trees for the metadata maps (#19, PROLLY-01..03). The phase delivers three things:

1. A prototype prolly map for **one** metadata map, in a **separate crate** that nothing in the mount, seed or commit path depends on. Its nodes are chunk-store objects. Its node boundaries depend only on the keys (with a size-aware rule).
2. A benchmark on the Boost trees (about 78,000 files per version) that compares the current commit path with the prototype: commit time, CPU, peak memory, bytes written per commit, and on-disk size after several small commits. It runs by hand (macOS or Linux Docker), never in `cargo test`.
3. A written decision in the design docs (and a text ready for #19) that also states that file manifests are already content-cut trees.

Not in this phase: any migration, any change of the store format, any change to the default store behaviour, wiring the prototype into the root record, the commit path or the mount.

</domain>

<decisions>
## Implementation Decisions

### Locked by the owner (do not reopen)
- **D-01:** Prolly work is a prototype plus a written decision. No migration, no store format change, no change to the default store behaviour in this phase. The prototype is not wired into the root record, `DictMetadataStore`, `slicefs-cli` or any mount path.
- **D-02:** The prototype must not hold a whole map or a whole file in RAM. Memory stays bounded: O(tree depth x max node size + the current batch of edits), never O(map entries). (Owner rule "no whole file in RAM", #98.)
- **D-03:** No store backward compatibility is needed, but this phase must not change the on-disk format anyway. No change to `docs/design/store-format.md` grammar, the FORMAT header, `KNOWN_FLAGS`, the root record (`root_rec`) or the segment records.
- **D-04:** FastCDC default stays 4/8/32 KiB. BLAKE3-256 stays the default hash (full 256-bit IDs, no bit saving). The 1 TiB cap stays.
- **D-05:** Executors run in git worktrees, one per plan (local branch `exec/24-NN`, never pushed). No file is in two plans of the same wave. The workspace `Cargo.toml` (members list) and `Cargo.lock` count as files: only one plan per wave may change them.
- **D-06:** Tests stay short (test-time budget). The benchmark runs by hand or in Linux Docker (`scripts/docker-linux-test/run.sh`), never in the normal test suite. `cargo test` may only build the benchmark binary and run a tiny smoke unit test, if any.
- **D-07:** No local macOS mounts. The benchmark needs no mount: it runs in process on the chunk-store and metadata APIs (as the Phase 22 benchmark does).
- **D-08:** Reversible choices are decided and recorded here. One-way doors are listed for the owner and not decided (see "Owner questions").

### Which map
- [auto] Which map — Q: "Which metadata map does the prototype replace in the benchmark?" -> Selected: "The largest flat map that every file change touches: `inode_data` (u64 inode -> ObjectKey), with the same edits also applied to `dir_data` if cheap" (recommended default)
- **D-09:** The prototype is a generic sorted map of byte keys to byte values (keys compare as bytes; u64 keys are encoded big-endian so byte order is numeric order). The benchmark uses it for the `inode_data` map (`BTreeMap<u64, ObjectKey>` in `crates/metadata/src/store.rs`, one entry of 8 + 33 bytes per inode, about 78,000 entries per Boost version, about 156,000 for two versions). The roadmap says "for example the directory map"; `inode_data` is chosen because it is the largest map, every file change touches it, and its fixed-width values make "boundaries depend only on the keys" exact. The researcher measures the entry counts of all six maps (`inode_map`, `inode_data`, `dir_data`, `manifest_data`, `xattr_data`, `refcounts`) on the Boost store; if `dir_data` (or another map) is the better single target, the planner may switch and records why.
- **D-10:** The unit tests of the crate also use variable-length keys and values (for example directory-style `(parent u64, name)` keys), so the boundary rule is checked beyond fixed-width entries.

### Boundary rule (research flag)
- [auto] Boundary rule — Q: "How are node boundaries chosen?" -> Selected: "Dolt-style size-aware rule on a hash of the key only" (recommended default)
- **D-11:** A node ends after an entry when a hash of that entry's **key** (never its value bytes) falls under a threshold that grows with the current node byte size (Dolt's rule: probability from a Weibull or similar CDF of the node size; a hard minimum and maximum node size). The same rule cuts every level; an internal entry is (last key of the child, child ObjectId, entry count of the subtree). The researcher fixes the exact rule and parameters (target, min and max node size, hash function, the CDF and its shape) from Dolt's sources and the chunk-store object limits; the planner records them as constants with one comment each. Starting point: target about 4 KiB per node, min about 512 B, max 16 KiB (well under the chunk store's one-record limit).
- **D-12:** The hash is a fast keyed or plain hash of the key bytes. It does not need to be BLAKE3; it must be stable across runs and platforms (no `std` `RandomState`). The researcher picks it (for example the first 8 bytes of BLAKE3 of the key, since `blake3` is already a dependency, or xxhash/`foldhash` with a fixed seed) and states the cost per key.
- **D-13:** "Boundaries depend only on the keys" means: with values of fixed length, the node boundaries are a function of the key set alone. With variable-length values, the size-aware rule makes boundaries depend on the key set and the entry byte lengths, never on value content bytes. The doc states this precisely. Tests prove: (a) the same key/value set built in different insert orders, or by bulk build versus by batches of edits, gives the same root ObjectId; (b) changing values of the same length does not move any boundary; (c) no node is above max or (except the last of a level) below min.

### Node storage and format (prototype only)
- [auto] Node storage — Q: "How is a node stored?" -> Selected: "Each node is one chunk-store object via `ChunkStore::put`, its ObjectId is the child pointer" (recommended default)
- **D-14:** Each node is one chunk-store object (`ChunkStore::put` / `get` / `read`), so it is deduped, compressed and packed like file chunks and its ID uses the store hasher. Nodes are not cut again by FastCDC (`store_io::put_value` is not used): the prolly rule already makes them content-defined and small. The node byte format is private to the prototype crate (a magic, level, count, then length-prefixed entries), documented in the crate's module doc, and is explicitly not a store format (D-03).
- **D-15:** The root of a map is one ObjectId (a snapshot is a root ID). No root record, no segment record, no header line is written.

### Memory and API
- [auto] API — Q: "What does the prototype offer?" -> Selected: "Bulk build from a sorted stream, point get, apply a sorted batch of edits, ordered iteration; nothing more" (recommended default)
- **D-16:** API: (1) build a tree from a sorted iterator of entries with one in-progress node per level (streaming, bounded); (2) `get(key)`; (3) `apply(batch)` where the batch is a sorted list of puts and deletes for one commit, which rewrites only the touched leaf nodes and their paths (O(changes x depth)) and reuses all other node IDs; (4) an ordered iterator (cursor) that reads one path of nodes at a time. A diff of two roots is deferred (see Deferred).
- **D-17:** No node cache that grows with the map. A small fixed-size cache (by count or bytes) is allowed if the benchmark shows it matters; its bound is a constant and the doc states it.
- **D-18:** The prototype has the size of a prototype: target about 1,000 to 1,500 lines with tests. The researcher checks the maturity of existing Rust prolly-tree crates (as #19 asks) and records the result; the default is our own small implementation on the chunk store with no new heavy dependency, because a third-party crate would need its own node storage. Adding a crate is a reversible choice the planner may make if research shows one fits `ChunkStore` nodes directly.

### Crate and benchmark layout
- [auto] Layout — Q: "Where do the crate and the benchmark live?" -> Selected: "New workspace crate `crates/prolly` (depends only on chunk-store); benchmark binary in `benchmarks/prolly-bench` (depends on prolly, chunk-store, metadata, and the seed path); driver script `scripts/prolly-bench/bench.sh` with a README" (recommended default)
- **D-19:** `crates/prolly` (package name for example `prolly`) depends on `chunk-store` only (plus a hash crate if D-12 needs one). `metadata` and `slicefs-cli` do not depend on it (D-01). The planner checks this with a grep or `cargo tree` in a verify step.
- **D-20:** `benchmarks/prolly-bench` follows `benchmarks/index-bench` and the Phase 22 `chunk_profile_bench` example: a release-built binary, CSV to stdout, a work dir, machine info without path, host or user. `scripts/prolly-bench/bench.sh` follows `scripts/chunk-profiles/bench.sh` (usage, env vars, `SMOKE=1` mode with generated small trees, warm page cache, several runs with a median row, peak RSS by `/usr/bin/time`). A Docker smoke target in `scripts/docker-linux-test/run.sh` is optional, like `chunk-profiles-smoke`.

### Benchmark method
- [auto] Method — Q: "What exactly is compared?" -> Selected: "Same store, same edit script; baseline = the real `DictMetadataStore` commit; prototype = the same edits as prolly batches for the chosen map; both whole-commit and map-only numbers" (recommended default)
- **D-21:** Setup: seed Boost 1_84_0 into a new store in process (the seed code path, default profile and hash), then apply a fixed, seeded edit script of small commits, then the 1_85_0 tree as one large commit. Small commits: for example 10 commits each of K changed files for K in {1, 10, 100, 1000} (content change, metadata change, create, delete), so the cost curve against changes is visible. The researcher and planner fix the exact script; it is deterministic (seeded) and recorded in the report.
- **D-22:** Two comparisons per commit, both reported: (a) **whole commit**: the current `DictMetadataStore::commit` (begin + finish) versus the same commit where the chosen map's full serialization is replaced by the prolly `apply` (measured as current commit minus that map's `intern_u64_digest_map` cost plus the prolly cost, or by a bench-local commit that does exactly that; the planner picks the honest one and documents it); (b) **map only**: interning the chosen map in full (`intern_u64_digest_map` through `NodeWriter`/`put_value`) versus `apply` of the same batch. If a bench-local copy of the map serializer is needed, it is a copy in the bench crate, not a change to `metadata` (D-01).
- **D-23:** Metrics per commit and mode: wall time, CPU user and system time (`getrusage`), peak RSS (one process per mode, `/usr/bin/time`; plus the bench's own high-water report if available), bytes hashed/serialized, new objects and new stored bytes in the chunk store (delta of `ChunkStore::stats()`), and after the whole script the on-disk size of the store (packs apparent and allocated, index included). Several runs (default 3), median reported, as in Phase 22.
- **D-24:** The baseline is measured fairly: `store_io` already cuts each map node with FastCDC under a content-cut manifest tree, so the current path already writes only the changed chunks of a large map. The report must separate "bytes serialized and hashed per commit" (current: O(map size); prolly: O(changes x depth)) from "new bytes stored per commit" (both small), and must show the RAM and CPU cost of the full in-RAM maps (`begin_commit` clones every map). The decision rests on these facts, not on the issue's older assumption that each commit stores the full maps.

### Decision document
- [auto] Doc — Q: "Where does the written decision go?" -> Selected: "Full report in `docs/research/prolly-trees.md`; a short decision section in a design doc; a ready-to-post text for #19 in the phase dir" (recommended default)
- **D-25:** `docs/research/prolly-trees.md`: method, command lines, machine, inputs, tables, the boundary rule, the Rust crate survey, the result, and the recommendation (adopt, later or drop) with reasons. It states that file manifests are already content-cut trees (`crates/chunk-store/src/file.rs`: nodes cut after an entry whose chunk ID matches a bit pattern, `NODE_MIN` 64 to `LEAF_ENTRIES` 4096 entries, entries carry subtree byte lengths, so offset lookup is O(log n) and an edit rewrites one path), and that metadata map nodes are already FastCDC-cut values (`crates/metadata/src/store_io.rs`).
- **D-26:** The design-doc part: a short "Metadata maps: prolly-tree decision" section (status, decision, link to the research doc). The planner picks the file: a new `docs/design/metadata-maps.md` or a section in an existing design doc; it must not touch the format grammar in `store-format.md` (D-03).
- **D-27:** #19: executors run no `gh` write command. The plan writes the comment text to `.planning/phases/24-prolly-evaluation/24-ISSUE-19-COMMENT.md`; the orchestrator or owner posts it. No license text anywhere.
- **D-28:** The recommendation is the deliverable; **adopting** prolly maps (a store format change in a later milestone) is an owner decision (one-way door). The doc lists what adoption would change (root record, all six maps, GC mark, rehash, snapshots, the format number) and its cost.

### Report run
- **D-29:** One plan runs the benchmark by hand with a time limit (`timeout`), on the dev machine (no mount needed) or in Linux Docker, with the Boost data from `scripts/macos-mount-test/fetch-boost.sh`, and commits the results (CSV under the phase `runs/` dir or summarized in the report, as Phase 22 did). If the Boost data cannot be fetched in the executor environment, the plan runs `SMOKE=1` and marks the full run as a human-needed item; it does not invent numbers.

### Claude's Discretion
- Crate, type, function and constant names; the node byte layout; the exact hash; the size-aware CDF and its parameters (within D-11/D-12, from research); the edit-script details (within D-21); whether a small bounded node cache is used (D-17); the design-doc file (D-26); the plan split.
- Suggested plan split (planner may change it, D-05 applies): 01 prolly crate: node format, boundary rule, streaming bulk build, get, sorted batch apply, cursor, tests incl. order independence and bounded memory (wave 1; owns the workspace `Cargo.toml` change); 02 benchmark binary + driver script + README + smoke (wave 2; owns its `Cargo.toml` member line); 03 run the benchmark, write `docs/research/prolly-trees.md`, the design-doc section and the #19 comment text (wave 3).

### Owner questions (one-way doors, not decided here)
- **Q-1:** Adopting prolly maps for the store (a format change, a new format number and root record, GC and rehash changes): only after this report, and in which milestone.
- **Q-2:** If adopted: one prolly map per metadata map, or one combined namespace map (keys like `(kind, inode, name)`), which changes the root record and GC mark design.
- **Q-3:** If adopted: whether the boundary rule and its constants become part of the frozen store format (they decide every node ID, so a later change rewrites every map).

</decisions>

<canonical_refs>
## Canonical References

**Downstream agents MUST read these before planning or implementing.**

### Scope and requirements
- `.planning/ROADMAP.md` — Phase 24 entry (goal, success criteria), owner decisions of v2.2, research flag "Phase 24 (boundary rule details)"
- `.planning/REQUIREMENTS.md` — PROLLY-01, PROLLY-02, PROLLY-03
- GitHub issue #19 (`gh issue view 19`, read only) — goal, properties, points to examine (node size spread, write amplification, node storage, crate maturity), benchmark request

### Current commit path (the baseline)
- `crates/metadata/src/store.rs` — `root_rec` layout, `begin_commit` (clones every map), `finish_commit`/`finish_inner` (interns each map in full each commit), `intern_u64_digest_map`, `intern_key_u64_map`
- `crates/metadata/src/store_io.rs` — `put_value`/`NodeWriter::put_node`: every metadata node is stored as a FastCDC-cut value under a manifest tree
- `crates/metadata/src/inode_map.rs`, `crates/metadata/src/directory.rs` — the inode map and directory lists

### Chunk store and manifests
- `crates/chunk-store/src/lib.rs` — `ChunkStore::put`, `get`, `read`, `stats()` (`Stats { objects, raw_bytes, stored_bytes, packs }`), `Error::TooLarge`, `ChunkProfile`
- `crates/chunk-store/src/file.rs` — the content-cut manifest tree (`is_cut`, `NODE_MIN`, `LEAF_ENTRIES`, `NODE_CUT_ONE_IN`, subtree byte lengths): the basis of the "manifests are already content-cut trees" statement
- `docs/design/chunk-store.md` — chunk store design
- `docs/design/store-format.md` — header and format policy (must not change in this phase)

### Benchmark pattern (Phase 22)
- `scripts/chunk-profiles/bench.sh`, `scripts/chunk-profiles/README.md` — driver script pattern (SMOKE mode, runs, medians, peak RSS, machine info)
- `crates/chunk-store/examples/chunk_profile_bench.rs` — in-process benchmark binary pattern
- `benchmarks/index-bench/` — standalone benchmark crate pattern
- `docs/research/chunk-profiles.md` — report pattern
- `scripts/macos-mount-test/fetch-boost.sh` — Boost data
- `scripts/docker-linux-test/run.sh` — Linux Docker runs
- `crates/slicefs-cli/src/seed.rs` — `run_seed_with` (in-process seed of a directory)
- `.planning/phases/22-chunk-profiles/22-CONTEXT.md` — D-14 (metadata values follow the store profile; manifest node boundaries do not depend on the profile), D-25..D-28 (benchmark and report rules)

</canonical_refs>

<code_context>
## Existing Code Insights

### Reusable Assets
- `ChunkStore::put/get/read/stats`: node storage, dedup and the bytes-written metric for the prototype and the bench.
- `run_seed_with` in `crates/slicefs-cli/src/seed.rs`: in-process seed of a Boost tree into a new store for the bench setup.
- `chunk_profile_bench` example and `scripts/chunk-profiles/bench.sh`: CSV, medians, SMOKE data generator (`gen tree --files --seed --change-every`), peak RSS, machine info.
- `file.rs` `is_cut` and its tests (node size spread, every node but the last within limits): a model for the prolly boundary tests.

### Established Patterns
- Every metadata node already goes through `store_io::put_value` (FastCDC + manifest tree), so a big map that changes in a few places stores only its changed chunks. The cost that remains per commit is the full serialization, hashing and the full in-RAM maps (`begin_commit` clones each `BTreeMap`).
- The six maps (`inode_map`, `inode_data`, `dir_data`, `manifest_data`, `xattr_data`, `refcounts`) are each serialized as one flat node per commit: 8-byte count + fixed entries of 8 + 33 bytes (41 B). About 156,000 inodes give about 6.4 MB per u64-digest map.
- Benchmarks are by hand, release-built, never in `cargo test`; tests use small constants (`#[cfg(test)]`).

### Integration Points
- Workspace `Cargo.toml` members list: the new crate and the bench crate are added there (one plan per wave, D-05).
- None into the product: no change to `metadata`, `slicefs-cli`, `chunk-store` source (D-01). If the bench needs a non-public metadata function, it copies the logic into the bench crate instead of changing `metadata`; a tiny `pub` addition in `metadata` is allowed only if the copy is not possible, and the plan says why.

</code_context>

<specifics>
## Specific Ideas

- Follow Dolt's size-aware chunker for the boundary probability (issue #19 "Node size spread").
- Report the cost curve against the number of changed files (1, 10, 100, 1000) and the large version change (1_84_0 -> 1_85_0), not only one number.
- Write amplification (#19): report nodes and bytes written per commit for the prototype (about depth nodes per changed leaf) next to the current FastCDC-cut map rewrite.

</specifics>

<deferred>
## Deferred Ideas

- Diff of two roots that skips equal subtrees (for sync, replication, incremental backup): a later phase if prolly maps are adopted.
- Prolly trees for file manifests with subtree lengths: not needed; manifests are already content-cut trees with byte lengths (D-25).
- Wiring a prolly map into the root record, GC mark, rehash and snapshots: only after an owner decision (Q-1..Q-3), in a later milestone.
- A combined namespace map (Q-2).

</deferred>

---

*Phase: 24-prolly-evaluation*
*Context gathered: 2026-11-26*
