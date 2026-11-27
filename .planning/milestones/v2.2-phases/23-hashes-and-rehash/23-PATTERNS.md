# Phase 23: Hashes and rehash - Pattern Map

**Mapped:** 2026-11-26
**Files analyzed:** 17 (new and modified)
**Analogs found:** 16 / 17

All analog paths are git-tracked source (checked with `git ls-files` for the key ones; all live under `crates/`, `docs/`, `Cargo.toml`).

## File Classification

| New/Modified File | Role | Data Flow | Closest Analog | Match |
|---|---|---|---|---|
| `Cargo.toml`, `crates/chunk-store/Cargo.toml`, `crates/slicefs-cli/Cargo.toml` | config | n/a | same files (swap `sha2-compress` for `sha2`) | exact |
| `crates/chunk-store/src/hash.rs` (add `Sha256`) | utility (hasher) | transform | `Blake3_256` in the same file | exact |
| `crates/chunk-store/src/lib.rs` (make `extsort` pub) | config | n/a | `pub mod file;` / `pub mod hash;` lines 27-39 | exact |
| `crates/slicefs-cli/src/header.rs` (flag `hash-sha256`) | model/parser | request-response | `CHUNK_PROFILE_FLAG` handling in the same file | exact |
| `crates/slicefs-cli/src/content.rs` (flag in `check_write_with`, `rehash-tmp` cleanup, open with hasher) | service | CRUD | `check_write_with`, `GC_TMP_DIR` in the same file | exact |
| `crates/metadata/src/segment/mod.rs` (last record per version) | service | file-I/O | both loaders in the same file | exact |
| `crates/metadata/src/segment/switch.rs` (new, `write_switch_segment`) | service | file-I/O | `compact_segment` in `compaction.rs` | role-match |
| `crates/metadata/src/rehash/mod.rs` (new engine) | service | batch / streaming | `put_value`/`get_value` in `store_io.rs`, `FileReader`/`FileWriter` in `file.rs`, `referenced` in `store.rs` | role-match |
| `crates/metadata/src/rehash/map.rs` (new `DiskMap`) | utility | file-I/O | `extsort.rs` (temp files, bounded RAM) | partial |
| `crates/slicefs-cli/src/gc.rs` (`gc_on_open_store`) | service | batch | `gc_report` in the same file | exact |
| `crates/slicefs-cli/src/rehash.rs` (new command) | controller (CLI command) | batch | `gc.rs` `run_gc_mode`/`gc_report`, `snapshot.rs` `run_snapshot_create` | role-match |
| `crates/slicefs-cli/src/cli.rs`, `main.rs`, `lib.rs` | controller | request-response | `Cmd::Gc` / `Cmd::Scrub` | exact |
| `crates/chunk-store/src/hash.rs` tests (golden vectors) | test | transform | `lookup_by_name_and_tag` test | exact |
| `crates/slicefs-cli/tests/rehash_tests.rs` (HASH-06, crash, refusal) | test | batch | `hash_change_tests.rs`, `gc_cli_tests.rs` | role-match |
| `crates/metadata/tests/` snapshot-supersession and switch tests | test | file-I/O | `segment_store_tests.rs`, `gc_tests.rs` | role-match |
| `docs/design/store-format.md`, `docs/design/chunk-store.md`, new `docs/design/rehash.md` | docs | n/a | existing design docs | role-match |
| `docs/design/unsafe-review.md` / unsafe-count doc | docs | n/a | existing | exact |

## Pattern Assignments

### `crates/chunk-store/src/hash.rs` (add `Sha256`) — utility, transform

**Analog:** `Blake3_256` in `crates/chunk-store/src/hash.rs` lines 31-46 and `HASHERS` lines 69-79.

```rust
/// BLAKE3, the full 256-bit output. Tag 0x1e: the multihash code of BLAKE3.
#[derive(Clone, Copy, Debug, Default)]
pub struct Blake3_256;

impl ObjectHasher for Blake3_256 {
    fn name(&self) -> &'static str { "blake3-256" }
    fn tag(&self) -> u8 { 0x1e }
    fn digest(&self, data: &[u8]) -> [u8; DIGEST_LEN] { *blake3::hash(data).as_bytes() }
}

static HASHERS: &[&dyn ObjectHasher] = &[
    &Blake3_256,
    #[cfg(any(test, feature = "test-hasher"))]
    &TestKeyedBlake3,
];
```

Copy: same derive + `impl ObjectHasher`, name `sha256`, tag `0x12`, digest `Sha2::digest(data).into()` (see RESEARCH Pattern 1). Add `&Sha256,` right after `&Blake3_256,` with no `cfg` (all builds). `parse_hash`, `zero_chunk()` and `--hash` pick it up from `hashers()` with no other code.

**Test pattern** (`hash.rs` ~line 112, `lookup_by_name_and_tag`): extend with `hasher_by_name("sha256").unwrap().tag() == 0x12` and the golden vectors from RESEARCH Q1 (empty, "abc", 56-byte string, one million `a`). Also a store round trip like the existing `a_new_write_hash_keeps_old_objects_readable` (same file, uses `open(&mem, hasher)` helper at line ~130).

---

### `crates/slicefs-cli/src/header.rs` (flag `hash-sha256`) — parser, request-response

**Analog:** the `chunk-profile` flag in the same file.

- Constants, lines 32-38: `CHUNK_PROFILE_FLAG`, `CHUNK_PROFILE_RELEASE = "v2.2"`, `KNOWN_FLAGS: &[&str] = &[CHUNK_PROFILE_FLAG];`. Add `HASH_SHA256_FLAG`, `HASH_SHA256_RELEASE`, and put both flags in `KNOWN_FLAGS`.
- `marker_with(hasher, profile)` line 50-54 appends `ro_compat chunk-profile v2.2` for a non-default profile. Copy the shape: when `hasher.name() == "sha256"`, push `incompat hash-sha256 <release>\n` (class word `incompat`, see `Class::word` lines 73-79).
- Pairing rule in `parse`, lines 379-391 (`lines.iter().position(|l| matches!(l, Line::Flag(f) if f.name == CHUNK_PROFILE_FLAG))`, then check the class and the `chunks` line). Copy for: a `hash sha256` line requires the flag present as `Incompat`, else `bad_at(...)` (M9). The flag without the line is valid.
- `decide` lines 444-470 filters flags by `KNOWN_FLAGS`; no change needed once the name is in the list.
- Kani: the four proved functions (`valid_name_bytes`, `valid_release_bytes`, `first_line_number`, `Class::from_word_bytes`, harnesses at ~lines 1047-1096) do not change; `hash-sha256` is a valid name. Run `scripts/safety/kani.sh` once.
- Test helper at line ~923: `format!("{class} {CHUNK_PROFILE_FLAG} {CHUNK_PROFILE_RELEASE}\n")`; copy for header unit tests of the new flag and its refusal cases.

---

### `crates/slicefs-cli/src/content.rs` — service, CRUD

**Analog:** `check_write_with`, lines 135-157 (excerpt):

```rust
let opened = header::check_with(store, Want::Write, new.unwrap_or(DEFAULT_HASHER), profile.unwrap_or(ChunkProfile::DEFAULT))?;
check_profile(store, opened.header.as_ref(), profile)?;
let Some(new) = new else { return Ok(None) };
let mut h = opened.header.ok_or_else(|| std::io::Error::other(...))?;
let current = hasher_of(store, Some(&h))?;
if current.name() == new.name() { return Ok(None); }
match h.lines.iter_mut().find(|l| matches!(l, Line::Hash(_))) {
    Some(l) => *l = Line::Hash(new.name().to_string()),
    None => h.lines.insert(0, Line::Hash(new.name().to_string())),
}
header::write_header(store, &h)?;
```

Change: when `new` is sha256 and the header has no `hash-sha256` flag, push a `Line::Flag(..)` before `write_header` (one atomic write, flag lands before any SHA-256 object). Rehash step 2 and step 5 reuse this function (step 5: rewrite hash line to target).

**Cleanup pattern:** `GC_TMP_DIR = "gc-tmp"` (line ~161) and `ChunkStore::open_with` removal at `crates/chunk-store/src/lib.rs:1448-1454`:
`if let (Some(d), false) = (&opts.gc_tmp_dir, opts.read_only) { remove_dir_all ... }`. Add a `REHASH_TMP_DIR = "rehash-tmp"` constant next to `GC_TMP_DIR` and one `remove_dir_all` in the writable `content::open` (not under `gc-tmp`, R-02).

**Read-only open (dry-run):** `content::open_read_only` (lines ~199-205): `header::check(store, Want::Read)` then `Options { read_only: true, .. }`. Use it for `--dry-run`; apply `decide(.., Want::Write)` on the parsed header so refusals match a real run (RESEARCH Q11).

Also reuse: `parse_hash` lines 96-100, `require_store`, `READ_BUF` (1 MiB window).

---

### `crates/metadata/src/segment/mod.rs` (last record per version) — service, file-I/O

**Analog:** both loaders in the same file: `load_store_from_segments` (lines ~60-111, pushes every `SnapshotRecord` into a `Vec`) and `scan_store_with_log` (ends line ~204: `Ok((last_root, snapshots, WriteLog { spans, ops }))`).

Core excerpt to change (lines 94-108):
```rust
SegmentEntry::RootUpdate { root } => { last_root = Some(root); }
ref snap @ SegmentEntry::SnapshotRecord { .. } => {
    if let Some(snap_entry) = snap.as_snapshot_entry() { snapshots.push(snap_entry); }
}
...
Ok((last_root, snapshots))
```
Add one helper `last_per_version(Vec<SnapshotEntry>) -> Vec<SnapshotEntry>` (BTreeMap by version, later insert wins, `into_values()`; RESEARCH Pattern 3) and call it at both return points. Return types stay the same, so no caller changes (`gc.rs:110`, `scrub.rs:91`, `snapshot.rs`, `stats.rs:265`, `mount.rs:124,181`). Order is by `segment_order(p)`, not by name (line 72-73 comment).

Tests: copy style from `crates/metadata/tests/segment_store_tests.rs`. Do not assert segment counts after an offline GC (it leaves copies, Pitfall 3).

---

### `crates/metadata/src/segment/switch.rs` (new) — service, file-I/O

**Analog:** `compact_segment` in `crates/metadata/src/segment/compaction.rs` lines 30-95 (temp + rename):

```rust
let tmp_path = output_dir.join(format!("{}.tmp", out_filename));
let mut writer = SegmentWriter::new(&tmp_path, output_segment_id)?;
writer.write_entry(&entry)?;
writer.close()?;                      // sync + EOF marker
std::fs::rename(&tmp_path, &out_path)?;
```

Copy that, then add what compaction omits: sync the directory after the rename, delete older segments, sync the directory again. Models for the commit-time shape: `write_root_segment` / `rotate_segment` in `crates/metadata/src/wal/mod.rs` (~lines 347-402; file name `segment-{:06}.seg`) and `finish_inner` seed in `store.rs:1191-1211` (`[RootUpdate] + one SnapshotRecord per snapshot`). Segment id rule: max existing id + 1 (`mount.rs:199-220` `next_segment_id`). Name must end `.seg.tmp` while incomplete so every filter ignores it (`name.ends_with(".seg")`, `segment/mod.rs:72`).

---

### `crates/metadata/src/rehash/mod.rs` (new engine) — service, streaming batch

**Analogs:**
- Value primitives: `crates/metadata/src/store_io.rs` `to_key` (line 51), `to_id` (56), `put_value` (61, `FileWriter` over bytes), `get_value` (69, `FileReader`).
- File streaming: `crates/chunk-store/src/file.rs` `FileReader::open/read`, `FileWriter::new/write/finish`, `for_each_file_object` (line 333, bounded; never `file_objects` line 323, which returns a `Vec`).
- Wire formats to translate: root record layout `crates/metadata/src/store.rs:36-52` (230 bytes, offsets from `KEY_BYTES = 33`); flat maps `MAP_ENTRY = 8 + KEY_BYTES` at `store.rs:1625-1676`; refcounts `store.rs:1757-1801`; inode map `inode_map.rs:88-96` (`RECORD`); manifest list `manifest.rs:22-28`.
- Reference graph walk (for kinds and order, tests only): `referenced` `store.rs:1580-1603`.

Core pattern (RESEARCH Pattern 4, `rehash_value`): `FileReader::open` -> windowed `read` (1 MiB) -> `FileWriter::write` -> `finish`; apply the same function to files, inode nodes, directory nodes and xattr nodes. Flat-map stream translate: RESEARCH "Code Examples" (translate each 41-byte record key, keep `ino`, check ascending order). Do not translate directory-list name keys (BLAKE3 of name, `directory.rs:254-257`).

Errors: use `chunk_store::Error` kinds from `ChunkStore::read` (`Missing`, `Corrupt`, `UnknownHash`, `Io`); stop before the switch (D-15).

Test hook pattern: `CommitFault` / `test_fail_next_commit_at` at `store.rs:261-275, 1026-1035`, used in `write_log_replay_tests.rs:958-962`. Copy as a `#[doc(hidden)]` `stop_after: Option<Step>` option (RESEARCH Q10). Crash tests with `MemIo::crash()` as in `crates/metadata/tests/node_crash_tests.rs`.

---

### `crates/metadata/src/rehash/map.rs` (new `DiskMap`) — utility, file-I/O

**Analog (partial):** `crates/chunk-store/src/extsort.rs` for the temp-dir lifecycle and fixed-RAM discipline: `TmpDir::new(parent)` (line 46), constants `SORT_CHUNK_BYTES = 4 << 20`, `MERGE_FAN_IN = 32`, `READ_BUF = 64 << 10` (lines 22-28). No existing open-addressing table; use `std::os::unix::fs::FileExt` (`read_exact_at`, `write_all_at`), 66-byte slots, tag 0 = empty (`slicefs-traits/src/key.rs:20-21`), direct-mapped RAM cache. See RESEARCH Q5 for the design.

**Refcount sort:** make `mod extsort;` public in `chunk-store/src/lib.rs:28` and make `TmpDir`, `Sorter`, `SortedFile`, `RecReader`, `SortCfg` `pub` (currently `pub(crate)`); use `Sorter<41>`.

---

### `crates/slicefs-cli/src/gc.rs` (`gc_on_open_store`) — service, batch

**Analog:** `gc_report` in the same file, lines 90-165. The part to extract is lines 126-149:

```rust
let no_stop = std::sync::atomic::AtomicBool::new(false);
let content = if full {
    let boundary = cs.gc_begin_full()?;
    let marked = live_objects_exact(&cs, &roots, &[], &no_stop);
    let ids = match marked {
        Ok(Some(ids)) => ids,
        Ok(None) => { cs.gc_abort(); return Err("GC mark stopped, nothing deleted".into()); }
        Err(e) => { cs.gc_abort(); return Err(format!("GC mark failed, nothing deleted: {}", e).into()); }
    };
    cs.gc_finish_full(ids, boundary)
} else { /* live_objects + gc_begin + gc_finish */ }
.map_err(|e| format!("GC of file content failed: {}", e))?;
```

Make this a `gc_on_open_store(cs: &Arc<ChunkStore>, roots, full)` that `gc_report` calls after its own lock and open. Rehash calls it with the open store and does NOT call `GarbageCollector::run_gc` (it copies segments) or `lock_offline` (second flock in one process is refused). Reuse `stats_line`, `usage_line(before, after)` (lines 51-56), `store_usage`.

---

### `crates/slicefs-cli/src/rehash.rs` (new command) — controller, batch

**Analogs:** `run_snapshot_create` (`snapshot.rs:40-75`) for the offline command preamble, `gc.rs` `run_gc_mode` / `gc_report` for report printing.

Preamble to copy (`snapshot.rs:46-62`):
```rust
crate::content::require_store(store_path)?;
crate::header::check(store_path, crate::header::Want::Write)?;
let lock_path = store_path.join("mount.lock");
if lock_path.exists() { return Err(format!("Store is mounted (mount.lock found at {}). ...", lock_path.display()).into()); }
let _store_lock = crate::header::lock_offline(store_path)?;   // held to the end
let segs_dir = store_path.join("segments");
let (root_opt, snapshots) = load_store_from_segments(&segs_dir).map_err(|e| format!("failed to load segments: {}", e))?;
```
Per R-01, `require_store` runs before the lock; take the lock, then header check, then `scan_store_with_log(&segs_dir)` and refuse when `!log.is_empty()` ("mount and unmount the store once, then run rehash again"). Then follow the D-18 order (flag, rewrite, `cs.sync()`, `write_switch_segment`, header hash line via `content::check_write_with`, `gc_on_open_store`, report). A real run always runs the full GC (R-08).

Output style: `println!("{}", gc_report(...)?)` in `gc.rs` ~line 85-88; progress lines to stderr.

---

### `crates/slicefs-cli/src/cli.rs`, `main.rs`, `lib.rs` — controller

**Analog:** `Cmd::Gc` (`cli.rs:171-181`) with `#[arg(long)] full: bool`, and its dispatch in `main.rs:64-73`:

```rust
Cmd::Gc { store, full } => {
    if let Err(e) = gc::run_gc_mode(&store, full) {
        if json { eprintln!("{{\"error\": \"{e}\"}}"); } else { eprintln!("slicefs gc error: {e}"); }
        std::process::exit(1);
    }
}
```
Add `Cmd::Rehash { to, dry_run, store }` with `#[arg(long, value_parser = crate::content::parse_hash)] to: String` (as `--hash` does on `Seed`), `#[arg(long)] dry_run: bool`, and the same error block with `"slicefs rehash error: {e}"`. Add `pub mod rehash;` in `lib.rs` (beside `gc`, `scrub`). Parse tests: copy the style at `cli.rs:297-340` (`Cmd::Seed { .. } => assert_eq!(...)`, `_ => panic!("expected ...")`).

---

### Tests

**`crates/slicefs-cli/tests/rehash_tests.rs`** (HASH-06, crash, refusal). Analog: `hash_change_tests.rs` lines 1-40 (`open_fs`, `next_segment`, `TEST_HASH = "test-keyed-blake3"`, `run_seed`, `SliceFsFilesystem`, `test_create/test_write/test_release`, `content::select_hash`). Use `slicefs_cli::seed::run_seed_with(store, src, hash, profile)` (`seed.rs:46`); snapshots via `run_snapshot_create`; compare trees with `tests/common/mod.rs` `dump(fs)`; scrub via `run_scrub(store, json)`; large profile `ChunkProfile::new(16384, 65536, 262144)` as in `chunk_profile_tests.rs:32-34`. Lock refusal pattern: `mount_lifecycle_tests.rs:675` (`lock_offline(store).is_err()` while held). Spliced mixed-tag file: write under BLAKE3, `select_hash` to the second hash, partial `test_write`, release. Keep data small (a few MB, `hash_change_tests` runs 1.5 s). Do not write the word `unsafe` in any new `.rs` text (unsafe-count counts comments).

**`crates/metadata/tests/`** new switch and loader tests. Analogs: `segment_store_tests.rs`, `gc_tests.rs`, `node_crash_tests.rs` (`MemIo`, `crash()`, `set_fail_syncs`).

## Shared Patterns

### Offline command lock and checks
**Source:** `crates/slicefs-cli/src/snapshot.rs:46-62`, `gc.rs:104`. **Apply to:** `rehash.rs`. `header::lock_offline` held in a `_store_lock` binding to the end; never call a function that locks again in the same process (`gc_report`, `run_snapshot_*`).

### Durable file write (temp, sync, rename, dir sync)
**Source:** `header::write_header` (`header.rs:644`), `compaction.rs:46-84`, `wal/mod.rs:347-402`. **Apply to:** `switch.rs`, header rewrite.

### Verified reads
**Source:** `ChunkStore::read` (`chunk-store/src/lib.rs:2885-2959`) via `FileReader`. **Apply to:** all rehash reads. `get` also verifies but hides the error kind.

### Error and exit pattern
**Source:** `main.rs:64-73`. **Apply to:** the `Rehash` arm.

### Temp-dir cleanup
**Source:** `lib.rs:1448-1454`, `content.rs` `GC_TMP_DIR`. **Apply to:** `rehash-tmp` in `content::open`, at rehash start and end.

### Safety gates
`scripts/safety/unsafe-count.sh` counts the word in all `.rs` files, comments included: avoid it in new code. Run `scripts/safety/kani.sh` once after the header change. Cargo gates use `--locked`, so commit the `Cargo.lock` change.

## No Analog Found

| File | Role | Data Flow | Reason |
|---|---|---|---|
| `crates/metadata/src/rehash/map.rs` (open-addressing table) | utility | file-I/O | No on-disk hash table exists; only `extsort.rs` gives the temp-dir and bounded-RAM pattern. Use RESEARCH Q5 design. |

## Metadata

**Analog search scope:** `crates/chunk-store/src`, `crates/metadata/src`, `crates/metadata/tests`, `crates/slicefs-cli/src`, `crates/slicefs-cli/tests`.
**Files scanned:** about 20 read or grepped.
**Pattern extraction date:** 2026-11-26
