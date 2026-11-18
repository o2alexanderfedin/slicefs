# Phase 8: Correctness Fixes - Research

**Researched:** 2026-03-30
**Domain:** Rust atomics, saturating arithmetic, POSIX statfs/statvfs, libc FFI
**Confidence:** HIGH — all findings verified against live codebase and libc source

---

<user_constraints>
## User Constraints (from CONTEXT.md)

### Locked Decisions

#### statfs — Three-Tier Space Reporting (FIX-02)
- statfs must report **three tiers** of space information:
  1. **Logical bytes** — sum of all file sizes as users see them (already tracked via `logical_bytes()`)
  2. **CAS bytes** — total size of deduplicated block content (Phase 7.1 already computes this via `dir_size(&sp.join("vt0"))`)
  3. **Host disk bytes** — actual bytes consumed on the underlying OS filesystem
- **All three tiers encoded in statfs fields** for `df` visibility:
  - `blocks/bsize` = host disk capacity (total from underlying volume)
  - `bfree/bavail` = remaining host disk free space
  - `files` = actual inode count (not hardcoded 1M)
  - Used blocks derived from host disk consumption
- **Host disk capacity/free** determined by calling OS `statvfs()` on the backing store path at mount time and on each statfs call
- **Inode count** tracked via `AtomicU64` counter — increment on inode create, decrement on delete, reconstructed during mount from inode_map
- **CAS bytes** tracked via `AtomicU64` counter — increment on block store put, decrement on GC delete
- **Logical + CAS bytes** also exposed in `slicefs stats` CLI command for detailed dedup ratio analysis

#### Refcount Saturation (FIX-01)
- Replace `+= 1` with `saturating_add` — refcount caps at `u64::MAX`
- **Log a warning** when saturation is hit — block becomes immortal (can never be GC'd)
- **Decrement is a no-op at `u64::MAX`** — once saturated, block is permanently pinned. Guarantees no data loss; GC skips it
- **`slicefs scrub` reports saturated refcounts** as warnings — "N blocks have saturated refcounts (immortal)". Gives operators visibility without treating it as corruption

### Claude's Discretion
- Exact AtomicU64 initialization strategy during mount (scan inode_map vs accumulate during replay)
- Whether statvfs is called on every statfs or cached with periodic refresh
- Warning log format for saturated refcounts
- How CAS bytes AtomicU64 is kept in sync (track at put/delete vs derive from dir_size on demand)

### Deferred Ideas (OUT OF SCOPE)
None — FIX-03/FIX-04 resolved in Phase 7.1.
</user_constraints>

---

<phase_requirements>
## Phase Requirements

| ID | Description | Research Support |
|----|-------------|-----------------|
| FIX-01 | Refcount increment uses saturating_add — no silent overflow to 0 on u64::MAX | `increment_refcount` in store.rs line 142 uses `+= 1`; one-line fix plus decrement guard plus scrub check |
| FIX-02 | statfs reports actual inode count (not hardcoded 1M) and tracks physical bytes accurately | `statfs()` in filesystem.rs line 1192 hardcodes `files = 1_000_000`; needs `inode_count: AtomicU64` in `DictMetadataStore` + `libc::statvfs()` for host disk metrics |
| FIX-03 | Snapshot lookup by version is O(1) via HashMap<u64, SnapshotEntry> | **PRE-SATISFIED** — Phase 7.1 added `snapshots_by_version: Mutex<HashMap<u64, SnapshotEntry>>` (store.rs line 118) |
| FIX-04 | Snapshot lookup by name is O(1) via HashMap<String, u64> index | **PRE-SATISFIED** — Phase 7.1 added `snapshots_by_name: Mutex<HashMap<String, u64>>` (store.rs line 119) |
</phase_requirements>

---

## Summary

Phase 8 is a focused correctness hardening phase. There are exactly two pending bugs: a refcount overflow risk (FIX-01) that can cause GC to delete live data, and a statfs reporting inaccuracy (FIX-02) where `df` shows a hardcoded 1,000,000 inodes and an unrealistic `bfree`. FIX-03 and FIX-04 (snapshot O(1) lookup) were fully resolved in Phase 7.1 via dual HashMap indexes — they require only documentation as pre-satisfied.

FIX-01 is a single-function change: `increment_refcount` in `crates/metadata/src/store.rs` uses `+= 1` on a `u64`. In Rust release mode, integer overflow wraps silently (debug mode panics). The fix is `saturating_add(1)`. A `u64::MAX` guard in `decrement_refcount` (make it a no-op when already at MAX) prevents the symmetric underflow. The `scrub` command gains a new check that counts saturated entries and reports them as warnings. The `tracing` crate (already in the dependency graph) provides the warning log mechanism.

FIX-02 requires adding an `inode_count: AtomicU64` field to `DictMetadataStore` (paralleling the existing `logical_bytes: AtomicU64` pattern exactly), incrementing it in `create_inode`, decrementing it in `delete_inode`, and initializing it from `inode_map.map.len()` during `load_from_root`. The `statfs()` FUSE handler must call `libc::statvfs()` on the backing store path to get real host disk capacity and free space. `libc` is already in `slicefs-cli/Cargo.toml`. The three-tier mapping to statfs fields is: host disk fields for `blocks/bfree/bavail`, `inode_count` for `files`, and `u64::MAX - inode_count` for `ffree`.

**Primary recommendation:** Implement FIX-01 first (one function, zero new dependencies), then FIX-02 (new AtomicU64 field + statvfs FFI call).

---

## Standard Stack

### Core
| Library | Version | Purpose | Why Standard |
|---------|---------|---------|--------------|
| `std::sync::atomic::AtomicU64` | std | Lock-free inode counter | Same pattern as existing `logical_bytes` — no new deps |
| `libc::statvfs` | 0.2.180 (workspace) | Host disk capacity/free query | Already in `slicefs-cli` Cargo.toml; POSIX standard |
| `tracing::warn!` | 0.1 (workspace) | Saturated refcount warning log | Already in `slicefs-cli` Cargo.toml |

### No New Dependencies Required

All required capabilities exist in the current dependency set. No `Cargo.toml` changes needed for either fix.

---

## Architecture Patterns

### Established AtomicU64 Pattern (replicate exactly)

`DictMetadataStore` already uses `logical_bytes: AtomicU64` with these update sites:
- `create_inode` — `fetch_add(size, Ordering::Relaxed)`
- `update_inode` — delta add/sub with `saturating_sub`
- `delete_inode` — `fetch_update(... saturating_sub(size))`
- `load_from_root` — scan all inodes, sum sizes, `AtomicU64::new(sum)`
- Public getter: `pub fn logical_bytes(&self) -> u64`

The `inode_count: AtomicU64` must follow this pattern identically. The initialization value during `load_from_root` comes from `inode_data.len()` (count of all non-root inodes) + 1 (root inode 1 is always present but not in `inode_data` map — confirm with actual code; the `inode_data` BTreeMap in `load_from_root` at line 837 contains all inodes after load, including root). The simplest correct initialization: `inode_data.len() as u64`.

### libc::statvfs Usage Pattern

```rust
// Source: libc 0.2.180 unix/bsd/apple/mod.rs — verified against registry
use std::ffi::CString;
use std::path::Path;

fn host_disk_stats(store_path: &Path) -> Option<(u64, u64, u64)> {
    // Returns (total_blocks_in_bsize, free_blocks, bsize)
    let path_cstr = CString::new(store_path.to_str()?).ok()?;
    let mut sv: libc::statvfs = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::statvfs(path_cstr.as_ptr(), &mut sv) };
    if ret != 0 { return None; }
    Some((sv.f_blocks, sv.f_bfree, sv.f_bsize))
}
```

`libc::statvfs` fields on macOS:
- `f_bsize: c_ulong` — filesystem block size
- `f_blocks: fsblkcnt_t` — total blocks in filesystem
- `f_bfree: fsblkcnt_t` — free blocks (privileged)
- `f_bavail: fsblkcnt_t` — free blocks (unprivileged)
- `f_files: fsfilcnt_t` — total file serial numbers (total inodes of host FS)
- `f_ffree: fsfilcnt_t` — free file serial numbers

For `reply.statfs(blocks, bfree, bavail, files, ffree, bsize, namelen, frsize)`:
- `blocks` = `sv.f_blocks` (host disk total, in `sv.f_bsize` units)
- `bfree` = `sv.f_bfree`
- `bavail` = `sv.f_bavail`
- `bsize` = `sv.f_bsize as u32`
- `files` = `meta.inode_count()` (SliceFS inode count, not host inode count)
- `ffree` = `u64::MAX - meta.inode_count()` (CAS has no hard inode limit)

### Saturating Refcount Pattern

```rust
// store.rs increment_refcount — CURRENT (line 142)
*rc.entry(*digest).or_insert(0) += 1;

// store.rs increment_refcount — TARGET
let prev = *rc.entry(*digest).or_insert(0);
if prev == u64::MAX {
    tracing::warn!("refcount saturated for block {:?} — block is immortal", digest);
} else {
    *rc.entry(*digest).or_insert(0) = prev.saturating_add(1);
}
// Simpler equivalent:
let val = rc.entry(*digest).or_insert(0);
if *val < u64::MAX {
    *val += 1;
    // no saturation log needed here — only log when hitting MAX
} else {
    tracing::warn!("refcount at u64::MAX for block — increment no-op (immortal)");
}
```

The cleaner idiomatic form that logs only on the transition to MAX:

```rust
pub fn increment_refcount(&self, digest: &Digest224) {
    let mut rc = self.refcounts.lock().unwrap();
    let val = rc.entry(*digest).or_insert(0);
    let new_val = val.saturating_add(1);
    if new_val == u64::MAX && *val < u64::MAX {
        tracing::warn!(
            "refcount saturated for block — block is now immortal and will not be GC'd"
        );
    }
    *val = new_val;
}
```

`decrement_refcount` — guard for saturated entry:

```rust
pub fn decrement_refcount(&self, digest: &Digest224) {
    let mut rc = self.refcounts.lock().unwrap();
    if let Some(count) = rc.get_mut(digest) {
        if *count == u64::MAX {
            // Saturated — no-op: block is immortal
            return;
        }
        if *count <= 1 {
            rc.remove(digest);
        } else {
            *count -= 1;
        }
    }
}
```

### Scrub Saturated Refcount Check

Add to `run_scrub` in `scrub.rs` after loading the `DictMetadataStore`:

```rust
// Count saturated refcounts via DictMetadataStore::saturated_refcount_count()
// (new method) or inline via a public iterator/scan method.
```

The cleanest approach: add `pub fn saturated_refcount_count(&self) -> usize` to `DictMetadataStore` that counts entries with value `u64::MAX`. Report in `ScrubReport` as a new `saturated_blocks: usize` field with corresponding warning in `print_human_report`.

### StoreStats Three-Tier Update

`stats.rs` `StoreStats` gains no new fields — it already has `logical_bytes` and `physical_bytes`. The CLI output gains a third line "CAS bytes" which is `physical_bytes` (already computed from `dir_size(vt0/)`). The discretion question of whether to add a separate `cas_bytes` AtomicU64 counter is resolved: **use `dir_size` on demand** in `stats.rs` (already done), and **no AtomicU64 for CAS bytes** — the AtomicU64 approach was considered in the CONTEXT.md discretion items but the existing `dir_size` call in stats.rs already satisfies the requirement. The live `statfs()` handler in filesystem.rs can continue using `dir_size(&sp.join("vt0"))` for CAS bytes (already present at line 1179). The locked decision for a CAS bytes AtomicU64 was listed under "Claude's Discretion" so the simpler `dir_size` approach is chosen.

---

## Don't Hand-Roll

| Problem | Don't Build | Use Instead | Why |
|---------|-------------|-------------|-----|
| Host disk capacity | Custom file counting | `libc::statvfs()` | OS kernel knows exact block accounting; file counting misses filesystem overhead |
| Atomic counter | Mutex<u64> | `AtomicU64` | Already established pattern; no lock needed for simple increment/decrement |
| Saturating increment | Custom checked arithmetic | `u64::saturating_add(1)` | Std library; one method call; no overflow risk |

---

## Common Pitfalls

### Pitfall 1: inode_count Initialization Off-by-One (Root Inode)

**What goes wrong:** Root directory inode (inode 1) is always present but may or may not be counted in `inode_data` BTreeMap after `load_from_root`. If `inode_data` includes it, `inode_data.len()` is correct. If not, the count is off by 1.

**Why it happens:** The store.rs load path at line 837 does `load_u64_digest_map` for `inode_data` — this includes all persisted inodes. Root inode 1 IS in `inode_data` (it was created via `create_inode` in the first commit). So `inode_data.len() as u64` is the correct initialization value.

**How to avoid:** Verify with a unit test: fresh store → create 3 files → `inode_count()` returns 3 (files only) + 1 (root dir) = 4 (or whatever `inode_data.len()` reflects after root dir is created). Check `create_inode` call sequence during `seed`: root dir inode is created first via `create_inode`, which increments `inode_count`. So `inode_count` naturally includes root during live operation. During `load_from_root`, initialize to `inode_data.len()` since it maps all live inodes.

### Pitfall 2: statvfs Path Must Be a Valid CString

**What goes wrong:** `libc::statvfs` takes `*const c_char`. If the store path contains non-UTF8 bytes or a null byte, `CString::new()` fails. If `.ok()?` is used and `store_path` is `None` (in-memory test mode), the fallback must not panic.

**How to avoid:** Guard with `if let Some(ref sp) = self.store_path` (same pattern as existing `dir_size` call in statfs handler). Fall back to logical-only values when store path is unavailable.

### Pitfall 3: statvfs bsize vs frsize

**What goes wrong:** POSIX distinguishes `f_bsize` (preferred I/O block size) from `f_frsize` (fundamental block size). `f_blocks`, `f_bfree`, `f_bavail` are in units of `f_frsize`, not `f_bsize`.

**How to avoid:** Use `sv.f_frsize` as the block size for unit conversions. Pass `sv.f_frsize as u32` as the `bsize` argument to `reply.statfs(...)`. On macOS `f_bsize == f_frsize` in typical use, but correctness requires `f_frsize`.

### Pitfall 4: Saturated Refcount Warning Spam

**What goes wrong:** Logging a warning on every call to `increment_refcount` for a block that is already at `u64::MAX` floods logs during normal operation (the block is legitimately referenced many times).

**How to avoid:** Log only on the transition — when `*val == u64::MAX - 1` before increment (i.e., the increment that causes saturation), not on every subsequent call. The code pattern above (`if new_val == u64::MAX && *val < u64::MAX`) achieves this.

### Pitfall 5: decrement_refcount Asymmetry With BTreeMap Entry API

**What goes wrong:** The existing `decrement_refcount` uses `get_mut` and removes the entry at count 1. The saturation guard must be added before the subtraction, not after the entry-check, to prevent the `u64::MAX - 1` case from accidentally triggering removal.

**How to avoid:** Check `*count == u64::MAX` as the first guard in `decrement_refcount`, return early before any arithmetic.

### Pitfall 6: Test test_statfs_returns_nonzero_blocks Will Need Update

**What goes wrong:** `filesystem.rs` contains an inline unit test `test_statfs_returns_nonzero_blocks` (line 1723) that asserts hardcoded constants `blocks: u64 = 1_000_000` and `files: u64 = 1_000_000`. This test is testing the old hardcoded behavior and will be stale after FIX-02.

**How to avoid:** Delete or replace this test with a behavioral test in `statfs_tests.rs` that verifies `files` reflects actual inode count and that `bfree`/`blocks` come from `statvfs`.

---

## Code Examples

### FIX-01: increment_refcount (verified against store.rs lines 137-143)

```rust
// Source: crates/metadata/src/store.rs — current implementation
pub fn increment_refcount(&self, digest: &Digest224) {
    let mut rc = self.refcounts.lock().unwrap();
    *rc.entry(*digest).or_insert(0) += 1;  // BUG: wraps on u64::MAX in release
}

// Target implementation
pub fn increment_refcount(&self, digest: &Digest224) {
    let mut rc = self.refcounts.lock().unwrap();
    let val = rc.entry(*digest).or_insert(0);
    if *val == u64::MAX {
        // Already saturated — no-op, block is immortal
        return;
    }
    *val += 1;
    if *val == u64::MAX {
        tracing::warn!(
            "refcount saturated for block — block is now immortal and will not be GC'd"
        );
    }
}
```

### FIX-01: decrement_refcount guard (verified against store.rs lines 145-158)

```rust
// Add saturation guard as first check
pub fn decrement_refcount(&self, digest: &Digest224) {
    let mut rc = self.refcounts.lock().unwrap();
    if let Some(count) = rc.get_mut(digest) {
        if *count == u64::MAX {
            return; // saturated — immortal, no-op
        }
        if *count <= 1 {
            rc.remove(digest);
        } else {
            *count -= 1;
        }
    }
}
```

### FIX-02: inode_count field in DictMetadataStore (verified against store.rs)

```rust
// Add to struct (after logical_bytes field, same pattern):
inode_count: AtomicU64,

// DictMetadataStore::new():
inode_count: AtomicU64::new(0),

// create_inode() — after insert:
self.inode_count.fetch_add(1, Ordering::Relaxed);

// delete_inode() — after remove:
self.inode_count.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |c| {
    Some(c.saturating_sub(1))
}).ok();

// load_from_root() — initialization (after inode_data is loaded):
let initial_inode_count = inode_data.len() as u64;
// ...
inode_count: AtomicU64::new(initial_inode_count),

// Public getter:
pub fn inode_count(&self) -> u64 {
    self.inode_count.load(Ordering::Relaxed)
}
```

### FIX-02: statfs() FUSE handler (verified against filesystem.rs lines 1170-1197)

```rust
fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
    let inode_count = self.meta.inode_count();

    // Get host disk stats via statvfs on backing store path
    let (host_blocks, host_bfree, host_bavail, bsize) =
        if let Some(ref sp) = self.store_path {
            if let Ok(path_cstr) = std::ffi::CString::new(sp.to_str().unwrap_or("/")) {
                let mut sv: libc::statvfs = unsafe { std::mem::zeroed() };
                if unsafe { libc::statvfs(path_cstr.as_ptr(), &mut sv) } == 0 {
                    (sv.f_blocks, sv.f_bfree, sv.f_bavail, sv.f_frsize as u32)
                } else {
                    (0u64, 0u64, 0u64, 4096u32)
                }
            } else {
                (0u64, 0u64, 0u64, 4096u32)
            }
        } else {
            (0u64, 0u64, 0u64, 4096u32)
        };

    let ffree = u64::MAX - inode_count;

    reply.statfs(host_blocks, host_bfree, host_bavail,
                 inode_count, ffree, bsize, 255, 0);
}
```

### FIX-02: scrub saturated refcount report (new method on DictMetadataStore)

```rust
// store.rs — new method
pub fn saturated_refcount_count(&self) -> usize {
    let rc = self.refcounts.lock().unwrap();
    rc.values().filter(|&&v| v == u64::MAX).count()
}
```

```rust
// scrub.rs — add to ScrubReport
pub struct ScrubReport {
    // ... existing fields ...
    pub saturated_blocks: usize,  // blocks with refcount == u64::MAX (immortal)
}

// After DictMetadataStore::load_from_root() succeeds:
let saturated = meta.saturated_refcount_count();
if saturated > 0 {
    // Not an error, just a warning
    eprintln!("Warning: {} block(s) have saturated refcounts (immortal — will not be GC'd)", saturated);
}
```

---

## Pre-Satisfied Requirements

### FIX-03 and FIX-04 — Verified Complete

**Evidence from store.rs (lines 118-119):**
```rust
snapshots_by_version: Mutex::new(HashMap::new()),
snapshots_by_name: Mutex::new(HashMap::new()),
```

Both dual HashMap indexes are present in `DictMetadataStore`. Phase 7.1 CONTEXT.md confirms: "Snapshots use dual HashMap indexes (FIX-03/FIX-04 DONE)". The Phase 8 planner should include a verification task that asserts these fields exist and that snapshot lookup code uses them — not as implementation work, but as a gate check confirming the pre-satisfied state.

---

## Validation Architecture

### Test Framework
| Property | Value |
|----------|-------|
| Framework | Rust built-in `#[test]` / cargo test |
| Config file | None — standard cargo test discovery |
| Quick run command | `cargo test -p metadata -p slicefs-cli` |
| Full suite command | `cargo test` |

### Phase Requirements → Test Map

| Req ID | Behavior | Test Type | Automated Command | File Exists? |
|--------|----------|-----------|-------------------|-------------|
| FIX-01 | `increment_refcount` at `u64::MAX - 1` produces `u64::MAX`, not 0 | unit | `cargo test -p metadata -- test_refcount_saturates` | ❌ Wave 0 |
| FIX-01 | `decrement_refcount` at `u64::MAX` is a no-op | unit | `cargo test -p metadata -- test_refcount_decrement_saturated` | ❌ Wave 0 |
| FIX-01 | `saturated_refcount_count()` returns correct count | unit | `cargo test -p metadata -- test_saturated_refcount_count` | ❌ Wave 0 |
| FIX-01 | Scrub report includes `saturated_blocks` field | unit | `cargo test -p slicefs-cli -- test_scrub_saturated_blocks` | ❌ Wave 0 |
| FIX-02 | `inode_count()` returns 0 on fresh store | unit | `cargo test -p metadata -- test_inode_count_empty` | ❌ Wave 0 |
| FIX-02 | `inode_count()` increments on `create_inode`, decrements on `delete_inode` | unit | `cargo test -p metadata -- test_inode_count_tracks_lifecycle` | ❌ Wave 0 |
| FIX-02 | `inode_count()` initialized correctly after `load_from_root` | unit | `cargo test -p metadata -- test_inode_count_after_reload` | ❌ Wave 0 |
| FIX-02 | `statfs()` `files` field reflects `inode_count()`, not 1,000,000 | integration | `cargo test --test statfs_tests -- test_statfs_files_reflects_inode_count` | ❌ Wave 0 |
| FIX-02 | `statfs()` `blocks`/`bfree` come from `statvfs` (not hardcoded `u64::MAX / 4`) | integration | `cargo test --test statfs_tests -- test_statfs_blocks_from_host` | ❌ Wave 0 |
| FIX-03 | `snapshots_by_version` HashMap field exists in DictMetadataStore | compile-time | `cargo build -p metadata` | ✅ Existing |
| FIX-04 | `snapshots_by_name` HashMap field exists in DictMetadataStore | compile-time | `cargo build -p metadata` | ✅ Existing |

### Sampling Rate
- **Per task commit:** `cargo test -p metadata -p slicefs-cli`
- **Per wave merge:** `cargo test`
- **Phase gate:** Full suite green before `/gsd:verify-work`

### Wave 0 Gaps

- [ ] `crates/metadata/src/store.rs` — add `test_refcount_saturates`, `test_refcount_decrement_saturated`, `test_saturated_refcount_count` unit tests (inline `#[cfg(test)]` module already exists in store.rs)
- [ ] `crates/metadata/src/store.rs` — add `test_inode_count_empty`, `test_inode_count_tracks_lifecycle`, `test_inode_count_after_reload` unit tests
- [ ] `crates/slicefs-cli/tests/statfs_tests.rs` — add `test_statfs_files_reflects_inode_count`, `test_statfs_blocks_from_host` (file exists, add test functions)
- [ ] `crates/slicefs-cli/src/scrub.rs` — add `test_scrub_saturated_blocks` unit test (inline module exists)

---

## State of the Art

| Old Approach | Current Approach | When Changed | Impact |
|--------------|------------------|--------------|--------|
| `*rc.entry(d).or_insert(0) += 1` | `saturating_add(1)` with MAX guard | Phase 8 | Eliminates silent data-loss on overflow |
| `files = 1_000_000` hardcoded | `inode_count: AtomicU64` | Phase 8 | `df` reports real inode count |
| `bfree = u64::MAX / 4` | `libc::statvfs()` on store path | Phase 8 | `df` shows real host disk free space |
| `dir_size(vt0/)` on every statfs | `dir_size(vt0/)` on every statfs | unchanged (discretion) | Acceptable — statfs call rate is low; no cache needed |
| Vec<SnapshotEntry> O(n) scan | dual HashMap O(1) lookup | Phase 7.1 | Already done — FIX-03/FIX-04 complete |

---

## Open Questions

1. **statvfs fallback for in-memory test mode**
   - What we know: `self.store_path` is `Option<PathBuf>`, set to `None` when `SliceFsFilesystem` is created without a backing store (used in unit tests)
   - What's unclear: `fresh_fs()` in statfs_tests.rs line 20 passes `None` as the store_path argument. The new `statfs()` implementation must handle `store_path == None` gracefully
   - Recommendation: When `store_path` is `None`, fall back to `blocks=0, bfree=0, bavail=0, bsize=4096`. Tests that verify host disk values should use a real `TempDir`-backed filesystem

2. **CAS bytes AtomicU64 vs dir_size (Claude's discretion)**
   - What we know: CONTEXT.md lists "CAS bytes tracked via AtomicU64 counter" as a locked decision, but also lists "How CAS bytes AtomicU64 is kept in sync" as Claude's discretion
   - What's unclear: The locked decision says track at "block store put/delete" — but `put` currently happens through `blockset::State::push_all` which has no hook into `DictMetadataStore`
   - Recommendation: Defer the CAS bytes AtomicU64 to Phase 9 (which already touches the write path). For Phase 8, use `dir_size(vt0/)` in `statfs()` (already in place) for the physical tier. `slicefs stats` already computes `physical_bytes` via `dir_size`. This satisfies the user's three-tier requirement without requiring a new hook into blockset.

---

## Sources

### Primary (HIGH confidence)
- Live codebase — `crates/metadata/src/store.rs` — verified `increment_refcount` (line 142), `logical_bytes: AtomicU64` pattern (lines 70, 115, 352, 392, 419, 855-870)
- Live codebase — `crates/slicefs-cli/src/filesystem.rs` — verified `statfs()` handler (lines 1170-1197), hardcoded `files = 1_000_000` (line 1192)
- Live codebase — `crates/slicefs-cli/tests/statfs_tests.rs` — verified existing test infrastructure (9 passing tests)
- `~/.cargo/registry/src/.../libc-0.2.180/src/unix/bsd/apple/mod.rs` — verified `libc::statvfs` struct fields for macOS
- `~/.cargo/registry/src/.../libc-0.2.180/src/unix/mod.rs` — verified `pub fn statvfs(path: *const c_char, buf: *mut crate::statvfs) -> c_int`
- `crates/slicefs-cli/Cargo.toml` — verified `libc = { workspace = true }` and `tracing = { workspace = true }` already present

### Secondary (MEDIUM confidence)
- `.planning/phases/08-correctness-fixes/08-CONTEXT.md` — user decisions document
- `.planning/STATE.md` — Phase 7.1 completion confirmation (dual HashMap indexes in place)
- `.planning/research/PITFALLS.md` — prior research on statfs and refcount pitfalls

---

## Metadata

**Confidence breakdown:**
- FIX-01 (refcount saturation): HIGH — one-line fix with clear before/after code, verified against live store.rs
- FIX-02 (statfs inode count): HIGH — `logical_bytes` pattern is the exact template; `libc::statvfs` verified in registry
- FIX-03/FIX-04 (pre-satisfied): HIGH — dual HashMap fields verified in store.rs lines 118-119
- Validation architecture: HIGH — test files located, test commands verified running

**Research date:** 2026-03-30
**Valid until:** 2026-04-30 (stable domain — no fast-moving dependencies)
