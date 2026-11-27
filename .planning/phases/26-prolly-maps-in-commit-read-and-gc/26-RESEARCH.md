# Phase 26: Prolly maps in commit, read and GC - Research

**Researched:** 2026-11-27
**Domain:** Rust metadata store (`crates/metadata`, `crates/prolly`, `crates/slicefs-cli`): replace five in-RAM `BTreeMap`s by prolly trees in the commit, read, GC, rehash and bench paths; crash tests; benchmark against flat release `v2.3.0`
**Confidence:** MEDIUM-HIGH (code facts are read this session; the performance outcome of PMAP-07 and the concurrency design are not measured yet)

<user_constraints>
## User Constraints (from CONTEXT.md)

### Locked Decisions

**On-disk format (owner decisions, 2026-11-27)**
- Directory entries are stored by name in the `dir_data` tree: key = directory ino (8 bytes, big endian) || entry name; value = child ino + file type. One change in any directory touches O(tree height) nodes; readdir is a range scan from the directory prefix; the per-directory list object goes away. This replaces the `dir_data` row of the layout-a key grammar in `docs/design/store-format.md` (Phase 25 left room for it). — **Reversibility:** one-way — it is part of store format 8; undo needs a format change and a store copy (Phase 27 path).
- Small values are stored inline in the prolly leaves: the inode record goes inline in `inode_data`; an xattr set goes inline in `xattr_data` when the entry fits `MAX_ENTRY` (2,048 bytes) and spills to a separate object (value = `ObjectKey`) when it does not; `manifest_data` values stay 33-byte `ObjectKey`s (a manifest is a content tree). `refcounts` keep content key → 8-byte count. This removes about one chunk-index entry and one read per inode lookup. — **Reversibility:** one-way — part of store format 8; undo needs a format change and a store copy.
- `docs/design/store-format.md` § "Metadata maps (store format 8)" is updated with the new key grammar and value encodings (with a version or tag byte where an inline value and a spilled value can both occur).

**Carried from Phase 25 (locked)**
- Layout a (five trees), constants L=8192 / NODE_MAX=16384 / NODE_MIN=512 / MAX_ENTRY=2048 / MAX_KEY=1024, integer boundary rule with `RULE_VERSION` 1, optional boundary salt, store format 8, 197-byte root record with five map keys and `next_ino`.
- Design target 1B+ inodes; counts from internal entry counts or the root record, never a full leaf walk.
- Online GC stays mandatory; FSKit is the only macOS backend; mounts only in Docker for tests.

**Decided autonomously (reversible, internal)**
- [Read path] 'read on demand from prolly nodes through a bounded node cache; dirty entries stay in memory until commit; no whole map in RAM'.
- [Read path] node cache: 'fixed byte cap, default 64 MiB, mount option to change it, counted in the existing memory caps'.
- [Commit] 'per tree, sort the dirty keys, one apply batch, write new nodes, then the root record with the five new roots'.
- [Commit] stores written by the interim Phase 25 build (format 8 header, flat maps): 'refuse cleanly at mount with a message (review IN-02), not a size error; also fix IN-01 (read next_ino after take_all) and IN-05 (bench --l default 8192)'.
- [GC] 'a stack of one node path per tree, for the live root and each snapshot; a test caps peak memory on a large map'.
- [Crash tests] 'test-only fault point after each node write and each sync step, then replay and check the tree'.
- [Bench] 'a generator that writes through the product metadata API; flat baseline with a v2.3.0 binary built in a separate worktree'.

### Claude's Discretion
Encoding details of the inline values (field order, tag byte), cache eviction policy, the exact fault-point list, the mitigation for the insert/delete-heavy regression (or a written acceptance), and how hard links appear in `dir_data` (each name is its own entry pointing to the same ino).

### Deferred Ideas (OUT OF SCOPE)
- Carried from Phase 25: salt rebuild command; 100M/1B measured runs; global namespace identity; snapshot diff by equal subtrees.
- Owner question from 25-07 (open, does not block): allow a tolerance (for example 5%) on the 1B node-count rule if the layout is ever decided again.
- Not in this phase: store migration (Phase 27), crash/soak tests of a live mount (Phase 28), scrub report (Phase 29), refcount removal (#165).
</user_constraints>

<phase_requirements>
## Phase Requirements

| ID | Description | Research Support |
|----|-------------|------------------|
| PMAP-03 | The commit and read paths use prolly maps; a commit with one change does work proportional to the change | Prolly crate gaps (cache, range seek, ids visitor) in Wave 0; overlay + working root + commit protocol; counter test (`ApplyStats`); see "Architecture Patterns" and "Recommended Plan Split" |
| PMAP-06 | GC mark reaches all prolly nodes with bounded memory; online GC keeps working | Mark design: prolly node IDs go straight to `add`, only file-values go to the sorter; `visit` API; memory-cap test pattern; online GC window tests |
| PMAP-07 | Phase 24 benchmark on product code: one-change commit at 1M+ inodes <= 1/5 of flat; Boost within rubric; insert/delete regression recorded | `commit_bench.rs` is unchanged since `v2.3.0` and builds in both trees; pre-registered metric and platform; risk that sync dominates on macOS |
| PMAP-08 | Crash tests: a commit cut at any point replays to a consistent tree | Existing window harness `write_log_replay_tests.rs`, `CommitFault`, `MemIo`; new fault points and a tree validator |
</phase_requirements>

## Summary

Today a commit is O(store size): `begin_commit` clones five whole `BTreeMap`s under the exclusive gate, and `finish_commit` re-serializes five flat maps with `intern_u64_digest_map` / `intern_key_u64_map` [VERIFIED: crates/metadata/src/store.rs `begin_commit`, `finish_inner`]. The prolly crate (Phase 25) already has `build`, `get`, `iter`, `walk` and a batch `apply` that rewrites only the touched path and resyncs, with `ApplyStats {nodes_read, nodes_written, bytes_written, ...}` [VERIFIED: crates/prolly/src/apply.rs]. A one-change commit at depth 3 writes 3 nodes and reads 6 [CITED: docs/research/prolly-trees.md:303-304]. The crate is a prototype in four ways that matter for this phase: it reads straight from `&ChunkStore` with no cache and no seam for one; its only cursor starts at the first entry (no seek, so no readdir range scan); `walk` reports `NodeInfo` with no node ID and no leaf values (the GC mark needs both); and nothing in the `metadata` crate depends on it yet (`grep prolly crates/*/Cargo.toml` finds only the crate's own manifest).

The recommended design: each of the five maps becomes a `MetaTree` = a working root `ObjectId` plus a bounded dirty overlay (sorted map of pending puts/deletes with tickets, as `Live<T>` has today). Hot-path flushes (`DIRTY_FLUSH`) apply the overlay to the working root; a commit applies the batch taken at the cut to the working root as it was at the cut, writes the 197-byte root record with five new root IDs, syncs, then logs the root. The `apply` of one root at a time rule (prolly crate doc, "one `apply` per root at a time") is kept by making flushes skip while a commit is prepared. Nodes come from a bounded decoded-node cache that lives in the prolly crate (Node is crate-private). The GC mark gets a new walker: prolly node IDs are added to the live set directly (they are raw `SPT1` objects and `for_each_file_object` only decodes `SFM1`); only manifest keys, spilled xattr keys and the root record go through the external sorter.

The riskiest parts are, in order: (1) the working-root/overlay concurrency protocol (lock order with lazy node reads; every read-modify-write that today holds a map mutex across the whole operation); (2) the `dir_data` by-name change (readdir, `.`/`..`, hard links, rename across directories, `delete_inode` of a directory); (3) the rehash engine, which byte-copies flat maps today and must rebuild trees (child IDs change with the hash); (4) PMAP-07 itself: sync dominates a small commit on macOS, so the 1/5 target must be pre-registered for a platform and a metric before any run; (5) a long tail of consumers that break `cargo test --workspace` (prolly-bench smoke, scrub, stats, header/refusal). Plan a tracer slice on `manifest_data` (a plain `u64 -> ObjectKey` map) that goes through every layer, then widen.

**Primary recommendation:** Wave 0 extends `crates/prolly` (cache, seek/range, node visitor, O(1) count). Wave 1 is a tracer on `manifest_data` end to end (overlay, working root, commit, load, GC mark, rehash primitive, crash windows, counter test). Waves 2 to 4 convert refcounts and xattrs, inodes, then directories. Wave 5 closes the commit protocol and crash/GC tests. Wave 6 measures and documents. Pre-register the PMAP-07 rule first.

## Project Constraints (from CLAUDE.md)

No `./CLAUDE.md` or `./.claude/CLAUDE.md` exists in the repo [VERIFIED: `ls CLAUDE.md .claude/CLAUDE.md` found neither]. The binding directives come from the owner's memory and the phase brief (treat as locked):

- No whole metadata map in RAM (1B+ inode design target); per-file daemon memory bounded.
- No store backward compatibility (no deployments): format changes need not stay readable by old releases; interim Phase 25 stores are refused, not read.
- Tests that mount run only in Docker (`scripts/docker-linux-test/run.sh`, OrbStack); the Boost mount run only by hand, on main, or on `ci/**`; short waits in tests.
- `cargo test --workspace` must pass at each plan end.
- macOS backend: FSKit only. No SMB/NFS fallback.
- Public GitHub text (issues, PRs) follows the owner's content rules. No push, no `gh` writes in this phase.
- Parallel or background agents get their own git worktree, never the shared main tree. The v2.3.0 baseline build goes in a separate worktree too.
- A huge `target/debug/deps` slows test start (840k files = 16 s per binary): `cargo clean` before suspecting a hang. Current `target/` is 8.0 GB.
- Safety gates (clippy with unsafe count, loom/Miri/TSan scripts in `scripts/safety/`) are a release gate, not per-PR; the phase gate of Phase 25 ran `RUST_STABLE=stable scripts/safety/clippy.sh` and Docker `all`.
- Reversible technical choices: decide and report; one-way doors go to the owner. The one-way doors found here are in "Open Questions".

## Architectural Responsibility Map

| Capability | Primary Tier | Secondary Tier | Rationale |
|------------|-------------|----------------|-----------|
| Node format, boundary rule, apply, range scan, node cache | `crates/prolly` | `crates/chunk-store` (object put/read) | Node is crate-private; the cache must sit where nodes are decoded |
| Five maps as trees: overlay, working root, flush, commit apply, root record | `crates/metadata` (`store.rs`, new `pmap` module) | prolly | The store owns locks, tickets, `commit_lock`, WAL cut/install |
| Name -> ino lookup, readdir, emptiness, `..` | `crates/metadata` (`directory.rs` replaced) | fs layer (`filesystem.rs` stripes) | Entry change atomicity is the fs stripe; entry storage is the store |
| Namespace/rename ordering, `check_not_ancestor` | `crates/slicefs-cli/src/filesystem.rs` | metadata `lookup(.., "..")` | Unchanged; relies on `..` being a real entry |
| GC mark of prolly nodes | `crates/metadata/src/gc`, `store.rs::for_each_referenced` | chunk-store `exact_mark`/`live_filter` | Mark already owns the roots and the sorter |
| Rehash (key translation, tree rebuild) | `crates/metadata/src/rehash` | `extsort` of chunk-store | Child IDs change with the hash, so trees are rebuilt |
| Salt of the boundary hash | `crates/slicefs-cli` (`header.rs`, `content.rs`) | metadata (receives it) | Header holds it; nothing in `metadata` reads it today [VERIFIED: grep "salt" crates/metadata/src finds nothing] |
| Crash-window harness, benchmark | tests in `slicefs-cli`/`metadata`; `benchmarks/prolly-bench`; `scripts/` | Docker | Mounts only in Docker |

## Standard Stack

### Core
| Library | Version | Purpose | Why Standard |
|---------|---------|---------|--------------|
| `prolly` (in-workspace) | 0.1.0 path crate | Tree build/get/apply/walk | Phase 25 format of record; `Config::DEFAULT` pinned, goldens pass macOS and Linux |
| `chunk-store` (in-workspace) | path | `put`/`read`/`sync`, `gc_begin`, `exact_mark`, `extsort`, `MemIo` | Nodes are chunk-store objects; `MemIo` gives crash simulation |
| `metadata` (in-workspace) | path | Store, WAL, GC, rehash | Add `prolly = { path = "../prolly" }` to its manifest |

### Supporting
| Library | Version | Purpose | When to Use |
|---------|---------|---------|-------------|
| `blake3` (workspace) | already used by prolly | Boundary hash | No change |
| `tempfile`, `proptest` (dev) | already used | Tests | Existing pattern |

### Alternatives Considered
| Instead of | Could Use | Tradeoff |
|------------|-----------|----------|
| Node cache in the prolly crate | Cache of raw bytes in `metadata` behind a `NodeStore` trait | Raw-byte cache still re-decodes a 7-8 KB leaf (about 100 entries, 200 allocations) on every `lookup`; decoded `Arc<Node>` avoids it. A trait seam is only needed for tests |
| Working root + flush | Only commit applies; no mid-run flush | Dirty overlay would grow with the writes between commits (unbounded RAM for a bulk copy); violates the no-whole-map rule at scale |
| Prolly node IDs in the sorter | Direct `add` | Sorter path calls `for_each_file_object`, which requires `SFM1`; see Pitfall 3 |

**Installation:** no new external package. `crates/metadata/Cargo.toml` gains one path dependency on `prolly`.

**Version verification:** not applicable (workspace crates only). Toolchain on this host: `~/.cargo/bin/rustc` is `rustc 1.95.0-nightly (9e79395f9 2026-02-10)` [VERIFIED: ran it]; `cargo` is not on the default `PATH` of a fresh shell, use `~/.cargo/bin`.

## Package Legitimacy Audit

No external package is installed or recommended by this phase. Packages removed due to `[SLOP]`: none. Packages flagged `[SUS]`: none. The only manifest change is a path dependency between workspace crates. (No `gsd-tools package-legitimacy` run is needed; there is nothing to check.)

## Architecture Patterns

### System Architecture Diagram

```
                    FUSE request (stripes, cells, shared commit gate)
                                    |
        lookup / readdir / getattr / create / link / unlink / setattr / xattr / manifest / refcount
                                    |
                                    v
        +--------------------- MetadataStore (DictMetadataStore) ----------------------+
        |  live caches (sharded, Rank::Store)       overlays (dirty edits + tickets)   |
        |   inode_live: ino -> InodeMeta            dir edits: dir ino -> {name -> Some((ino,type))|None}
        |                                           manifest/xattr/refcount overlays   |
        |       read  = overlay (dirty, in-flight)  then tree at working root          |
        |       write = overlay only (no node I/O under a shard or overlay lock)       |
        +-----------+--------------------------------------------+---------------------+
                    | flush (>= DIRTY_FLUSH, try_lock; skipped    | commit
                    |  while a commit is prepared)                | begin_commit (exclusive gate):
                    v                                             |   WAL cut, take dirty, read 5 roots
          apply(working root, sorted batch)                       | finish_commit (gate released):
                    |                                             |   apply(cut root, taken batch) x5
                    v                                             |   write root record (197 B)
        +----------------------+                                  |   install roots, settle tickets
        |  prolly Ctx          |<---- get / range / visit --------+   cs.sync(), pre_root_sync
        |  NodeCache (64 MiB)  |                                      WAL install_root, last_root
        +----------+-----------+
                   | miss                       GC mark (online or offline)
                   v                              root record -> 5 tree roots
        ChunkStore::read / put (verified,         prolly visit: node IDs -> add(id) direct
        deduplicated, packed, synced by commit)   leaf values (manifest key, spill key) -> sorter -> for_each_file_object
```

### Recommended Project Structure
```
crates/prolly/src/
├── cache.rs        # NodeCache (byte-capped, decoded Arc<Node>), Ctx { store, cache }
├── range.rs        # seek + range cursor (readdir), first/last, count (O(1) from internal entries)
├── visit.rs        # visit(ctx, root, on_node(id, level, entries/children)) for GC and rehash
└── (apply.rs, tree.rs, node.rs unchanged except they take a Ctx)
crates/metadata/src/
├── pmap.rs         # MetaTree {root cell, gen}, Overlay<K,V> with tickets, flush/commit helpers
├── dirent.rs       # dir_data key/value encoding (replaces directory.rs DirList)
├── store.rs        # five MetaTrees; begin/finish_commit; root record of five tree roots
├── gc/mod.rs       # mark: new tree walker
└── rehash/         # tree rebuild primitive (value translator per map)
```

### Pattern 1: Working root + overlay (the core primitive)
**What:** each map has `root: Mutex<ObjectId>` (a 33-byte value; brief lock), `gen: AtomicU64` (bumped at every root install), and an overlay `BTreeMap<K, Option<V>>` per shard with the same ticket scheme as `Live<T>` (`dirty`, `flushing`, `tickets`, `settle`) [VERIFIED: crates/metadata/src/store.rs `Live<T>`]. A read checks the overlay (dirty, then in-flight), then reads the tree. A write touches the overlay only.
**Read order rule (no lost update):** on an overlay miss, load the root after the miss (install the root first, evict the overlay entry after). Never load the root first and check the overlay after.
**Read-modify-write (refcount inc/dec, xattr set):** a shard or overlay lock may not be held across a node read (field docs of `dir_live`/`inode_live` forbid it). Do: lock, overlay hit -> modify and done; miss -> record `gen`, unlock, read the tree, lock, recheck the overlay (another thread may have inserted) and `gen` (a flush may have moved the value); retry on change. This replaces `d == digest` in `cached_inode`/`cached_dir` and the `old` digest compare in `change_xattrs`, which have no digest once values are inline.
**When to use:** all five maps.

### Pattern 2: Commit protocol (replaces five clones)
1. `begin_commit` (exclusive gate, `commit_lock`, then `apply_lock` of Rank::Store acquired in rank order): WAL `cut()`; read the five working roots (`cut_roots`); `take_all` the dirty and in-flight edits of every overlay and live cache with new tickets; read `next_ino` after `take_all` (review IN-01); copy `logical_bytes` and `inode_count`. This is O(dirty), not O(store).
2. `finish_commit` (gate released, `apply_lock` still held by `PreparedCommit`): per tree sort the taken batch and `apply(cut_root_i, batch_i)`; write the root record with the five new roots (`put_node`, a file value, 197 bytes unchanged); install `root_i := new_i`, bump `gen`, settle tickets; `cs.sync()`, `pre_root_sync`, `install_root`; set `last_root`.
3. Flushes (hot path) use `try_lock` on `apply_lock` and skip while it is held. The working root at the cut therefore equals `cut_roots` at install, and no post-cut edit can reach the committed root. Post-cut edits live only in overlays until the next flush or commit.
4. Failure matrix: an error before install (apply or root record put) requeues the taken tickets of **all five** maps (today `Drop` requeues only `inode_live` and `dir_live` [VERIFIED: store.rs `impl Drop for PreparedCommit`]); an error after install (sync, `pre_root_sync`, `install_root`) needs no requeue: the state is in the working root, and the next commit takes an empty batch, writes the root record again, and syncs. `CommitFault` gains points (see Validation Architecture).
**Why not apply onto "the latest working root":** two `apply` calls on one root fork the lineage; a post-cut flush would then put half of a rename (link done, unlink not yet) into a committed root. The exclusive-gate cut is what gives consistency; skipping flushes inside the prepare window preserves it.

### Pattern 3: dir_data by name (owner decision)
Key = `dir_ino (8 BE) || name bytes`; value = `child ino (8, LE per existing inode.rs style) || type (1)`. Longest key 8 + 255 = 263 bytes < `MAX_KEY` 1,024 [VERIFIED: crates/prolly/src/lib.rs:157-161 `pub const MAX_KEY: usize = 1_024;`, and NAME_MAX 255 in store.rs]. `lookup` = point `get`; `link` = overlay put (after a duplicate check against overlay then tree); `unlink` = overlay delete; readdir = range scan `[dir||"", (dir+1)||"")` merged with the overlay for that dir; rmdir/rename emptiness = "does the range hold anything besides `.`/`..`" (one cursor step), not `list_directory().len()`. Rename across directories stays what it is today: `link(new)`, `unlink(old)`, `set_parent` under fs stripes and the rename mutex [VERIFIED: filesystem.rs `rename_held`]; the metadata store sees three independent entry edits, all committed together because the cut is atomic under the exclusive gate. A hard link is one more entry (new dir, new name, same ino); the type byte never changes for an ino.
Reuse `Live<T>` with `T = DirEdits` (`BTreeMap<name, Option<(ino,type)>>` per directory) so the shard/ticket/settle code is unchanged: key stays the directory ino, a take clones only the edits of that directory.

### Pattern 4: GC mark of prolly trees
`for_each_referenced` today emits `Ref::Node` / `Ref::Content` keys into the external sorter, and `mark()` expands each sorted key with `for_each_file_object` [VERIFIED: crates/metadata/src/gc/mod.rs `mark`]. `for_each_file_object` calls `load_node`, which decodes `SFM1` only [VERIFIED: crates/chunk-store/src/file.rs `decode_node`, `load_node`]. A prolly node is a raw `store.put` object with magic `SPT1` [VERIFIED: crates/prolly/src/lib.rs:166-167 `pub const NODE_MAGIC: [u8; 4] = *b"SPT1";`]. So: a new variant `Ref::Tree(ObjectId)` is passed to `add` directly (ExactMark and LiveFilter both accept unbounded streams and duplicates); `Ref::Content` (manifest keys from `manifest_data` leaves, spilled xattr keys, refcount-free) and the root record key stay on the sorter path. Memory: one path of nodes per tree (the `walk` recursion holds a path), plus the existing sorter chunk. `check_temp_space` / `referenced_estimate` must stop reading flat map lengths: use `prolly::count` (sum of child counts at the root node, O(1)) per tree. Stop and pacer: call `stop()` and `pacer.at_safe_point()` every `WALK_STOP_EVERY` nodes.
Cost note for snapshots: each root is walked in full. N snapshots that share almost all nodes cost N times the node count in reads. Bounded memory forbids a visited-set; lockstep diff against the previous root ("snapshot diff by equal subtrees") is deferred by the owner. Record the O(roots x nodes) cost in the plan as an accepted limit and measure it once.

### Pattern 5: Rehash by tree rebuild
Rehash byte-copies the flat maps because they hold no content address in their bytes, and translates only key-carrying nodes [VERIFIED: crates/metadata/src/rehash/mod.rs module doc]. Prolly nodes carry child `ObjectId`s, so every node ID changes with the hash. Rebuild: read each tree with a cursor in key order, translate leaf values that carry keys (manifest key, spilled xattr key), and feed the target-hash `ChunkStore` through `prolly::Builder`. Keys of four trees are inos or names, so order is unchanged and no sort is needed. `refcounts` keys are content keys: their order changes with the hash, so reuse the existing external sorter path (rows sorted by new key, equal new keys merged) and feed the sorted rows to a `Builder`. Boundaries depend only on keys and entry lengths and `ID_LEN` is 33 for every hash, so the rebuilt tree has the same shape rules. The `KeyMap` stays. Introduce one generic `translate_tree(root, value_translator)` in Wave 1 and add a translator per map in later waves.

### Pattern 6: Salt plumbing
`Config::DEFAULT.with_salt(Option<[u8;32]>)` exists; the salt is a header line read by `slicefs-cli` and nothing in `metadata` receives it. Every writer of a tree (seed, mount read-write, snapshot create/switch, replay, rehash target) must pass the store's salt, or `apply` will cut with a different rule than the old nodes (still valid, but not canonical). Add the salt to `StoreIo` (all stores open through `content::store_io(store)`) and read it from the header there.

### Anti-Patterns to Avoid
- **Holding a shard or overlay lock across a node read** (rule in the field docs of `dir_live`, `inode_live`). On-demand reads make this easy to break: `insert_inode(at)` and `is_dir` probe maps under a shard lock today.
- **A second apply on the same root** (see Pattern 2, point 3).
- **A global `Mutex<BTreeMap>` for the refcount overlay** with node reads inside it.
- **Reading `next_ino` before `take_all`** (IN-01).
- **Recursing into stats `content_stats` to "fix" no-whole-map**: it builds `HashSet<u64>` and `HashMap<Vec<ObjectKey>, ..>` over all inodes today (pre-existing O(files) RAM). Keep behavior, note it, do not widen the phase.

## Don't Hand-Roll

| Problem | Don't Build | Use Instead | Why |
|---------|-------------|-------------|-----|
| Batch edit of a sorted map with moving boundaries | Own insert/delete splitter | `prolly::apply` | Resync rules and the canonical root are already tested (goldens) |
| Sorted dedup of unbounded keys for the mark | A `HashSet<ObjectKey>` | `ChunkStore::gc_sorter`, `exact_mark`, `live_filter` | Bounded RAM, already used by the mark |
| Crash simulation | Own file mock | `chunk_store::mem::MemIo` (`crash`, `fail_sync_after`, `fail_write_durable_after`) | Keeps un-synced bytes out after `crash()` |
| Commit windows with a real log | New harness | `write_log_replay_tests.rs` `Window` and `cut_window_scenario` | AfterCut, AfterNodes, AfterSyncs, AfterRoot, RootTorn, GcDuringFinish exist |
| Peak-heap measurement | Sampling `/proc` | counting `GlobalAlloc`, one test per binary (pattern of `replay_memory_tests.rs`, `gc_full_memory_tests.rs`) | Exact, deterministic |
| Lock-order checking | Comments | `chunk_store::sync::{Mutex, Rank}` with debug tracking | Panics on a rank violation in debug builds |

**Key insight:** the correctness-critical parts (tree rewrite, GC sets, crash mock, lock ranks) already exist. The phase is wiring plus three prolly-crate additions; hand-rolled variants of those would re-open Phase 24/25 results.

## Runtime State Inventory

> This is a format change (flat maps -> trees) with a refusal rule, so the question "what still holds the old shape" applies.

| Category | Items Found | Action Required |
|----------|-------------|------------------|
| Stored data | Stores written by the interim Phase 25 build on `develop`: `FORMAT` line says format 8 and the root record is 197 bytes, but the five fields point to flat map files. `check_root_len` cannot tell them apart [VERIFIED: store.rs `check_root_len` compares only the length]. Discriminator: the object at each root field starts with `SFM1` (flat map = file value, whose root is always a manifest node [VERIFIED: file.rs `finish` returns only `put_node` results or a higher-level node]) versus `SPT1` for a tree | Code edit: at load, read the five root objects (or just the first) and refuse a non-`SPT1` root with a new M-numbered message (IN-02). No data migration (no deployments, no backward compat) |
| Live service config | None. No external service holds metadata names | None, verified by reading `store.rs`, `mount.rs`, `snapshot.rs` |
| OS-registered state | None for the format. Docker volumes `slicefs-target-25-02/-06/-07/-08`, `slicefs-prolly-work` may hold dev stores or build output [VERIFIED: `docker volume ls`] | Do not reuse a store from those volumes for Phase 26 tests; use fresh stores. Cargo target volumes are build output only |
| Secrets/env vars | None. The boundary salt is a plain header line, not a secret [CITED: docs/design/store-format.md "Boundary salt"] | None |
| Build artifacts | `target/` 8.0 GB; the v2.3.0 baseline needs its own worktree and its own `CARGO_TARGET_DIR` so the two builds do not share outputs (`SLICEFS_TARGET_VOL` for Docker) | Plan the worktree and volume names in Wave 6; `cargo clean` rule from the owner memory |

Also: `benchmarks/prolly-bench` `batches`, `maps` and `flat` read flat map nodes of product stores by root-record offsets [VERIFIED: benchmarks/prolly-bench/src/main.rs `RR_*`, `MAP_ENTRY`]; they stop working on a prolly store (see Pitfall 9).

## Common Pitfalls

### Pitfall 1: Lost update on a cold read racing a flush
**What goes wrong:** a cold read caches a value from the tree, a flush moves the tree and evicts the overlay entry, the cache then holds a stale value (or an RMW computes from a stale base).
**Why:** inline values have no digest to compare; today's `d == digest` check goes away.
**How to avoid:** `gen` per tree, read before the tree read, compared under the shard/overlay lock before caching or writing. Install the root before evicting the overlay.
**Warning signs:** `atomic_modify_inode_loses_no_change`, `flush_install_does_not_resurrect_a_deleted_inode`, `late_older_install_does_not_overwrite_a_newer_change` (existing tests in store.rs) fail or flake.

### Pitfall 2: Post-cut edits in the committed root
**What goes wrong:** a hot-path flush applies to the working root between `begin_commit` and the root install, so the committed root has half of an operation.
**How to avoid:** Pattern 2 point 3 (skip flushes while prepared). Test: the `cut_window_scenario` requests after the cut include `rename` and `fsync`; add an assertion that the committed root equals the state at the cut.

### Pitfall 3: Prolly node ID through the file-value sorter
**What goes wrong:** the mark emits a node ID into the sorter; `for_each_file_object` fails with `Corrupt("manifest node ...")`, the GC aborts.
**How to avoid:** `Ref::Tree` bypass (Pattern 4). Test with a store where every tree has depth 3.

### Pitfall 4: Online GC and the unflushed working root
**What goes wrong:** nodes written by hot-path flushes before `gc_begin` are not in the last committed root.
**How to avoid:** the caller already does `gc_begin`, then a commit, then marks from the new root [CITED: crates/metadata/src/gc/mod.rs `OnlineGc` doc: "first calls `ChunkStore::gc_begin` ..., then takes the roots (a commit ...)"]. Keep that order; the commit flushes everything into the root. Nodes put after `gc_begin` are kept by the chunk store.

### Pitfall 5: Cache hides reads from the GC touched set
**What goes wrong:** a node that is only read through the cache is not "read" for the chunk store.
**How to avoid:** every node a later request needs is reachable from a root that is marked (the live root or a snapshot root); the cache holds immutable content-addressed nodes, so a cached node of a swept object is never requested. Do not cache nodes of roots that are not anchors.

### Pitfall 6: `apply` shrink leaves orphans
A shrink of the height may write nodes that the collapse then skips [CITED: crates/prolly/src/apply.rs module doc]. They are garbage until GC, which is expected; a test that counts objects before/after GC must allow them.

### Pitfall 7: Saturating refcounts and O(map) accessors
`saturated_refcount_count` scans the whole refcount map today; scrub calls it. On a tree it is a full leaf walk with one cursor (bounded memory, but not O(1)). It is a scrub-time scan, not a mount-time count: allowed, but name it.

### Pitfall 8: Per-flush skew in tests
Unit tests set `DIRTY_FLUSH = 8` and `INODE_CACHE_MAX = 16` under `cfg(test)` so the flush path runs [VERIFIED: store.rs lines 60-75]. Keep these constants small in test builds for every new overlay, or the flush path is never exercised.

### Pitfall 9: Workspace tests that read flat maps
`benchmarks/prolly-bench/tests/smoke.rs` runs `whole` then `batches` on a product store and is part of `cargo test --workspace`; `scrub.rs`, `stats.rs`, `rehash` tests, `gc` tests and `hash_change_tests.rs` call `referenced_roots`, `content_keys`, `load_from_root` on product stores. Each wave must keep these green; list them in the plan's "must still pass" lines.

### Pitfall 10: Name order differs from hash order
`DirList` orders by BLAKE3 of the name, the tree by name bytes. Any test or tool that compares a `list_directory` result to a literal order (not sorted) can break. FUSE `readdir` uses an index offset (`skip(offset)`), which stays valid for a quiet directory.

## Code Examples

### Counter assertion for success criterion 2
```rust
// One change, two store sizes, per tree: nodes read/written must grow with height, not N.
// Source: ApplyStats in crates/prolly/src/apply.rs (nodes_read, nodes_written)
let (s_small, h_small) = one_change_stats(10_000);
let (s_large, h_large) = one_change_stats(1_000_000);
for t in TREES {
    assert!(s_large[t].nodes_read    <= 3 * h_large[t]);            // [ASSUMED] bound, pin after measuring
    assert!(s_large[t].nodes_written <= 2 * h_large[t] + 1);        // [ASSUMED]
}
assert!(total(&s_large).nodes_read as f64 <= total(&s_small).nodes_read as f64 * (h_large_sum as f64 / h_small_sum as f64) * 1.5);
```
The sizes need trees built with `prolly::build` or a test-only bulk loader, not 1M product `create_inode` calls. Measured reference: depth 3, one change: 3 written, 6 read [CITED: docs/research/prolly-trees.md:303-304].

### Interim-store refusal
```rust
// In load_from_root, per tree root field: read the object, check the magic.
let bytes = cs.read(&root_id)?;               // ChunkStore::read, hash-verified
if bytes.get(..4) != Some(&prolly::NODE_MAGIC[..]) {
    return Err(MetaError::Corrupted(M_INTERIM_FLAT_MAPS.into()));  // new M-numbered text, header.rs style
}
```

### Hot-path flush that skips during a commit
```rust
if let Some(_g) = self.apply_lock.try_lock() {   // sync::Mutex::try_lock exists [VERIFIED: chunk-store sync.rs:499]
    self.flush_trees();                          // take dirty (ticketed), apply, install, settle
}                                                // else: leave the entries dirty; the commit takes them
```

## State of the Art

| Old Approach | Current Approach | When Changed | Impact |
|--------------|------------------|--------------|--------|
| Five `BTreeMap`s in RAM, cloned at the cut, serialized whole per commit | Five prolly trees; overlay of dirty edits; apply of the batch | This phase | Commit O(change); no O(N) clone under the exclusive gate |
| `DirList` per directory (one object, hash-ordered, whole list rewritten per change) | Entry per name in `dir_data` | This phase (owner decision) | One name change touches one path; readdir is a range scan |
| Inode digest map + one object per inode | Inline record in the `inode_data` leaf | This phase (owner decision) | One chunk-index entry and one read less per inode |
| Float boundary test | Integer table, `RULE_VERSION` 1 | Phase 25 | Same cuts on every platform |

**Deprecated/outdated after this phase:** `intern_u64_digest_map`, `load_u64_digest_map`, `intern_key_u64_map`, `load_key_u64_map`, `MAP_ENTRY`, `for_each_map_value`, `map_size`, `directory.rs` (`DirList`, `add_dir_entry`, ...), `msg_flat_maps` stays (it is for format 7).

## Recommended Plan Split (waves; tracer first)

> Each wave ends with `cargo test --workspace` green. Waves are serial unless noted. One plan = one commit group; a worktree per agent.

**Wave 0 - Prolly crate additions (no product change).**
- 26-01: `NodeCache` (byte cap, decoded `Arc<Node>`, content-addressed, one internal mutex, no callbacks out while held) and `Ctx`; `get`, `iter`, `walk`, `apply`, `Builder` read through it (old signatures stay as wrappers over an uncached Ctx so Phase 24/25 tests and the bench do not move). Update the "no node cache (D-17)" doc text: D-17 of Phase 24 forbids a cache that grows with the map and allows a fixed-size one [CITED: .planning/milestones/v2.2-phases/24-prolly-evaluation/24-CONTEXT.md:52].
- 26-02: `range`/`seek` cursor (start/end bounds; per-node parent-bound checks as `Cursor`), `count(root)` from the root's child counts, `visit(root, f)` giving node ID, level, and for leaves the entries; `apply` returns nodes read/written logically (cache hits count) so the counter test is meaningful.
- Fix IN-05 (bench `--l` default 8192) here (one line in `benchmarks/prolly-bench/src/layout.rs`).

**Wave 1 - Tracer: `manifest_data` through every layer (the pattern all later waves copy).**
- 26-03: `pmap.rs` (`MetaTree`, `Overlay`, tickets, `gen`), `StoreIo` carries salt and the node cache, `Cargo.toml` dependency. `manifest_data` becomes a tree; the other four fields of the root record stay flat in this wave (root record layout is unchanged: 197 bytes). New `begin_commit`/`finish_commit` shape for that one tree; `set_manifest_digest`/`manifest_digest`/`delete_inode` through the overlay.
- 26-04: GC mark `Ref::Tree`, `referenced_estimate` via `count`, memory-cap test binary (counting allocator, two sizes 4x apart, pattern of `gc_full_memory_tests.rs`), online GC test (nodes unreachable freed, live kept), rehash `translate_tree` for manifest values (+ rehash tests), scrub and stats keep working.
- 26-05: counter test (criterion 2) for this tree, `CommitFault` points per apply/root record, crash window tests with a tree validator. This plan decides whether the working-root protocol holds before four more maps depend on it.

**Wave 2 - Small maps.**
- 26-06: `refcounts` (RMW with saturation and delete at 0; rehash with the existing external sorter into a `Builder`; `saturated_refcount_count` as a cursor scan).
- 26-07: `xattr_data` (inline when `8 + 2 + 2 + set bytes` fits `MAX_ENTRY`, else spill; tag byte; `change_xattrs` as overlay RMW; rehash translator for spilled keys).

**Wave 3 - Inodes.**
- 26-08: `inode_data` inline record (reuse `serialize_inode`/`deserialize_inode`, 56 or 72 bytes [VERIFIED: crates/metadata/src/inode.rs `deserialize_inode` accepts 56 or 72]); `inode_live` flush becomes overlay flush; `cached_inode` revalidation by `gen`; `insert_inode(at)` probe outside the shard lock; `delete_inode`; `inode_count` and `logical_bytes` stay atomics persisted in the root record.

**Wave 4 - Directories and the format switch.**
- 26-09: `dir_data` by name (`dirent.rs`, `DirEdits`), `list_directory` via range scan, new `dir_is_empty`-style accessor so rmdir/rename stop listing, `set_parent`, `delete_inode` of a directory (range delete of its entries), `.`/`..` handling per the owner's answer to Open Question 1. fs-layer callers: `filesystem.rs` rmdir (list at 4699-4705 region), `rename_held` (ENOTEMPTY list), `test_readdir`, stats walk.
- 26-10: all five fields are trees: remove the flat code and constants, add the interim-store refusal (IN-02) with its M message in `header.rs`/`load_from_root`, update `docs/design/store-format.md` (key grammar, value encodings, tag byte), `docs/design/metadata-maps.md`, tests that assert the old root record fields (`root_record_layout`, `a_230_byte_root_record_is_refused`, `map_decode_tests`).

**Wave 5 - Close the protocol.**
- 26-11: finish the crash matrix over all five trees (every `CommitFault` point, `MemIo::crash` at each step, both WAL strategies of the existing harness), the validator, GC-during-finish window with prolly, two snapshots sharing nodes + GC, `CommitStats` accessor, IN-01. Docker `all` and `pjdfstest` with `SLICEFS_WORKERS=4` on a prolly store; `verify` for snapshots.

**Wave 6 - Measure and document (PMAP-07).**
- 26-12: pre-register the rule (file in the phase `runs/` folder, written before any run, `decide.py` style); extend `crates/metadata/tests/commit_bench.rs` (N from env, repeated one-change commits, median) and keep it using only `MetadataStore` plus `DictMetadataStore::new/commit`; make `benchmarks/prolly-bench` build and smoke-test on prolly stores (retire or port `batches`/`maps`/`flat`/`prolly` replay of product stores; keep `whole`, `layout`, `synth`, `scale`).
- 26-13: run in Docker (OrbStack): flat baseline from a `git worktree` of `v2.3.0` with its own target volume (the same `commit_bench.rs`, which is byte-identical in both trees [VERIFIED: `git diff v2.3.0 HEAD --stat -- crates/metadata/tests/commit_bench.rs crates/slicefs-traits` printed no line for those two paths]), then the branch; Boost `whole` mode in both trees; insert/delete-heavy rows; record results, mitigation or written acceptance; update `docs/research/prolly-trees.md`; phase gate (`cargo test --workspace`, `RUST_STABLE=stable scripts/safety/clippy.sh`, Docker `all`).

Parallelism: Wave 0 plans can run in two worktrees; Waves 2 (two plans) can run in parallel after Wave 1; everything else is serial because plans edit `store.rs`.

### Riskiest parts (and where each is retired)
1. Lock order with lazy node reads and the RMW paths: retired in Wave 1 on one tree, then copied. Add a loom model of overlay/flush/commit if the owner wants the TSAFE gates to cover it (`scripts/safety/model.sh` runs `loom_` tests of `chunk-store` and `slicefs-cli` only [VERIFIED: model.sh `MODEL_CRATES` default]); new locks also need `Site` entries in `chunk-store/src/lock_stats.rs` and its report tables.
2. Rename across directories, hard links, `delete_inode(dir)`, `check_not_ancestor` (`lookup(cur, "..")`): Wave 4; tests: `rename_ancestor_tests`, `dir_link_tests`, `posix_compliance_tests`, pjdfstest.
3. Snapshot roots sharing nodes: Wave 5 test: create snapshot, change live tree, GC, mount the snapshot read-only, compare; `snapshot_switch_gc_tests`.
4. GC while mounted: the commit-after-`gc_begin` order and the working root; Wave 5 `GcDuringFinish` window on prolly.
5. Crash consistency: Wave 5.
6. PMAP-07: Wave 6, with the pre-registered rule from Wave 0/6.

## Open Questions (one-way doors: owner decides; do not lock in the plan without an answer)

1. **Are `.` and `..` stored entries, and how does an empty directory exist in `dir_data`?**
   - Known: `DirList::new_dir` stores both as entries; `check_not_ancestor` calls `lookup(cur, "..")`; rmdir and rename filter `.`/`..` out of `list_directory`; `is_dir` is "has a `dir_data` key".
   - Unclear: by-name format has no directory key; an empty directory needs some entry.
   - Recommendation: store both `.` and `..` as real entries (same semantics as today; `is_dir(ino)` = point get of `ino||"."`); cost is two entries per directory (about +2 on 100M directories at the 1B target). The alternative (store only `..`, synthesize `.`) saves half of that but changes `lookup(dir, ".")`. This is part of store format 8, so ask.
2. **Where does the entry's file type come from?** `MetadataStore::link(parent, name, ino)` takes no type and `DirEntry` has `name` and `ino` only [VERIFIED: crates/slicefs-traits/src/metadata.rs:53-60 `pub struct DirEntry { pub name: String, pub ino: InodeId }`]. Recommendation: the type byte is `(mode & S_IFMT) >> 12` read from the inode (cached, it was just created) at `link` time; `make_directory` knows it; 0 if the inode is absent. The trait does not change, so the v2.3.0 baseline build stays valid.
3. **Readdir order changes from name-hash to name bytes.** POSIX fixes no order; FUSE offset stays an index. Recommendation: accept; note it in `store-format.md`.
4. **Inline encodings (format 8):** inode value = today's 56/72-byte record with no tag (the length discriminates), dir value = ino (8) + type (1), xattr value = tag byte (0 inline, 1 spill) then the set or a 33-byte key. These are Claude's discretion by CONTEXT, but they are one-way after the freeze; confirm quickly.
5. **Node cache option and its cap.** CONTEXT says the 64 MiB cap is "counted in the existing memory caps". The only existing caps are the dirty-data budget (`--dirty-budget`, default 64M) for file data and the count-based `INODE_CACHE_MAX`/`DIR_CACHE_MAX`; `--cache-size` is accepted but unused [VERIFIED: crates/slicefs-cli/src/mount.rs `_cache_size` doc: "accepted but unused (there is no read cache)"]. Recommendation: a new option (name to the planner, for example `--meta-cache-size`, default 64M) and reuse nothing; Phase 28 soak will read VmHWM against it. Is it additive to the dirty budget? State it in the docs.
6. **Paged readdir and a stable cookie.** `list_directory` returns the whole `Vec` and callers `skip(offset)` [VERIFIED: filesystem.rs `test_readdir`]. A giant directory costs O(n) per call today, and still does. Recommendation: keep the API for this phase (implemented over the range scan), and add the paged API as a stretch item with no format effect.
7. **Is porting rehash in scope?** The phase brief lists rehash among the users to study and there is no requirement ID for it, but `rehash_tests.rs` (889 lines), `hash_change_tests.rs` (334 lines) and in-module tests must pass. Recommendation: yes, as Pattern 5 inside Wave 1 and each later wave.
8. **Accept O(roots x nodes) GC mark for N snapshots?** Recommendation: accept and record; revisit with the deferred snapshot-diff idea.

## Assumptions Log

| # | Claim | Section | Risk if Wrong |
|---|-------|---------|---------------|
| A1 | Skipping flushes (try_lock) while a commit is prepared is acceptable: dirty entries grow for the commit duration only | Pattern 2 | A slow sync (seconds) lets the overlay grow; may need a hard cap that blocks writers |
| A2 | PMAP-07 pre-registered metric: median wall time of `commit()` after one change, N = 1,000,000 inodes, Linux Docker (OrbStack) on disk-backed storage, at least 11 samples, flat from `v2.3.0` on the same host; CPU recorded too | Wave 6 | If sync dominates, the ratio may be above 0.2 on macOS; Phase 24 saw 64.4 ms flat vs 44.6 ms prolly for ONE map at 1M on macOS [CITED: docs/research/prolly-trees.md "the 44.6 ms is the sync effect"] |
| A3 | "Boost numbers stay within the rubric" means not `drop` of the Phase 24 rubric: median `total_ms` and CPU not worse than flat for K up to 100 at Boost scale, and new stored bytes for K up to 100 at most 2x flat [CITED: docs/research/prolly-trees.md "Recommendation" rubric] | Wave 6 | The owner may mean the `adopt` threshold (gain >= 20% and >= 10 ms); ask before the run |
| A4 | Counter bound of 3 x height nodes read and 2 x height + 1 written per tree for one change | Code Examples | Too tight or too loose; pin after the first measurement |
| A5 | Flat whole-commit time at 1M inodes is at least 5x the prolly one-change commit on Linux | Wave 6 | Unmeasured. Measure the flat baseline first (Wave 0) so the answer is known early |
| A6 | A decoded-node cache in the prolly crate is the right place | Standard Stack | A trait seam might be wanted for fault-injection tests; cheap to add later |
| A7 | Reusing `serialize_inode` for the inline record is enough (no version byte) | Open Question 4 | A later inode field needs a length-based discriminator (it already has 56/72) |
| A8 | Type byte = `(mode & S_IFMT) >> 12` | Open Question 2 | Different convention (DT_*) only matters to a future reader |
| A9 | Rehash port is in scope | Open Question 7 | If not, `rehash` must be disabled and its tests removed, which is a product regression |
| A10 | `stats::content_stats` O(files) RAM is left as is | Anti-Patterns | The owner rule "stats never loads a whole map" would be violated by a pre-existing function |
| A11 | Seeding 1M inodes in Docker with the v2.3.0 flat store takes minutes, not hours | Wave 6 | Schedule risk; measure at 100k first |
| A12 | The per-root full GC walk is acceptable for the snapshot counts of this phase | Pattern 4 | Slow GC with many snapshots |

## Environment Availability

| Dependency | Required By | Available | Version | Fallback |
|------------|------------|-----------|---------|----------|
| Rust toolchain | build, tests | yes, at `~/.cargo/bin` (not on default PATH) | rustc 1.95.0-nightly | `export PATH=$HOME/.cargo/bin:$PATH` |
| Docker (OrbStack) | mount tests, pjdfstest, Linux bench | yes | server 29.8.2 | none (owner rule: mounts only in Docker) |
| Docker volumes `slicefs-target*`, `slicefs-cargo` | cached builds | yes | n/a | new volume via `SLICEFS_TARGET_VOL` |
| git worktree | v2.3.0 baseline, agents | yes (`v2.3.0` tag exists; `commit_bench.rs` and `crates/slicefs-traits` unchanged between tags) | git | none needed |
| python3 | `bench.sh` median rows, `decide.py` | yes | 3.9.6 | none |
| Boost data (`fetch-boost.sh DIR 1_84_0 1_85_0`) | Boost `whole` runs | not checked | n/a | `SMOKE=1` generated trees for CI; Boost by hand only |
| macOS host RAM | 1M-inode builds | 192 GiB [VERIFIED: `sysctl hw.memsize` 206158430208] | | Docker VM memory may be lower: measure |

**Missing dependencies with no fallback:** none found.
**Missing dependencies with fallback:** `cargo` not on the default shell PATH (use `~/.cargo/bin`).

## Validation Architecture

### Test Framework
| Property | Value |
|----------|-------|
| Framework | Rust `cargo test` (workspace), `proptest` for property tests, counting `GlobalAlloc` test binaries for memory, Docker scripts for mounts |
| Config file | workspace `Cargo.toml` (members include `benchmarks/prolly-bench`); no separate test config |
| Quick run command | `cargo test -p prolly` and `cargo test -p metadata --lib` |
| Full suite command | `cargo test --workspace`; Docker: `scripts/docker-linux-test/run.sh all`, `SLICEFS_WORKERS=4 scripts/docker-linux-test/run.sh pjdfstest`, `scripts/docker-linux-test/run.sh verify` |

Docker case names, quoted from `run.sh`: `smoke) ...`, `test)  ...`, `all)   ...`, `verify) ...`, `coverage) ...`, `pjdfstest) ...` [VERIFIED: scripts/docker-linux-test/run.sh:34-39]. `SLICEFS_WORKERS=N` makes the scripts mount with `--workers N` and check the `Workers: N (set)` line [VERIFIED: pjdfstest.sh].

### Phase Requirements -> Test Map
| Req ID | Behavior | Test Type | Automated Command | File Exists? |
|--------|----------|-----------|-------------------|-------------|
| PMAP-03 | seed, mount, write, commit, remount: files, dirs, hard links, xattrs, snapshots equal | integration (Docker mount) + in-process | `scripts/docker-linux-test/run.sh all`; `cargo test -p slicefs-cli --test e2e_filesystem_tests --test dir_link_tests --test snapshot_switch_gc_tests` | yes (re-run on prolly) |
| PMAP-03 | pjdfstest 4 workers on a prolly store | Docker | `SLICEFS_WORKERS=4 scripts/docker-linux-test/run.sh pjdfstest` | yes |
| PMAP-03 | one-change commit reads/writes grow with height only (two sizes) | unit/integration | `cargo test -p metadata --test commit_counter_tests` | no - Wave 1/5 |
| PMAP-03 | no whole map at mount (open of a large store reads O(height) nodes) | integration | counter on `load_from_root` | no - Wave 1 |
| PMAP-06 | mark reaches every node of live root and each snapshot | integration | `cargo test -p metadata --test gc_prolly_tests` | no - Wave 1 |
| PMAP-06 | peak heap of the mark does not grow with the map (two sizes 4x apart) | alloc-counting binary, one test | `cargo test -p metadata --test gc_prolly_memory_tests` | no - Wave 1 |
| PMAP-06 | online GC frees unreachable nodes, keeps the rest, during finish | integration | `cargo test -p slicefs-cli --test gc_online_tests` plus `GcDuringFinish` window | partly (extend) |
| PMAP-08 | cut at each apply/root put/sync/install and `MemIo::crash` replay to a consistent tree | integration | `cargo test -p slicefs-cli --test write_log_replay_tests a_crash_between_the_cut_and_the_root_replays`; new `-p metadata --test commit_crash_tests` | partly (extend) |
| PMAP-07 | one-change commit at >= 1M inodes <= 1/5 of flat | by-hand bench in Docker | `cargo test --release -p metadata --test commit_bench -- --ignored --nocapture` in both trees | exists (extend) |
| PMAP-07 | Boost rubric, insdel recorded | by-hand bench | `scripts/prolly-bench/bench.sh DIR` in both trees | exists (adapt) |
| IN-02 | interim flat-map store refused with a message | unit | `cargo test -p slicefs-cli --test store_header_tests` | extend |
| rehash | rehash of a prolly store keeps the tree | unit/integration | `cargo test -p metadata --lib rehash`; `-p slicefs-cli --test rehash_tests --test hash_change_tests` | exists (port) |

### Tree validator (for PMAP-08 and every wave)
After replay, check: `prolly::walk` of each of the five roots (per-node parent checks are inside `walk`); every `dir_data` child ino exists in `inode_data`; every `manifest_data` ino and `xattr_data` ino exists in `inode_data`; every directory has its `.` and `..` entries (if stored); `inode_count` equals the `inode_data` count; `logical_bytes` equals the sum of sizes (small trees only); a mounted-state dump equals the pre-crash `dump_open` as the existing harness does.

### Fault points (new `CommitFault` values, `#[doc(hidden)]`)
Existing: `BeforeSync = 1`, `BeforeRoot = 2` [VERIFIED: store.rs:261-269]. Add: after each tree's `apply` (five), after the root record put, after install of roots (before sync), before `pre_root_sync`. `MemIo` already offers `crash()` (keeps only durable bytes [VERIFIED: mem.rs:350-358]), `fail_sync_after(skip)` and `fail_write_durable_after(kind, skip)`; it has no "fail the n-th pack put", so the per-write cut is a counting `CommitFault` (fail at the n-th node put) or `crash()` after n puts.

### Sampling Rate
- **Per task commit:** `cargo test -p prolly` and the touched crate's `--lib`.
- **Per wave merge:** `cargo test --workspace`.
- **Phase gate:** `cargo test --workspace`, `RUST_STABLE=stable scripts/safety/clippy.sh` (unsafe count), Docker `all`, `pjdfstest` with 4 workers, `verify`; then the by-hand PMAP-07 runs.

### Wave 0 Gaps
- [ ] `crates/prolly` tests for `NodeCache` (cap respected, same results cached/uncached), `range` (equals filtered `iter`), `count`, `visit` (every node ID once per occurrence).
- [ ] `crates/metadata/tests/gc_prolly_memory_tests.rs` (own allocator binary, one test).
- [ ] `crates/metadata/tests/commit_counter_tests.rs`.
- [ ] A test-only bulk loader that builds the five trees for N inodes with `prolly::build`, so tests at 1M do not call `create_inode` a million times.
- [ ] The pre-registered PMAP-07 rule file (before any measurement).
- [ ] A flat baseline measured on this host at 100k and 1M with the `v2.3.0` worktree, early (answers A5).

## Security Domain

`security_enforcement` is not set in `.planning/config.json`, so it is enabled [VERIFIED: config.json has no such key]. This phase has no network surface; the relevant surface is untrusted on-disk data (a damaged or crafted store, a shared store) and untrusted names.

### Applicable ASVS Categories
| ASVS Category | Applies | Standard Control |
|---------------|---------|-----------------|
| V2 Authentication | no | n/a (FUSE `DefaultPermissions`, `SessionACL::Owner` unchanged [VERIFIED: mount.rs `build_mount_options`]) |
| V3 Session Management | no | n/a |
| V4 Access Control | no change | Permission checks stay in the kernel (`DefaultPermissions`) and the fs layer; by-name entries must not weaken `lookup` of `..` for ancestry checks |
| V5 Input Validation | yes | `prolly::node::decode` bounds checks, `Error::Corrupt` never panics; names <= 255 bytes (`NameTooLong`), no NUL; reject any key above `MAX_KEY`; check `EntryTooLarge` before an apply |
| V6 Cryptography | yes (integrity only) | BLAKE3/SHA-256 object IDs verified on `ChunkStore::read`; boundary hash is keyed BLAKE3 with the header salt; never hand-roll |

### Known Threat Patterns for this stack
| Pattern | STRIDE | Standard Mitigation |
|---------|--------|---------------------|
| Crafted tree whose nodes are valid alone but do not fit together | Tampering | Parent-child checks in `get`/`range`/`walk`/`apply` (last key, first key above the previous sibling, leaf count) [CITED: crates/prolly/src/lib.rs "Each child is checked against its parent entry"]; map every `Error::Corrupt` to `MetaError::Corrupted`, never `unwrap` |
| Count overflow in a crafted internal entry | DoS | `u64` count checks already in `apply`; use checked arithmetic when summing counts in `count()` |
| Boundary grinding (a user who picks names makes many NODE_MIN nodes) | DoS | Accepted risk of Phase 25 (bounded, tree never breaks); salt option for stores that need it [CITED: docs/design/store-format.md "Boundary salt"]. By-name keys let a user choose names in one directory: note in the doc |
| Cache poisoning | Tampering | Cache is keyed by content-addressed `ObjectId` and filled only from hash-verified `read`; never insert unverified bytes |
| Unbounded memory from a hostile store (deep or wide tree, huge directory) | DoS | One path of nodes per cursor; cache byte cap; `list_directory` still builds a `Vec` per directory (pre-existing) |
| Interim or foreign store opened as a prolly store | Tampering | Magic check at load (IN-02), header refusals (M-numbers), `RuleVersion` error on another rule version |
| Path/name injection into the key grammar (`dir || name` ambiguity) | Tampering | Fixed 8-byte dir prefix makes the key unambiguous; reject empty names |

## Sources

### Primary (HIGH confidence, read this session)
- `crates/prolly/src/{lib,apply,tree,node}.rs` - API, constants (lines 111-171 of lib.rs), apply and cursor behavior
- `crates/metadata/src/store.rs` - `Live<T>`, `Sharded`, `PreparedCommit`, `CommitFault`, `begin_commit`, `finish_inner`, `for_each_referenced`, `load_from_root`
- `crates/metadata/src/{store_io,directory,inode,xattr,gc/mod,rehash/mod}.rs`, `crates/metadata/src/wal/mod.rs:80-205` (`WalStrategy`, `WalCut`)
- `crates/chunk-store/src/{file,sync,mem}.rs` - `decode_node`, `finish`, `Rank`, `MemIo::crash`
- `crates/slicefs-cli/src/{filesystem,mount,online_gc,stats,scrub,snapshot,oplog}.rs`, `crates/slicefs-traits/src/metadata.rs`
- `crates/slicefs-cli/tests/{write_log_replay_tests,replay_memory_tests}.rs`, `crates/chunk-store/tests/gc_full_memory_tests.rs`, `crates/metadata/tests/{commit_bench,node_crash_tests}.rs`
- `docs/design/store-format.md` "Metadata maps (store format 8)", `docs/research/prolly-trees.md`, `.planning/phases/25-*/25-08-SUMMARY.md`, `25-REVIEW-DISPOSITION.md`, `.planning/milestones/v2.2-phases/24-prolly-evaluation/24-CONTEXT.md` (D-17)
- `scripts/docker-linux-test/run.sh`, `pjdfstest.sh`, `scripts/prolly-bench/{README.md,bench.sh}`, `scripts/safety/model.sh`

### Secondary (MEDIUM confidence)
- Phase 24 numbers quoted from `docs/research/prolly-trees.md` (macOS main run, one Linux check).

### Tertiary (LOW confidence)
- Everything tagged `[ASSUMED]` above; in particular the PMAP-07 feasibility (A5).

## Metadata

**Confidence breakdown:**
- Standard stack: HIGH - no new packages; all in-workspace.
- Architecture: MEDIUM - the working-root/overlay protocol is a design built from the existing ticket scheme; Wave 1 exists to prove it.
- Pitfalls: MEDIUM-HIGH - from reading the code and tests; races are reasoned, not yet tested.
- PMAP-07 outcome: LOW - not measured; sync cost may dominate.

**Research date:** 2026-11-27
**Valid until:** 2026-12-27 (code moves with Phase 26 commits; re-check `store.rs` line numbers before planning)
