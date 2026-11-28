---
phase: 26
depth: standard
status: issues_found
findings:
  critical: 0
  warning: 4
  info: 7
---

# Phase 26 code review

Two parts: A (store.rs, pmap.rs) and B (other product code). IDs are prefixed A- and B-.

Fix status (2026-11-28): all 11 findings are fixed, on branch `exec/26-fix`. Each finding has a "Fix status" line. (The front-matter `status` keeps the review result.)

# Part A

---
phase: 26-prolly-maps-in-commit-read-and-gc
reviewed: 2026-11-28T00:00:00Z
depth: standard
files_reviewed: 2
files_reviewed_list:
  - crates/metadata/src/store.rs
  - crates/metadata/src/pmap.rs
findings:
  critical: 0
  warning: 3
  info: 2
  total: 5
  necessity: 0
status: issues_found
---

# Phase 26: Code Review Report (part A: store.rs, pmap.rs)

**Reviewed:** 2026-11-28
**Depth:** standard (cross-file checks: `dirent.rs`, `xattr.rs`, `inode.rs`, `gc/`, `prolly::count/height`, `slicefs-cli` GC start, commit cut and reclaim paths)
**Files Reviewed:** 2
**Status:** issues_found

## Summary

Scope: the diff `bd6e2b4..HEAD` of `crates/metadata/src/store.rs` and `crates/metadata/src/pmap.rs` (non-test code: `store.rs` lines 1-2194, `pmap.rs` lines 1-626).

These parts were traced and hold:

- **Overlay, flush and commit (no lost update).** `read_with_generation` reads the generation before it checks the overlay. `Tree::install` bumps the generation before `settle` removes an entry. `modify` checks the overlay, then the generation, under the shard lock. So a tree value is used only if no install came after the read. The ticket scheme keeps a put that comes after a take. `put` and `modify` drop the in-flight ticket, so `settle` does not remove the newer value. `requeue` runs only while no other take is in flight: `apply_lock` covers a flush, and `prepared` together with `commit_lock` covers a commit. The invariant `map ⊆ dirty ∪ flushing` holds on every path.
- **Commit cut.** `prepared` is set and read only under `apply_lock`, so no flush can run between the takes and the install. The install of all five trees has no fallible step, so `installed` is never partly true. The root is logged only after `cs.sync()` and `pre_root_sync`. A failure after the install loses nothing, because the next commit applies its edits to the installed working roots.
- **Lock order.** `commit_lock` (Rank::Rename) comes before `apply_lock`, and `apply_lock` comes before a shard lock and then a root lock. `insert_inode` takes `inode_alloc` and then one shard. No closure that runs under a shard lock does I/O or takes another lock. `flush_trees` runs only after the shard lock is released.
- **Online GC.** `gc_start_mode` (filesystem.rs:2368-2424) commits under the exclusive gate after `gc_begin`. Every node that is reachable from a later working root is therefore either in that committed root or put after `gc_begin`.
- **Whole map in RAM.** No hot path loads a whole map. `tree_stats` uses `prolly::count/height`, which read the root node only (prolly/src/range.rs:44-57).
- **Bad on-disk data.** The root record length is checked before any field is sliced. Tree values with a bad length or tag decode to `Corrupted`. Bad keys (`name_of`, `from_encoded`) are errors, not panics.

The findings below are error-path drift, one place that breaks the read protocol, and two quality items. No BLOCKER was found.

## Narrative Findings (AI reviewer)

## Warnings

### WR-01: `delete_inode` deletes the inode first, then exits early on an error: the inode and logical-byte counts drift for good

**File:** `crates/metadata/src/store.rs:1583-1607`
**Issue:** The read-modify-write at 1583-1586 writes the pending delete of the inode first. Two fallible scans follow it: `self.delete_xattrs(ino)?` (1590) and `self.delete_dir_entries(ino)?` (1594). Either scan can fail on a tree read error (`MetaError::Io` or `Corrupted` from `scan_keys` or `DirKey::from_encoded`). In that case the function returns before the `logical_bytes` and `inode_count` decrements (1598-1607). The inode is already gone, so a retry gets `NotFound` and the decrements never run. The wrong counts go into every later root record (`finish_inner` 1236-1237), and stats reads them from there. A directory whose scan failed also keeps its `.`/`..` entries, so `is_dir(ino)` stays true for a deleted inode. The callers make this worse: `reclaim_inode` (filesystem.rs:2293) and `make_directory`'s undo path (store.rs:915) both ignore the error with `let _ =`. This is new in this phase: the base version removed whole map entries in RAM and could not fail at this point.
**Fix:** Apply the count changes in the same step that deletes the inode, before the fallible scans. Then a later error leaves only leftover entries, which the next delete or a scrub can clean up:
```rust
let (size, mode) = self.inodes.read_modify_write(&self.trees, &ino, |cur| match cur {
    None => (None, Err(MetaError::NotFound(ino))),
    Some(m) => (Some(None), Ok((m.size, m.mode))),
})??;
self.add_size(size, 0);
self.inode_count.try_update(Ordering::Relaxed, Ordering::Relaxed, |c| Some(c.saturating_sub(1))).ok();
self.manifest.write(ino, None);
let xr = self.delete_xattrs(ino);
let dr = if mode & S_IFMT == S_IFDIR { self.delete_dir_entries(ino) } else { Ok(()) };
self.flush_trees();
xr.and(dr)
```

**Fix status:** fixed in `6d81c0f`. The counts change in the same step as the inode delete, both scans run, and the error is returned. `reclaim_inode` and both undo paths log the error; `reclaim_inode` releases the content only if the inode is gone. Test: `a_failed_delete_scan_still_counts_the_delete` (failed before the fix).

### WR-02: A refcount tree read error drops the change silently, and the "next commit fails" safeguard is used up after the install

**File:** `crates/metadata/src/store.rs:482-491` (with `948-954` and `1244-1263`)
**Issue:** If `read_with_generation` fails in `change_refcount`, the increment or decrement is not applied. The code logs a warning and stores the error in `put_error`, and `increment_refcount` / `decrement_refcount` return `()`, so the caller goes on as if the count changed. The comment says the error is "kept for the next commit". But `begin_put_sync` (948-954) runs only after the trees are installed and settled (1244-1253), and it calls `take()` on the error. So the next commit fails one time, and its edits are already in the working roots. The commit after that succeeds and persists a count that is off by one. Nothing ever applies the dropped change again. The GC is mark-based, so this does not delete live content. But the counts are reported (scrub `saturated_refcount_count`, stats), and they drift for good after one transient read error. The base version had in-RAM counts and no such error path.
**Fix:** Do not drop the change. Use one of these options:
(a) Keep the error in the overlay: write a marker, or keep the digest in a "retry" set that the commit's take processes again, so the change is applied when the tree can be read.
(b) Make the error sticky. Do not `take()` it in `begin_put_sync`. Clear it only after a check (for example a scrub recount).
(c) At minimum, check `put_error` in `finish_inner` *before* the install block. The commit then requeues instead of installing, and the error is reported where the doc says.

**Fix status:** fixed in `3b0d4d6` (a sticky error, option b with the check of option c). The error goes into a new field `refcount_error`, which is never cleared. `finish_commit` checks it before any node write or install, so every later commit of the store fails and its edits are dirty again. A reload replays the write log on the last good root. `put_error` (node put errors) is not changed. Test: `a_dropped_refcount_change_is_never_committed` (failed before the fix).

### WR-03: `MapTree::for_each_value` reads the root before the overlay, which breaks the read rule in the module doc

**File:** `crates/metadata/src/pmap.rs:484-485`
**Issue:** The module doc (lines 12-16) says: "Never read the root first and the overlay after: an entry leaves the overlay only after its tree root is installed". `for_each_value` does the opposite. It reads `self.tree.root()` at 484, then copies `self.overlay.pending()` at 485. A flush between the two lines can install a new root and settle key `k`. Then `k` is in neither the old root's new value nor the pending copy: the scan gives the stale tree value of `k`, or misses `k` if it is new. A flush is a normal hot-path event, and the only product caller, `saturated_refcount_count` (store.rs:517-521, used by `slicefs scrub`), runs on a live store. So the reported count can be wrong. The doc comment says "exact when no write, flush or commit runs meanwhile", but if the two reads are swapped, the result stays correct across a flush. The ordering is the actual defect.
**Fix:** Copy the overlay first, then read the root. A key that is settled after the copy is still in `pending`, and its tree entry is skipped because `pending.contains_key`:
```rust
let pending: HashMap<Vec<u8>, Option<V>> = self.overlay.pending().into_iter().map(|(k, v)| (k.encode(), v)).collect();
let root = self.tree.root();
```

**Fix status:** fixed in `379e8f4`. The overlay is copied first, then the root is read; the doc texts are updated. Test: `for_each_value_sees_each_key_once_across_a_flush` (a test read hook flushes between the two reads; failed before the fix).

## Info

### IN-01: The inode number in an inline inode record is not checked against its tree key

**File:** `crates/metadata/src/store.rs:791-793`, `809` (decode in `crates/metadata/src/inode.rs:61`)
**Issue:** The inline record carries its own `ino` field. `load_meta` and `read_modify_write` return the decoded record without checking that `m.ino` equals the key. A corrupt record (ino 7 stored under key 5) has two effects. (a) In debug builds, `modify_inode`'s `debug_assert_eq!(m.ino, ino)` at 809 panics on bad on-disk data. (b) In release builds, `get_inode(5)` returns `ino: 7`. A caller that does `update_inode(&meta)` then changes inode 7, which makes the corruption worse. `inode_leaf` (1888-1895) in the GC walk checks only the lengths.
**Fix:** In `MapTree<u64, InodeMeta>` reads (or in a store wrapper around `inodes.read` / `read_modify_write`), return `MetaError::Corrupted("inode_data tree: record of ino X under key Y")` when `m.ino != key`. Replace the `debug_assert` with that error.

**Fix status:** fixed in `b42becf`. `checked_inode` returns `Corrupted` for a record of another number; `get_inode`, `modify_inode`, `delete_inode` and `link` use it. The `debug_assert` stays, but only checks that the closure keeps the number. The GC leaf check is not changed (a wrong number does not change what is reachable). Test: `an_inode_record_under_another_key_is_corrupt`.

### IN-02: `tree_nodes` is a public function that builds an O(nodes) list of every tree node, and only tests call it

**File:** `crates/metadata/src/store.rs:1807-1827`
**Issue:** `tree_nodes` collects the ID of every node of all five trees into one `Vec` (each node once per parent). At the 1B-inode design target this list holds tens of millions of keys, which breaks the "no whole map in RAM" rule. Its only callers are in `#[cfg(test)]` modules (`gc/mod.rs:453`, `rehash/mod.rs:854`). But it is exported as `pub`, so product code can start to use it without any warning.
**Fix:** Make it `#[cfg(test)] pub(crate)` (or `#[doc(hidden)]` with a doc line saying it is O(nodes) and for tests only), or build it on top of `for_each_referenced` with a callback.

**Fix status:** fixed in `b42becf`. Integration tests in `crates/metadata/tests` and `crates/slicefs-cli/tests` use it, so it cannot be `cfg(test)`. It is now `#[doc(hidden)]` with a tests-only doc line.

---

_Reviewed: 2026-11-28_
_Reviewer: Claude (gsd-code-reviewer)_
_Depth: standard_


# Part B

---
phase: 26-prolly-maps-in-commit-read-and-gc
reviewed: 2026-11-28T00:00:00Z
depth: standard
files_reviewed: 32
files_reviewed_list:
  - crates/metadata/src/rehash/mod.rs
  - crates/metadata/src/rehash/map.rs
  - crates/metadata/src/xattr.rs
  - crates/metadata/src/dirent.rs
  - crates/metadata/src/directory.rs
  - crates/metadata/src/inode.rs
  - crates/metadata/src/store_io.rs
  - crates/metadata/src/gc/mod.rs
  - crates/metadata/src/testkit.rs
  - crates/metadata/src/lib.rs
  - crates/prolly/src/cache.rs
  - crates/prolly/src/node.rs
  - crates/prolly/src/tree.rs
  - crates/prolly/src/visit.rs
  - crates/prolly/src/range.rs
  - crates/prolly/src/apply.rs
  - crates/prolly/src/lib.rs
  - crates/slicefs-cli/src/filesystem.rs
  - crates/slicefs-cli/src/handlers.rs
  - crates/slicefs-cli/src/mount.rs
  - crates/slicefs-cli/src/content.rs
  - crates/slicefs-cli/src/cli.rs
  - crates/slicefs-cli/src/fuse_callbacks.rs
  - crates/slicefs-cli/src/header.rs
  - crates/slicefs-cli/src/scrub.rs
  - crates/slicefs-cli/src/rehash.rs
  - crates/slicefs-cli/src/main.rs
  - crates/chunk-store/src/lock_stats.rs
  - vendor/fuser/src/ll/request.rs
  - vendor/fuser/src/lib.rs
  - scripts/prolly-bench/pmap07.py
  - scripts/prolly-bench/pmap07.sh
findings:
  critical: 0
  warning: 1
  info: 5
  total: 6
  necessity: 0
status: issues_found
---

# Phase 26: Code Review Report (part B)

**Reviewed:** 2026-11-28
**Depth:** standard
**Files Reviewed:** 32 (30 source files, and 2 bench scripts checked for verdict logic only)
**Status:** issues_found

## Summary

Scope: the changed product code of Phase 26, except `store.rs` and `pmap.rs`
(another reviewer has them). Diff base `bd6e2b4`. `directory.rs` is deleted;
the review checked that nothing still uses it.

I found no Critical issue. I checked these points against the source:

- **Decode bounds.** `prolly::node::decode` checks every length before it
  slices, and checks the count before it allocates. `DirKey::from_encoded`,
  `dirent::name_of`, `DirValue::decode`, `XattrKey::from_encoded`,
  `xattr::name_of`, `XattrValue::decode` and `deserialize_inode` check the
  length first. The rehash entry checks (`k.len()`, `v.len()`) come before
  every `ObjectKey::read` and every slice. I found no panic on bad data.
- **getxattr and listxattr.** A `size` of 0 reads no value byte. ERANGE comes
  from the length, before the value is read. The "one byte more" read stops
  a value that was replaced in the meantime from being cut.
  `FileReader::read` (`chunk-store/src/file.rs:271-272`) caps the allocation
  to the value, so `size + 1` and `usize::MAX` are safe. `read_range`
  clamps an inline value and returns nothing past the end. listxattr fills
  the buffer from the scan and stops at the first name that does not fit.
- **Node cache.** The charge is the same on insert and on evict. The cap is
  never passed. A hit is checked against the config that asks for it
  (`fits`), and against the parent entry (`load_with`). A node goes into the
  cache only after those checks pass. The lock is a leaf.
- **Range cursor.** I traced `seek_in` and `Cursor::next` with `end`. The
  `lo` bounds and the resume index are correct.
- **Rehash.** All five trees are rebuilt with the store salt. The rebuild
  translates the manifest lists and the spilled xattr values. It copies the
  inode and directory entries, which hold no key. The refcount rows are
  sorted by their new key and merged. The root record has 197 bytes: five
  keys and four u64 fields, so no key field is left out. `TmpDir` makes a
  unique `pid-N` subdirectory, so it cannot collide with the map file.
- **Online GC.** `set_xattr` writes the spilled values before any root refers
  to them. This is safe: `gc_begin` comes first, then a commit under the
  exclusive gate (`filesystem.rs:2378-2405`). The GC mark adds tree nodes
  directly, and `referenced_estimate` leaves them out of the sort estimate.
- **Whole map or value in RAM.** On the FUSE paths, I found none.
  `get_xattr`, `list_xattrs`, `list_directory`, `test_readdir` and
  `test_getxattr` still build whole lists or values, but only tests (and
  `stats`, which is accepted) call them.
- **pmap07.py.** The verdict code matches `runs/pmap07-rule.md`. The script
  still requires check C, which agrees with the owner amendment (the script
  stays frozen). The recorded run passes all three checks anyway.

Necessity rubric (`necessity-review.md`): I read it. No finding met its
evidence rule.

## Narrative Findings (AI reviewer)

## Warnings

### WR-01: readdir offsets are entry indexes, so a change between two pages skips or repeats entries

**File:** `crates/slicefs-cli/src/filesystem.rs:5174-5185`
**Issue:** `readdir_entries` counts entries from the start of the directory
scan. It skips `offset` entries and gives the next entry the offset `index`.
Suppose a program reads one page, then unlinks or creates a name that sorts
before its position, then reads the next page. The index then points at a
different entry. One entry that did not change is skipped or returned twice.
POSIX requires that each entry that is neither added nor removed comes back
exactly once. Example: a tool that deletes each page of entries as it reads
them, and does not reopen the directory, leaves entries behind. Your accepted
list covers only the O(n²/page) cost of these offsets, not this result. This
is **not a regression**: the base used `entries.iter().skip(offset as usize)`
(`bd6e2b4:crates/slicefs-cli/src/filesystem.rs:5125`). But the readdir path
was rewritten in this phase, and it now streams in name byte order, so a
fix is easy now.
**Fix:** Use a name cookie, which is the deferred stretch item. Keys are now
in name byte order. Use the offset to resume from a name: for example, keep a
small table per open directory handle that maps the offset to the last name
returned, or put the cookie in a stable 63-bit hash of the name. Then start
`scan_keys` just after that name (`name` + `\0`). Until that is done, record
the behavior with the known limits.

**Fix status:** fixed in `cadb30f`. Each open directory handle keeps a resume cursor: the offset and name of each entry of its last reply (the whole reply, because the kernel can use part of it). A readdir at one of those offsets resumes the scan after that name; offset 0 starts at the start, and any other offset uses the old skip. `releasedir` drops the cursor. Docs updated (`limits.md`, `metadata-maps.md`). Tests: `readdir_pages_give_each_unchanged_name_once_across_changes` (failed before the fix), `readdir_resumes_inside_the_last_reply`, and the new `mt-linux.sh` step `readdir-unlink` (made stronger in the next commit: it fails in Docker on the code before the fix and passes on the fix).

## Info

### IN-01: The doc comment of `new_inode` is now on `dt_file_type`

**File:** `crates/slicefs-cli/src/filesystem.rs:64-67`
**Issue:** `dt_file_type` was put between the doc comment of `new_inode` and
`new_inode`. Its rustdoc now begins with the text about `create_inode`, and
`new_inode` has no doc.
**Fix:** Move lines 64-65 ("The `create_inode` of a create ...") down, to
just above `fn new_inode` (line 81).

**Fix status:** fixed in `05b40ae`.

### IN-02: `handle_readdir` and `ReaddirResult` are left without a product caller, and they build a whole listing

**File:** `crates/slicefs-cli/src/handlers.rs:40-45`, `:157-167`; `crates/slicefs-cli/src/filesystem.rs:5151-5158`
**Issue:** The FUSE callback now streams through `readdir_entries`. Only
tests call `handle_readdir`, which calls `test_readdir` and collects the
whole directory into a `Vec` (the `mount.rs:1285` import is in `mod tests`).
They are still `pub`, so new product code could call them and bring back an
O(entries) listing.
**Fix:** Mark `handle_readdir`, `ReaddirResult` and `test_readdir`
`#[cfg(test)]`, or move them into the test modules.

**Fix status:** fixed in `05b40ae`. Integration tests and the stress harness use them, so they are `#[doc(hidden)]` with a tests-only doc line, not `cfg(test)`.

### IN-03: The errnos of a long xattr name are not the same on get and set

**File:** `crates/slicefs-cli/src/filesystem.rs:2202-2208`
**Issue:** `getxattr_errno` maps `NameTooLong` to ERANGE, the Linux errno.
setxattr and removexattr go through `meta_error_to_errno` and give
ENAMETOOLONG. macOS uses ENAMETOOLONG for getxattr too. The kernel checks the
name length before FUSE, so this is rarely seen.
**Fix:** Choose the errno per platform (ERANGE on Linux, ENAMETOOLONG on
macOS) in one helper, and use that helper for all four xattr calls.

**Fix status:** fixed in `05b40ae`. One helper, `xattr_errno`, gives ERANGE for a long name on getxattr, setxattr and removexattr (the Linux errno, as asked; not per platform). `getxattr_name_errors` checks all three.

### IN-04: Comments in `inode.rs` are out of date after the move to inline records

**File:** `crates/metadata/src/inode.rs:28-31`
**Issue:** The comment says that the 56-byte form keeps "their CAS digests,
so stores written before the extension load unchanged". Inode records are now
inline tree values with no digest, and format 8 does not read older stores.
The buffer is `Vec::with_capacity(60)`, but the record has 56 or 72 bytes.
**Fix:** Remove the CAS sentence. Use `with_capacity(72)`.

**Fix status:** fixed in `05b40ae`.

### IN-05: pmap07.py accepts 11 samples per tree, but the rule asks for 22

**File:** `scripts/prolly-bench/pmap07.py:46`, `:100-102`
**Issue:** The rule file says that each tree's numbers are the median over 22
samples (two rounds). The script refuses only when a tree has fewer than 11
samples, so a run with only one round would also get a verdict. The recorded
decision run had 22 samples per tree (`runs/pmap07/commit-*.log`, 11 lines in
each of 4 files), so the verdict stands.
**Fix:** None is needed for this phase, because the script is frozen. If
the script is used again, require `MIN_SAMPLES * 2` samples per side, or
check that the samples come from two rounds.

**Fix status:** fixed in `e588729`. A verdict needs 22 samples per tree (`--summary` still needs 11). The script run again on `runs/pmap07` gives output identical to `verdict.txt`, so the recorded verdict (pass) does not change.

---

_Reviewed: 2026-11-28_
_Reviewer: Claude (gsd-code-reviewer)_
_Depth: standard_
