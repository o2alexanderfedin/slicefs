# Phase 26: Prolly maps in commit, read and GC - Context

**Gathered:** 2026-11-27
**Status:** Ready for planning

<domain>
## Phase Boundary

Every metadata map of the live store becomes a prolly map (`crates/prolly`, layout a of Phase 25: five trees). The commit path writes only the changed nodes, the read path reads entries on demand, GC mark walks the trees with bounded memory, a cut commit replays to a consistent tree, and the gain is measured on the product code against the flat maps of release `v2.3.0` (PMAP-03, PMAP-06, PMAP-07, PMAP-08; #215).

Today all five maps are `BTreeMap`s held whole in RAM (`crates/metadata/src/store.rs`, `dir_data`, `inode_data`, `xattr_data`, …) and `begin_commit` clones them. Removing that is the core of this phase (Phase 25 D-01: no operation loads a whole map into RAM; 1B+ inode design target).

Not in this phase: store migration (Phase 27), crash/soak tests of a live mount (Phase 28), scrub report (Phase 29), refcount removal (#165).

</domain>

<decisions>
## Implementation Decisions

### On-disk format (owner decisions, 2026-11-27)
- Directory entries are stored by name in the `dir_data` tree: key = directory ino (8 bytes, big endian) || entry name; value = child ino + file type. One change in any directory touches O(tree height) nodes; readdir is a range scan from the directory prefix; the per-directory list object goes away. This replaces the `dir_data` row of the layout-a key grammar in `docs/design/store-format.md` (Phase 25 left room for it). — **Reversibility:** one-way — it is part of store format 8; undo needs a format change and a store copy (Phase 27 path).
- Small values are stored inline in the prolly leaves: the inode record goes inline in `inode_data`; an xattr set goes inline in `xattr_data` when the entry fits `MAX_ENTRY` (2,048 bytes) and spills to a separate object (value = `ObjectKey`) when it does not; `manifest_data` values stay 33-byte `ObjectKey`s (a manifest is a content tree). `refcounts` keep content key → 8-byte count. This removes about one chunk-index entry and one read per inode lookup. — **Reversibility:** one-way — part of store format 8; undo needs a format change and a store copy.
- `docs/design/store-format.md` § "Metadata maps (store format 8)" is updated with the new key grammar and value encodings (with a version or tag byte where an inline value and a spilled value can both occur).

### Carried from Phase 25 (locked)
- Layout a (five trees), constants L=8192 / NODE_MAX=16384 / NODE_MIN=512 / MAX_ENTRY=2048 / MAX_KEY=1024, integer boundary rule with `RULE_VERSION` 1, optional boundary salt, store format 8, 197-byte root record with five map keys and `next_ino`.
- Design target 1B+ inodes; counts from internal entry counts or the root record, never a full leaf walk.
- Online GC stays mandatory; FSKit is the only macOS backend; mounts only in Docker for tests.

### Decided autonomously
- [Read path] Q: 'Where do map entries live at run time?' → Selected: 'read on demand from prolly nodes through a bounded node cache; dirty entries stay in memory until commit; no whole map in RAM' — internal code, no format effect
- [Read path] Q: 'Node cache size?' → Selected: 'fixed byte cap, default 64 MiB, mount option to change it, counted in the existing memory caps' — a number and an option
- [Commit] Q: 'How does a commit write?' → Selected: 'per tree, sort the dirty keys, one apply batch, write new nodes, then the root record with the five new roots' — internal to the commit path
- [Commit] Q: 'What about stores written by the interim Phase 25 build (format 8 header, flat maps)?' → Selected: 'refuse cleanly at mount with a message (review IN-02), not a size error; also fix IN-01 (read next_ino after take_all) and IN-05 (bench --l default 8192)' — no deployments, no backward compatibility
- [GC] Q: 'How does mark walk the trees?' → Selected: 'a stack of one node path per tree, for the live root and each snapshot; a test caps peak memory on a large map' — internal
- [Crash tests] Q: 'How to cut a commit at each step?' → Selected: 'test-only fault point after each node write and each sync step, then replay and check the tree' — test code only
- [Bench] Q: 'How to build a store of 1M+ inodes and the flat baseline?' → Selected: 'a generator that writes through the product metadata API; flat baseline with a v2.3.0 binary built in a separate worktree' — bench tooling

### Claude's Discretion
- Encoding details of the inline values (field order, tag byte), cache eviction policy, the exact fault-point list, the mitigation for the insert/delete-heavy regression (or a written acceptance), and how hard links appear in `dir_data` (each name is its own entry pointing to the same ino).

</decisions>

<code_context>
## Existing Code Insights

### Reusable Assets
- `crates/prolly`: `Splitter`, `apply` (batch edits), `node::decode` with bounds checks, `Config::DEFAULT`, salt support, forced-cut counts, golden tests.
- `benchmarks/prolly-bench`: `maps`, `layout`, `synth` modes, `layout.sh` matrix runner, `decide.py` rule style (pre-registered rules), Docker case `prolly-layout`.
- `crates/metadata/src/inode_alloc.rs` (allocator), the WAL modes in `crates/metadata/src/wal/`, `gc/mod.rs` and `gc/background.rs` (online GC), `snapshot.rs`.

### Established Patterns
- Lock order documented at the top of `store.rs` (a live shard, then the map lock); the commit gate (RwLock) serializes commits against the FS.
- Root record written by `begin_commit` / commit settle; `intern_u64_digest_map` writes a flat map today and is the main thing to replace.
- Header and refusal messages in `crates/slicefs-cli/src/header.rs` (M-numbered messages).

### Integration Points
- `crates/metadata/src/store.rs`: `begin_commit`, commit settle, mount/load, `delete_inode`, lookup and readdir paths, `dir_live` shards.
- `crates/metadata/src/gc`: mark phase over the maps.
- `crates/metadata/src/rehash`: walks the root record maps.
- `crates/slicefs-cli` stats (counts must come from the root record or internal entries).

</code_context>

<specifics>
## Specific Ideas

- Success criterion 2 uses a counter test at two store sizes: nodes read and written for a one-change commit grow at most with the tree height.
- PMAP-07: one-change commit at 1,000,000 inodes or more takes at most 1/5 of the flat time of release `v2.3.0`.

</specifics>

<deferred>
## Deferred Ideas

- Carried from Phase 25: salt rebuild command; 100M/1B measured runs; global namespace identity; snapshot diff by equal subtrees.
- Owner question from 25-07 (open, does not block): allow a tolerance (for example 5%) on the 1B node-count rule if the layout is ever decided again.

</deferred>
