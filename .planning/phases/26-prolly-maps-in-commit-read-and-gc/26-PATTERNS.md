# Phase 26: Prolly maps in commit, read and GC - Pattern Map

**Mapped:** 2026-11-27
**Files analyzed:** 22 (new or modified)
**Analogs found:** 20 / 22
All analog paths below were checked with `git ls-files` and are tracked source (no `.gsd` mirrors). Line numbers are from the tree at branch start; re-check before editing, since `store.rs` moves with every plan.

## File Classification

| New/Modified File | Role | Data Flow | Closest Analog | Match |
|---|---|---|---|---|
| `crates/prolly/src/cache.rs` (new: NodeCache, Ctx) | utility | request-response (cached read) | `crates/prolly/src/tree.rs` `get` (uncached reader) | partial |
| `crates/prolly/src/range.rs` (new: seek, range, count) | utility | streaming | `crates/prolly/src/tree.rs` `Cursor`, `iter` | role-match |
| `crates/prolly/src/visit.rs` (new: node visitor) | utility | streaming | `crates/prolly/src/tree.rs` `walk`, `NodeInfo` | exact |
| `crates/prolly/src/{tree,apply}.rs` (modified: take `Ctx`, keep wrappers) | utility | CRUD / batch | themselves | self |
| `crates/metadata/src/pmap.rs` (new: MetaTree, Overlay, tickets, gen) | store | CRUD, event-driven (flush) | `Live<T>` in `crates/metadata/src/store.rs:78-133` | exact |
| `crates/metadata/src/dirent.rs` (new, replaces `directory.rs`) | model | CRUD | `crates/metadata/src/directory.rs` | role-match |
| `crates/metadata/src/store.rs` (modified: five trees, commit, load) | store | CRUD + batch commit | itself (`begin_commit`, `finish_inner`, `PreparedCommit`) | self |
| `crates/metadata/src/store_io.rs` (modified: salt, node cache) | config | file-I/O | itself | self |
| `crates/metadata/src/gc/mod.rs` (modified: `Ref::Tree`) | service | batch/streaming | itself (`mark`, `live_objects_exact_paced`) | self |
| `crates/metadata/src/rehash/{mod,map}.rs` (modified: `translate_tree`) | service | transform / batch | itself (`map.rs` translators) | self |
| `crates/metadata/src/{inode,xattr}.rs` (inline encodings) | model | transform | `serialize_inode` / `deserialize_inode` in `inode.rs` | exact |
| `crates/metadata/Cargo.toml` (prolly path dep) | config | n/a | other path deps in the same file | exact |
| `crates/slicefs-cli/src/header.rs` (new M message, IN-02) | config | request-response | `msg_flat_maps`, `msg_older` (lines 301-310) | exact |
| `crates/slicefs-cli/src/{filesystem,stats,scrub,mount}.rs` (callers, option) | controller | request-response | themselves | self |
| `crates/metadata/tests/commit_counter_tests.rs` (new) | test | request-response | `crates/metadata/tests/node_crash_tests.rs` + `ApplyStats` | role-match |
| `crates/metadata/tests/gc_prolly_memory_tests.rs` (new) | test | streaming | `crates/chunk-store/tests/gc_full_memory_tests.rs` | exact |
| `crates/metadata/tests/gc_prolly_tests.rs` (new) | test | batch | `crates/metadata/tests/gc_tests.rs` | exact |
| `crates/metadata/tests/commit_crash_tests.rs` (new) and `CommitFault` points | test | event-driven | `crates/slicefs-cli/tests/write_log_replay_tests.rs` `Window`, `cut_window_scenario` | exact |
| `crates/metadata/tests/commit_bench.rs` (modified: N from env, median) | test (bench) | batch | itself (unchanged since v2.3.0) | self |
| `benchmarks/prolly-bench/src/*.rs` (port or retire `batches`/`maps`/`flat`; IN-05) | utility | batch | itself (`layout.rs`, `main.rs` `RR_*`) | self |
| `docs/design/store-format.md`, `docs/design/metadata-maps.md`, `docs/research/prolly-trees.md` | doc | n/a | the existing "Metadata maps (store format 8)" section | self |
| PMAP-07 pre-registered rule file (`runs/`) | doc | n/a | `benchmarks/prolly-bench` `decide.py` rule style | partial |

## Pattern Assignments

### `crates/metadata/src/pmap.rs` (store, overlay with tickets)

**Analog:** `crates/metadata/src/store.rs` `Live<T>`, lines 78-133 (plus `take_all` at 223, `DIRTY_FLUSH`/`INODE_CACHE_MAX` at 60-75).

Copy the ticket scheme unchanged: `dirty`, `flushing` (key -> ticket), `tickets`, and the four transitions.

```rust
struct Live<T> {
    map: HashMap<u64, T>,
    dirty: HashSet<u64>,
    flushing: HashMap<u64, u64>,   // entries a commit/flush took, with take ticket
    tickets: u64,
}
fn mark_dirty(&mut self, ino: u64) { self.dirty.insert(ino); self.flushing.remove(&ino); }
fn requeue(&mut self) { /* flushing -> dirty again */ }
fn settle(&mut self, ino: u64, t: u64) -> bool {   // true only if ticket still current
    if self.flushing.get(&ino) == Some(&t) { self.flushing.remove(&ino); true } else { false }
}
```

Adapt: the overlay value type is `Option<V>` (None = delete), keyed by encoded key bytes (or dir ino for `dir_data`, `T = DirEdits`). Keep the cfg(test) constants small (store.rs 60-75: `DIRTY_FLUSH = 8`, `INODE_CACHE_MAX = 16`) so tests exercise the flush path (Pitfall 8). Lock rule from the field docs of `dir_live`/`inode_live`: no shard or overlay lock across a node read. Add `gen: AtomicU64` per tree and the recheck-under-lock loop of RESEARCH Pattern 1. New locks use `chunk_store::sync::{Mutex, Rank}` and need `Site` entries in `chunk-store/src/lock_stats.rs`.

---

### `crates/metadata/src/store.rs` (commit protocol, load, mount refusal)

**Analog:** itself. Excerpts to replace or extend.

`begin_commit` (line 1040): cut the WAL first, then take dirty state. The five `.lock().clone()` map copies (right after `inode_live.take_all()`) are what goes away; read the five working roots instead. Fix IN-01: read `next_ino` after `take_all` (today it is read before, at the line `let next_ino = self.inode_alloc.lock().next_ino();`).

```rust
let serial = self.commit_lock.lock();
let cut = match &wal { Some(w) => w.cut().map_err(|e| MetaError::Corrupted(format!("commit: write-log cut failed: {e}")))?, None => None };
let dirs = self.dir_live.take_all();
let next_ino = self.inode_alloc.lock().next_ino();   // IN-01: move after take_all
let inodes = self.inode_live.take_all();
let inode_data = self.inode_data.lock().clone();     // REPLACE with cut_roots + taken batches
```

`PreparedCommit` (line 275) holds `inodes: Vec<(u64,u64,InodeMeta)>` with tickets; replace the five map clones with five `ObjectId` cut roots and five taken `Vec<(key, ticket, Option<V>)>`. `Drop` (line 301) requeues only `inode_live` and `dir_live`; extend to every overlay (RESEARCH failure matrix).

`CommitFault` (line 264) is `#[repr(u8)]` with `BeforeSync = 1`, `BeforeRoot = 2`; add variants after each tree apply, after the root-record put, after root install, before `pre_root_sync` (keep `#[doc(hidden)]`).

Load: `check_root_len` (line 1721) compares only the length; the IN-02 refusal goes in `load_from_root` (line 1799), per RESEARCH "Interim-store refusal": read a root object, check `bytes.get(..4) == Some(&prolly::NODE_MAGIC[..])`, else return `MetaError::Corrupted` with the new M message.

Mark roots: `for_each_referenced` (line 1592) emits `Ref::Node`/`Ref::Content` keys; replace the `for_each_map_value` calls with the prolly visitor (see GC section). `referenced_estimate` (line 1613) must use `prolly::count` per tree.

Deprecated after the phase (delete, do not leave dead): `intern_u64_digest_map` (1751), `load_u64_digest_map`, `intern_key_u64_map`, `load_key_u64_map`, `MAP_ENTRY`, `for_each_map_value`, `map_size`.

---

### `crates/metadata/src/gc/mod.rs` (GC mark, bounded memory)

**Analog:** itself, `live_objects_exact_paced` (lines 87-99) and `mark` (line 160).

```rust
check_temp_space(cs, meta_roots, file_roots, true)?;
let mut live = cs.exact_mark().map_err(tmp_err)?;
let done = mark(cs, meta_roots, file_roots, &stop, pacer, &mut |id| live.insert(&id).map_err(tmp_err))?;
Ok(done.then_some(live))
```

New: a `Ref::Tree(ObjectId)` variant whose ids go straight to the `add`/`insert` callback (they are `SPT1` objects; `for_each_file_object` decodes `SFM1` only, so sending them through the sorter fails with `Corrupt`, Pitfall 3). Only manifest keys (`manifest_data` leaf values), spilled xattr keys and the root record stay on the sorter path. Call `stop()` and `pacer.at_safe_point()` every `WALK_STOP_EVERY` nodes. `check_temp_space` (line 110) calls `referenced_estimate` per meta root; keep that shape.

---

### `crates/prolly/src/visit.rs`, `range.rs`, `cache.rs`

**Analog:** `crates/prolly/src/tree.rs`: `get` (line 333, point lookup reading `&ChunkStore`), `Cursor` (358) and `iter` (375, starts at the first entry; the new range cursor adds a seek and an end bound with the same per-node parent-bound checks), `NodeInfo` (424) and `walk` (440, recursion holds one path; the new `visit` adds node `ObjectId`, level, and leaf entries). Public API shape to extend: `Builder::new(store, cfg)`, `push`, `finish` (lines 170-275); `apply(store, cfg, root, &[Edit]) -> (ObjectId, ApplyStats)` (`apply.rs:434`); `Edit::{Put{key,value}, Delete{key}}`; `ApplyStats { nodes_read, nodes_written, bytes_written, .. }` (`apply.rs:53`). Old signatures stay as wrappers over an uncached `Ctx` so Phase 24/25 tests and the bench do not move. Errors: map `Error::Corrupt` to `MetaError::Corrupted`, never unwrap. The cache must be filled only from hash-verified `ChunkStore::read`.

---

### `crates/metadata/src/dirent.rs` (model, CRUD)

**Analog:** `crates/metadata/src/directory.rs` (module doc lines 1-22 describe the old entry list; `name_key`, `entry_key`, `DirList::new_dir` which stores `.` and `..`). Keep: `NAME_MAX` 255 check via `check_name` (`store.rs` ~line 315), `DirEntry { name, ino }` from `slicefs-traits` (the trait does not change). Replace: key = `dir_ino (8 BE) || name`, value = `child ino (8 LE) || type (1)`, type = `(mode & S_IFMT) >> 12` read from the inode (A8). Readdir = range `[dir||"", (dir+1)||"")` merged with the overlay for that dir. Use `Live<T>` with `T = DirEdits` (`BTreeMap<name, Option<(ino,type)>>`).

---

### Inline encodings (`inode.rs`, `xattr.rs`)

**Analog:** `serialize_inode` / `deserialize_inode` in `crates/metadata/src/inode.rs` (deserialize accepts 56 or 72 bytes; the length is the discriminator, so no tag byte). Reuse as the `inode_data` leaf value. Xattr value: tag byte (0 inline set, 1 spill with 33-byte `ObjectKey`); inline only when `8 + 2 + 2 + set bytes` fits `MAX_ENTRY` (2,048). Existing xattr set encoding in `xattr.rs`.

---

### `crates/slicefs-cli/src/header.rs` (refusal message, IN-02)

**Analog:** `msg_flat_maps` (lines 306-309), `msg_older` (301-304). One function per message, formatted with `store.display()`, plain text with a pointer to docs:

```rust
fn msg_flat_maps(store: &Path) -> String {
    let store = store.display();
    format!("{store} was made by SliceFS v2.3.0 (store format 7, flat metadata maps). This SliceFS reads store format 8 only. Copy the files into a new store with the steps in docs/user/upgrade.md.")
}
```

The new message (interim Phase 25 build: format 8 header, flat maps) follows the same shape and gets the next M number. Tests: extend `crates/slicefs-cli/tests/store_header_tests.rs`.

---

### Crash tests and `CommitFault` points (PMAP-08)

**Analog:** `crates/slicefs-cli/tests/write_log_replay_tests.rs`, `enum Window` (line 835: `AfterCut`, `AfterNodes`, `AfterSyncs`, `AfterRoot`, `RootTorn`, `RootHeaderOnly`, `RootEmpty`, `Done`, `GcDuringFinish`) and `cut_window_scenario(fs, store, w)` (line 860), which uses `metadata::store::CommitFault` via `test_fail_next_commit_at`. Add windows per new fault point and run the same scenario; the requests after the cut include rename and fsync, so add the assertion that the committed root equals the state at the cut (Pitfall 2). Mock: `chunk_store::mem::MemIo` (`crash()`, `fail_sync_after`, `fail_write_durable_after`). Tree validator per RESEARCH "Tree validator" (walk each root, every dir child ino exists in `inode_data`, counts match). Also `crates/metadata/tests/node_crash_tests.rs` for the metadata-crate-level shape.

---

### `crates/metadata/tests/gc_prolly_memory_tests.rs` (peak memory)

**Analog:** `crates/chunk-store/tests/gc_full_memory_tests.rs` (and `crates/slicefs-cli/tests/replay_memory_tests.rs`): a counting `GlobalAlloc` wrapping `System` with `CURRENT`/`PEAK` atomics, one `#[test]` per binary.

```rust
use std::alloc::{GlobalAlloc, Layout, System};
static PEAK: AtomicUsize = AtomicUsize::new(0);
struct Counting;
fn grow(n: usize) { let now = CURRENT.fetch_add(n, Relaxed) + n; PEAK.fetch_max(now, Relaxed); }
unsafe impl GlobalAlloc for Counting { /* alloc, alloc_zeroed, dealloc (fetch_sub), realloc */ }
```

Run the mark at two sizes 4x apart and assert the peak does not grow with the map. Build the big trees with `prolly::build` or a test-only bulk loader, not 1M `create_inode` calls.

### `crates/metadata/tests/gc_prolly_tests.rs`

**Analog:** `crates/metadata/tests/gc_tests.rs` (marks, sweeps, keeps live). Add depth-3 trees and two snapshots sharing nodes (Pitfall 3, Wave 5).

### `crates/metadata/tests/commit_counter_tests.rs`

**Analog:** `ApplyStats` (`apply.rs:53`) plus the RESEARCH "Counter assertion" snippet; setup style from `crates/metadata/tests/node_crash_tests.rs`. Pin the A4 bounds after the first measurement.

### `crates/metadata/tests/commit_bench.rs` (PMAP-07)

**Analog:** itself, lines 1-40. It uses only `MetadataStore` plus `DictMetadataStore::new(io)`/`commit()` and `StoreIo::open`, so it builds on both v2.3.0 and the branch.

```rust
let io = Arc::new(Mutex::new(StoreIo::open(dir.path()).unwrap()));
let meta = DictMetadataStore::new(io);
/* create dirs, files: create_inode, link, set_manifest, increment_refcount */
meta.commit().unwrap();          // first commit
/* one more create_inode + link */
let t = Instant::now(); meta.commit().unwrap();   // the measured one-change commit
```

Change: N from env, 11+ samples, report the median; do not touch the signatures it uses.

---

### `crates/metadata/src/rehash/` (translate_tree)

**Analog:** the module doc of `rehash/mod.rs` (lines 1-30): byte copy vs translation of key-carrying nodes, refcount rows sorted by new key with the chunk-store external sorter, `KeyMap`. Reuse the sorter path for `refcounts` and feed sorted rows to `prolly::Builder`; the other four trees keep key order, so push in cursor order.

---

## Shared Patterns

### Lock order and gate
**Source:** top-of-file docs in `crates/metadata/src/store.rs` (live shard, then map lock); `commit_lock` then `apply_lock` (Rank::Store). Apply to all store changes. Hot-path flush uses `apply_lock.try_lock()` and skips while a commit is prepared (`sync::Mutex::try_lock`, `chunk-store/src/sync.rs:499`).

### Ticketed take/settle
**Source:** `Live<T>` (`store.rs:78-133`). Apply to the five overlays and the commit settle step; a settle succeeds only if the ticket is current.

### Error mapping
Map `prolly::Error::Corrupt` to `MetaError::Corrupted(String)`; `NameTooLong` from `check_name`; reject keys above `MAX_KEY` and values above `MAX_ENTRY` before an apply.

### Test constants
`cfg(test)` shrinks `INODE_CACHE_MAX` (16) and `DIRTY_FLUSH` (8) in `store.rs:60-75`; do the same for each new overlay.

### Must-still-pass lists (Pitfall 9)
`benchmarks/prolly-bench/tests/smoke.rs`, scrub, stats, `rehash_tests.rs`, `hash_change_tests.rs`, `gc` tests, `store_header_tests.rs`, `dir_link_tests`, `snapshot_switch_gc_tests`, `rename_ancestor_tests`, `posix_compliance_tests`, `root_record_layout`, `a_230_byte_root_record_is_refused`, `map_decode_tests`.

## No Analog Found

| File | Role | Data Flow | Reason |
|---|---|---|---|
| `crates/prolly/src/cache.rs` (byte-capped decoded-node cache) | utility | cached read | No node or object cache exists in the repo; `tree.rs` reads straight from `&ChunkStore`. Use RESEARCH Wave 0 spec (D-17 allows a fixed-size cache). |
| PMAP-07 pre-registered rule file | doc | n/a | Only the style exists (`decide.py` in `benchmarks/prolly-bench`, `docs/research/prolly-trees.md` rubric); the file itself is new and must precede any run. |
| Loom model of overlay/flush/commit (optional) | test | event-driven | `scripts/safety/model.sh` covers `chunk-store` and `slicefs-cli` only; owner decision. |

## Metadata

**Analog search scope:** `crates/metadata`, `crates/prolly`, `crates/chunk-store`, `crates/slicefs-cli`, `benchmarks/prolly-bench`, `scripts/`
**Files scanned:** about 25 read in part, targeted by grep
**Pattern extraction date:** 2026-11-27
