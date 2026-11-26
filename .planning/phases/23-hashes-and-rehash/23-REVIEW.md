---
phase: 23-hashes-and-rehash
reviewed: 2026-11-26T16:24:12Z
depth: deep
files_reviewed: 23
files_reviewed_list:
  - Cargo.toml
  - crates/chunk-store/Cargo.toml
  - crates/chunk-store/src/hash.rs
  - crates/chunk-store/src/extsort.rs
  - crates/chunk-store/src/lib.rs
  - crates/metadata/src/inode_map.rs
  - crates/metadata/src/lib.rs
  - crates/metadata/src/rehash/map.rs
  - crates/metadata/src/rehash/mod.rs
  - crates/metadata/src/segment/mod.rs
  - crates/metadata/src/segment/switch.rs
  - crates/metadata/src/store.rs
  - crates/metadata/src/wal/mod.rs
  - crates/slicefs-cli/Cargo.toml
  - crates/slicefs-cli/src/cli.rs
  - crates/slicefs-cli/src/content.rs
  - crates/slicefs-cli/src/gc.rs
  - crates/slicefs-cli/src/header.rs
  - crates/slicefs-cli/src/lib.rs
  - crates/slicefs-cli/src/main.rs
  - crates/slicefs-cli/src/rehash.rs
  - crates/slicefs-cli/tests/rehash_tests.rs
  - crates/slicefs-cli/tests/snapshot_switch_gc_tests.rs
findings:
  critical: 0
  warning: 2
  info: 4
  total: 6
  necessity: 0
status: issues_found
---

# Phase 23: Code Review Report

**Reviewed:** 2026-11-26T16:24:12Z
**Depth:** deep
**Files Reviewed:** 23
**Status:** issues_found

## Summary

Scope: `git diff cbe3935..HEAD -- crates/ scripts/ Cargo.toml`. The diff has no change in `scripts/`. `hash_change_tests.rs` and `store_header_tests.rs` were not reviewed. Only some parts of `rehash_tests.rs` were read: the crash tests and the helpers.

These items were checked, and no defect was found in them:

- **Switch segment.** The temp name `segment-N.seg.tmp` does not match any segment filter. The segment is synced, then renamed, then the directory is synced, and then the older segments are removed. After the rename, both loaders keep the last record of each version. Segment compaction (`+100_000` ids) keeps the order, so old segments that stay after a crash still lose to the switch.
- **Run order.** The flag is written before the first SHA-256 object. The chunk store is synced before the switch. The header line is written after the switch. The full GC always runs, so a rerun after a crash completes the job.
- **Lock.** There is one `lock_offline`, which is held to the end. `check_write`, `ensure_hash_flag`, `open_with_hasher` and `gc_on_open_store` take no lock. Every other writable open (the open that also removes `rehash-tmp`) takes the store lock first.
- **Old-to-new map.** The map key is (kind, old key), and a translation depends only on the bytes of the old key, so a hit is always correct. The real-run map is complete on disk. Its RAM is a fixed cache.
- **Refcount merge.** The refcount keys are content keys only (`filesystem.rs` 2522, 2929, `release_content`). The rows are sorted by the new key with a row index, so no row is deduplicated by error. The counts are summed with saturation. The extsort code operates correctly with no rows.
- **Header rules.** `hash sha256` must have the flag, and the flag must be incompat. The flag without the line is valid. A second write adds no second flag line.
- **Memory.** Each value has one window, one reader and one writer. The nesting depth is fixed. `for_each_file_object` is bounded.
- **Overflow and unsafe.** Counts read from the store use `checked_mul`. The diff adds no `unsafe`.

The two warnings are about the order of the refusals and the accuracy of the dry-run (a run that only reads and reports). No necessity findings: the guards in the new code read data from the store, which is a trust boundary.

## Warnings

### WR-01: An unknown write hash in the header is refused only after the switch

**File:** `crates/slicefs-cli/src/rehash.rs:124-156`, `crates/slicefs-cli/src/rehash.rs:185`, `crates/slicefs-cli/src/content.rs:221-226`, `crates/slicefs-cli/src/content.rs:149`
**Issue:** `header::parse` accepts any `hash <name>` line, and `header::decide` does not look at the name. Only `content::hasher_of` refuses a name that this build does not know. In a real run, step 1 does not call `hasher_of`. `open_with_hasher` passes `Some(target)`, so it does not call `hasher_of` either. The first call is in `check_write_with` at step 5, after the switch.
Failure scenario: a header has `hash foo` (from a newer build, or an edit by hand), and all live objects have known tags. Then `rehash --to sha256` writes the flag, rewrites every value, and switches the root and all snapshot roots. After that, it fails at step 5 with "records the write hash "foo", which this SliceFS does not know". The GC does not run. Each rerun fails at step 5 again, so the job never completes, and the old objects stay. `--dry-run` refuses the same store at once (`open_read_only` calls `hasher_of`), so the dry-run does not report the same refusal as the real run. This goes against "refusal checks before any write".
**Fix:** Refuse the name in step 1, before the dry-run branch:
```rust
if let Some(h) = header::check(store, Want::Read)?.header {
    header::decide(store, &h, Want::Write)?;
}
content::write_hasher(store)?; // refuses a recorded hash name this build does not know
```
Add a refusal test: a header with an unknown hash line gives an unchanged store tree.

### WR-02: The dry-run counts values again and reads them again on each map cache miss

**File:** `crates/metadata/src/rehash/mod.rs:556-559`, `crates/metadata/src/rehash/mod.rs:231-237`, `crates/metadata/src/rehash/mod.rs:274-302`, `crates/metadata/src/rehash/mod.rs:421`, `crates/metadata/src/rehash/map.rs:190-192`
**Issue:** In `Mode::DryRun` the map is `KeyMap::cache_only()`: 16384 slots, direct-mapped (one slot for each hash value), with no file. When a later key uses the slot of an entry, that entry is lost. A miss is a full recompute: `value()` reads all bytes of the value again, and `done()` adds its other-tag objects and bytes to the counts again.
Failure scenario: a store has 20,000 files. This gives about 80,000 map entries (inode node, directory node, manifest list, content), so most entries are lost before the refcount pass. The refcount pass (`refcounts()` calls `value()` for each row) then reads the bytes of each file a second time and counts them again. Each snapshot root also reads its shared files again. In a real run, these reads are map hits. So "Objects of another hash: N, about B bytes" can be about (1 + snapshots) x 2 times the true number, and the dry-run can read much more data than the real run. The report says that the number is "an estimate", but nothing tells the user that it can be a large overcount.
**Fix:** Use a full map in the dry-run too, but put it outside the store, so the D-22 rule "no temp dir in the store" still holds:
```rust
let tmp = tempfile::tempdir()?; // the system temp dir, not <store>
let map = match &map_dir {
    Some(dir) => KeyMap::on_disk(dir)?,
    None => KeyMap::on_disk(tmp.path())?,
};
```
If the dry-run must not write anywhere, then the report must say that a value shared by roots or rows can be counted more than once, and the bytes must be shown as an upper bound.

## Info

### IN-01: An error that is not a crash leaves temp files and new objects, and the message does not say so

**File:** `crates/slicefs-cli/src/rehash.rs:167-199`, `crates/metadata/src/segment/switch.rs:68-80`
**Issue:** Each `?` between step 3 and step 6 returns before `remove_rehash_tmp` (line 199). The map file stays in `<store>/rehash-tmp`, and the new objects written up to that point stay as garbage. For an error late in the run (for example a damaged object), this can be close to one full copy of the store data. Only the next writable open removes `rehash-tmp`. Only the next GC frees the objects. The message says "Nothing was switched; the store still uses its old root" and gives no other information. A failed write in `write_switch_segment` leaves `segment-N.seg.tmp`. Only a later rehash removes it (`remove_switch_tmps` has no other caller). The loaders ignore it.
**Fix:** On an error before the switch, call `content::remove_rehash_tmp(store)` (best effort). Add "new objects of this run stay until the next `slicefs gc`" to the message. Remove the switch tmp on the error path in `write_switch_segment`.

### IN-02: A rerun after a crash between the rename and the removal leaves the old segments

**File:** `crates/slicefs-cli/src/rehash.rs:175-181`
**Issue:** The rerun finds no root to change, so it does not call `write_switch_segment`, and `remove_segments_before` does not run. The old segments (old root, old snapshot records) stay until the first commit of a mount. The loaders read them correctly (`last_per_version`). `assert_finished` in `rehash_tests.rs` does not check the segment list, so no test finds this.
**Fix:** Also write the switch segment when the root set did not change but more than one `segment-*.seg` file exists. The write is cheap and idempotent. Alternatively, call `remove_segments_before` for the segment that holds the last root.

### IN-03: The switch segment id is not checked for overflow

**File:** `crates/metadata/src/segment/switch.rs:65`
**Issue:** `max_segment_id(dir)? + 1` panics in debug builds and wraps to 0 in release builds if a file `segment-18446744073709551615.seg` exists. In a release build, the switch then gets the lowest id. The old root stays current, and then step 6 runs the GC with the new roots. A real store does not get this id, but the id comes from file names, which the code does not control.
**Fix:** `let id = max_segment_id(dir)?.checked_add(1).ok_or_else(|| SegmentError::from(std::io::Error::other("segment id overflow")))?;`

### IN-04: The crash tests stop the process but do not model a power loss before the sync

**File:** `crates/slicefs-cli/tests/rehash_tests.rs:693-727`, `crates/metadata/src/rehash/mod.rs:1031-1048`
**Issue:** `stop_after` returns an error. The process continues, and `ChunkStore` is dropped in the normal way (`Drop` does not sync, but the written pack bytes stay in the OS page cache). So the step tests show a process crash only. The one test with a power-loss model (`new_objects_survive_a_crash_after_sync`, `MemIo::crash`) runs after `cs.sync()`. No test removes the unsynced pack writes of a rewrite. Then it is not proved that the old root still loads and that a rerun completes, with the torn new data in the packs.
**Fix:** Add an engine test. Run `rehash_roots` in `Mode::Rewrite` without `cs.sync()`, then call `mem.crash()` and open again. Check that the old root gives the same tree. Run `rehash_roots` again and check that the new root is equal to that of a clean run.

---

_Reviewed: 2026-11-26T16:24:12Z_
_Reviewer: Claude (gsd-code-reviewer)_
_Depth: deep_
