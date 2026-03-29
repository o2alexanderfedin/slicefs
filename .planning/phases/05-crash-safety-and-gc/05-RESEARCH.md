# Phase 05: Crash Safety and GC - Research

**Researched:** 2026-03-27
**Domain:** WAL design, log-structured storage, mark-and-sweep GC, crash recovery, FUSE fsync
**Confidence:** HIGH

---

<user_constraints>
## User Constraints (from CONTEXT.md)

### Locked Decisions

**WAL design and crash recovery**
- Pluggable WAL strategy via a trait, selected at mount time via CLI flag (`--wal-strategy`). Strategies:
  1. Per-operation (safest): every mutation logs a WAL entry before modifying Dictionary. Zero data loss window. Default for production
  2. Periodic checkpoint: full Dictionary snapshot every N seconds + delta WAL between checkpoints. Bounded data loss window
  3. Flush-on-fsync: WAL entries written only when fsync() is called. Application controls durability
  4. No WAL (unsafe, fast): Dictionary only persisted on clean unmount. For scratch/temp mounts explicitly optimized for speed
- Dirty mount detection: lock file in store directory. Created on mount, removed on clean unmount. If lock file exists at next mount = dirty -> WAL replay
- WAL replay on dirty mount restores last committed state without manual intervention

**GC storage model - log-structured segments**
- Log-structured append-only segments for Dictionary persistence (replaces single dictionary.bin)
- New Dictionary entries appended to the current segment file. Segments are immutable once closed
- Compaction = read segment, skip entries with refcount=0 (unreachable), write survivors to new segment, delete old segment
- All writes sequential - appends and compaction. No random in-place mutations. SSD-optimal
- Matches CAS immutability: Dictionary entries are never modified, only inserted and eventually garbage-collected
- The dictionary.bin format from Phases 2-4 is replaced by the segment-based format in this phase

**GC trigger and reclamation**
- Both background and on-demand GC:
  - Background GC thread during mount: runs periodically or when orphan count exceeds threshold. Non-blocking to FUSE operations
  - Offline CLI command: `slicefs gc <store>` runs GC with filesystem unmounted. For deep compaction

**Snapshot-aware GC safety**
- Multi-root mark-and-sweep for liveness determination
- GC walks ALL live roots (current root + all pinned snapshot roots). Any entry reachable from ANY root is live. Unreachable entries are garbage
- Safe with immutable Merkle tree: tree doesn't change during GC scan. New entries created during GC go into current segment (not being compacted)
- No refcount mutation during liveness check - mark-and-sweep avoids mutating state during the scan
- Shared entries between snapshots are naturally handled (reached from multiple roots, marked once)

**fsync/fdatasync durability**
- fsync flushes WAL + buffer to segment - flush file's write buffer through CAS pipeline, append new entries to current segment, fsync the segment file. Behavior depends on active WAL strategy
- fdatasync treated same as fsync for now - optimization to skip metadata-only writes deferred to Phase 7 if needed

### Claude's Discretion
- WAL entry format (binary, self-describing records)
- Segment file naming and rotation policy (size-based or time-based)
- Compaction heuristics (which segments to compact, when to trigger)
- Background GC thread scheduling (interval, orphan threshold)
- Lock file format and cleanup strategy
- How to handle WAL corruption (skip corrupted entries vs refuse to mount)

### Deferred Ideas (OUT OF SCOPE)
- fdatasync optimization - skip metadata writes when only data changed. Phase 7 if needed
- TRIM/discard hints - SSD TRIM support for deleted segments. Phase 7
- Concurrent compaction - compact while filesystem is mounted with minimal locking. Optimize in Phase 7 if background GC is insufficient
- WAL compression - compress WAL entries to reduce write amplification. Future optimization
</user_constraints>

---

<phase_requirements>
## Phase Requirements

| ID | Description | Research Support |
|----|-------------|-----------------|
| META-01 | Atomic metadata commits (crash-safe root pointer update) | WAL per-operation strategy + lock file dirty detection + replay; log-structured segments give atomic pointer rotation via segment rename |
| GC-01 | Crash-safe garbage collection of zero-refcount blocks | Two-phase approach: mark during GC scan without mutating refcounts; physically delete only segments whose entries are all confirmed dead |
| GC-02 | Two-phase mark-and-sweep or WAL-based refcount with deferred physical deletion | Mark phase walks all live roots and collects reachable Digest224s; sweep phase compacts segments, omitting entries absent from live set |
| GC-03 | Snapshot-aware GC (blocks reachable from any snapshot are live) | Multi-root mark collects live set from current root + all pinned snapshot roots; entry reachable from any root is kept |
| POSIX-11 | fsync/fdatasync correctness (guaranteed durability) | fuser 0.17 `fsync(datasync: bool)` callback; call flush_buffer_to_cas + append segment entries + File::sync_all on segment file |
</phase_requirements>

---

## Summary

Phase 5 replaces the single `dictionary.bin` file with a log-structured segment store and adds a pluggable WAL strategy, dirty-mount detection, background GC, and correct `fsync`/`fdatasync` handling. All four concerns are deeply interrelated: the segment format IS the WAL journal (new entries are appended, never in-place mutated); the GC compaction IS the mechanism that reclaims dead segments; and `fsync` is the point at which in-flight write buffers are flushed through the CAS pipeline and segments are durably committed to disk.

The codebase already has the primitives needed: `Dictionary` (in-memory KV store), `serialize_dictionary`/`deserialize_dictionary` (to be replaced), `commit()`/`load_from_root()` for root record management, refcount infrastructure (`increment_refcount`, `decrement_refcount`, `get_refcount`), and `destroy()` for clean-unmount persistence. The `DictMetadataStore` `Mutex<Dictionary>` pattern continues to serve as the thread-safety mechanism.

The primary implementation risk is the WAL strategy trait boundary: it must sit between the in-memory `Dictionary` mutation and the segment file append, so every `DictMetadataStore` mutation must be routed through the WAL layer. The secondary risk is GC liveness correctness — a mark phase that misses any live root will silently delete live data. The design decisions (multi-root mark-and-sweep, immutable Merkle tree, new writes go to current segment not the segment being compacted) all but eliminate this risk.

**Primary recommendation:** Implement in four vertical slices — (1) segment file I/O layer, (2) WAL strategy trait + per-op implementation, (3) dirty-mount detection + replay, (4) mark-and-sweep GC + background thread + `slicefs gc` CLI command. `fsync` is a cross-cutting concern that lands in slice 2 (it triggers WAL flush + segment fsync).

---

## Standard Stack

### Core
| Library | Version | Purpose | Why Standard |
|---------|---------|---------|--------------|
| `std::fs::File` | stdlib | Segment file I/O, lock file, fsync | `File::sync_all()` = `fsync(2)`, `File::sync_data()` = `fdatasync(2)` — zero dependencies |
| `std::sync::{Arc, Mutex}` | stdlib | Background GC thread shares `DictMetadataStore` | Existing pattern in codebase; GC thread holds `Arc<DictMetadataStore>` |
| `std::thread::spawn` + `Arc::clone` | stdlib | Background GC thread | Standard thread-based concurrency; tokio not needed for a single periodic background task |
| `fuser 0.17` | workspace | `fsync(datasync: bool)` callback | Already in workspace; `datasync` parameter distinguishes fsync vs fdatasync |
| `clap 4` | workspace | `slicefs gc <store>` subcommand | Already used for mount/unmount/seed subcommands |
| `crc32fast` or `std::hash` | TBD | WAL entry frame CRC | CRC-32 is the standard framing checksum for WAL entries (see okaywal); determine if adding crate is warranted |

### Supporting
| Library | Version | Purpose | When to Use |
|---------|---------|---------|-------------|
| `tempfile 3` | workspace | Atomic segment rename via `.tmp` | Segment write: write to `.tmp`, rename to final name — prevents partial segment reads |
| `tracing 0.1` | workspace | WAL replay progress, GC statistics logging | Already in workspace |

### Alternatives Considered
| Instead of | Could Use | Tradeoff |
|------------|-----------|----------|
| Hand-rolled segment format | `okaywal` crate | okaywal is mature but adds dependency and forces its recovery API; hand-rolled is ~150 LOC and fully under our control — prefer hand-rolled given the project's goal |
| `std::thread` for GC | `tokio` spawned task | tokio is in workspace but not used in the filesystem hot path; adding async complexity to GC is not warranted in Phase 5 |
| Custom CRC | `crc32fast` crate | `crc32fast` is fast and widely used; acceptable if the workspace gains a new dependency; alternatively omit CRC and rely on Blake3 content addressing for corruption detection |

**Installation (if crc32fast needed):**
```bash
# In Cargo.toml [workspace.dependencies] — only if CRC framing is chosen
# crc32fast = "1"
```

---

## Architecture Patterns

### Recommended Project Structure

New `crates/slicefs-wal/` crate (or `wal` module inside `metadata` crate — simpler and avoids a new crate for Phase 5):

```
crates/metadata/src/
├── store.rs            # DictMetadataStore — unchanged API, gains WalStrategy field
├── wal/
│   ├── mod.rs          # WalStrategy trait definition + WalConfig enum
│   ├── per_op.rs       # PerOpWal — writes entry before each Dictionary mutation
│   ├── periodic.rs     # PeriodicWal — checkpoints every N seconds
│   ├── flush_on_fsync.rs  # FlushOnFsyncWal — buffers, flushes on fsync signal
│   └── no_wal.rs       # NoWal — no-op, clean unmount only
├── segment/
│   ├── mod.rs          # SegmentStore — replaces dictionary.bin
│   ├── writer.rs       # SegmentWriter — append-only entry writer
│   ├── reader.rs       # SegmentReader — replay/scan
│   └── compaction.rs   # compact_segment(old: &Path, live_set: &HashSet<Digest224>)
└── gc/
    ├── mod.rs          # GarbageCollector — mark-and-sweep engine
    └── background.rs   # BackgroundGcThread — spawn/shutdown
```

CLI addition in `crates/slicefs-cli/src/`:
```
crates/slicefs-cli/src/
├── cli.rs              # Add Gc subcommand
└── gc.rs               # run_gc(store_path) — offline GC
```

### Pattern 1: WAL Strategy Trait

**What:** A trait with a single `log_mutation` method; each implementation decides whether to write to disk before returning, buffer for later, or no-op.

**When to use:** Every `DictMetadataStore` method that mutates in-memory state calls through the WAL before (or after, depending on strategy) the mutation.

**Example:**
```rust
// Source: project design, informed by okaywal Checkpointer trait pattern
pub trait WalStrategy: Send + Sync {
    /// Called before (or instead of) mutating Dictionary state.
    /// Returns Ok(()) when the entry is durably logged (or skipped for NoWal).
    fn log_mutation(&self, entry: &WalEntry) -> Result<(), WalError>;

    /// Called on fsync — flush any buffered entries and sync the segment file.
    fn flush_and_sync(&self) -> Result<(), WalError>;

    /// Called on clean unmount — allows final flush.
    fn shutdown(&self) -> Result<(), WalError>;
}

pub enum WalEntry {
    DictionaryAppend { digest: Digest224, data: Vec<u8> },
    RootUpdate { root: Digest224 },
    RefcountIncrement { digest: Digest224 },
    RefcountDecrement { digest: Digest224 },
}
```

### Pattern 2: Segment File Format

**What:** Append-only file of fixed-size records. Each record is self-describing.

**When to use:** Every new Dictionary entry (Digest224 key + Branches value = 92 bytes) is appended. Segment is closed (renamed to immutable name) when size threshold reached.

**Example:**
```rust
// Source: project design, standard WAL framing (see okaywal segment format)
// Segment file header (16 bytes):
//   magic:    [0x53, 0x4C, 0x53, 0x47]  // "SLSG" (SliceFS Segment)
//   version:  [0x01, 0x00, 0x00, 0x00]  // u32 LE = 1
//   segment_id: u64 LE                  // monotonically increasing
//
// Per-entry record (variable, min 97 bytes):
//   record_type: u8   // 0x01 = DictEntry, 0x02 = RootUpdate, 0xFF = EOF marker
//   payload_len: u32 LE
//   payload:     [payload_len bytes]
//
// DictEntry payload (92 bytes):
//   key: Digest224 = [u32; 7] = 28 bytes
//   branches: Branches = 64 bytes (existing format from serialize_dictionary)
//
// RootUpdate payload (28 bytes):
//   root: Digest224 = [u32; 7]
//
// On crash: reader stops at first byte that is not a valid record_type.
// Partial records at end of file are silently truncated (standard WAL recovery).
```

### Pattern 3: Dirty Mount Detection

**What:** Create `<store>/mount.lock` on mount, remove on clean unmount. Presence at next mount = dirty.

**When to use:** Beginning of `run_mount`. If lock file exists, replay WAL before starting FUSE.

**Example:**
```rust
// Source: project design, standard dirty-mount pattern (ext4, NTFS journal detection)
pub fn acquire_mount_lock(store_path: &Path) -> Result<MountLock, LockError> {
    let lock_path = store_path.join("mount.lock");
    if lock_path.exists() {
        // Dirty mount — WAL replay required before returning
        return Err(LockError::DirtyMount { lock_path });
    }
    std::fs::write(&lock_path, b"locked")?;
    Ok(MountLock { lock_path })
}

impl Drop for MountLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.lock_path);
    }
}
```

### Pattern 4: Mark-and-Sweep GC

**What:** Two-phase scan: mark all Digest224s reachable from any live root into a HashSet, then compact segments by retaining only marked entries.

**When to use:** Run by background thread (periodic) or `slicefs gc` CLI (offline).

**Example:**
```rust
// Source: project design, multi-root mark pattern
pub fn collect_live_set(
    dict: &Dictionary,
    roots: &[Digest224],  // current root + all snapshot roots
) -> HashSet<Digest224> {
    let mut live = HashSet::new();
    for root in roots {
        mark_reachable(dict, root, &mut live);
    }
    live
}

fn mark_reachable(dict: &Dictionary, digest: &Digest224, live: &mut HashSet<Digest224>) {
    if live.contains(digest) { return; }  // already visited
    live.insert(*digest);
    // Walk children via blockset Dictionary entries (Branches)
    if let Some(branches) = dict.get(digest) {
        for child in &branches {
            mark_reachable(dict, child, live);
        }
    }
}

pub fn compact_segment(
    segment_path: &Path,
    live_set: &HashSet<Digest224>,
    output_path: &Path,
) -> Result<usize, SegmentError> {
    // Returns number of entries written (survivors)
    // Reads input, writes only entries whose key is in live_set
    // Caller renames output_path to replace segment_path atomically
}
```

### Pattern 5: Background GC Thread

**What:** Spawn a thread at mount time that holds a `Weak<DictMetadataStore>` — upgrades to `Arc` to acquire access, drops if upgrade fails (filesystem unmounted).

**When to use:** Spawned in `run_mount`, checked/joined in `destroy()`.

**Example:**
```rust
// Source: standard Rust Arc/Weak thread lifecycle pattern
pub fn spawn_background_gc(
    meta: Arc<DictMetadataStore>,
    store_path: PathBuf,
    interval_secs: u64,
    orphan_threshold: usize,
) -> GcHandle {
    let weak = Arc::downgrade(&meta);
    let handle = std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(interval_secs));
            match weak.upgrade() {
                None => break,  // filesystem unmounted
                Some(meta) => run_gc_cycle(&meta, &store_path, orphan_threshold),
            }
        }
    });
    GcHandle(handle)
}
```

### Pattern 6: fsync Implementation

**What:** fuser 0.17 `fsync(datasync: bool)` callback. When `datasync=true` only file data needs sync; when `false` metadata too. Both treated identically in Phase 5 per the locked decision.

**Example:**
```rust
// Source: fuser 0.17 docs — https://docs.rs/fuser/latest/fuser/trait.Filesystem.html
fn fsync(
    &self,
    _req: &Request,
    ino: INodeNo,
    fh: FileHandle,
    _datasync: bool,   // treated same as fsync for now per Phase 5 decisions
    reply: ReplyEmpty,
) {
    // 1. If fh has a write buffer, flush it through CAS pipeline
    //    (same as release but without removing the open_files entry)
    // 2. WAL strategy: flush_and_sync() — appends buffered entries to segment, calls File::sync_all
    // 3. reply.ok()
    match self.flush_and_sync_fh(ino.0, fh.0) {
        Ok(()) => reply.ok(),
        Err(_) => reply.error(Errno::EIO),
    }
}
```

### Anti-Patterns to Avoid

- **Mutating refcounts during GC scan:** Causes races between concurrent writes and the GC mark phase. Use a point-in-time snapshot of the live Dictionary for the mark phase.
- **Deleting segment files before confirming the new compacted segment is durable:** Must `File::sync_all()` the new segment before deleting the old one. Otherwise a crash between writes leaves no segment.
- **Writing WAL entries after mutating in-memory state:** WAL MUST be written BEFORE the mutation for per-op strategy. If the process crashes after the mutation but before the WAL write, replay cannot recover the change.
- **Using a single global lock for GC + FUSE callbacks:** Will serialize all FUSE operations during GC scan. GC mark phase must operate on a snapshot or use fine-grained locking.
- **Growing `dictionary.bin` without limit:** The segment model solves this — compaction reclaims space, but only if old segments are actually deleted after successful compaction.

---

## Don't Hand-Roll

| Problem | Don't Build | Use Instead | Why |
|---------|-------------|-------------|-----|
| Atomic file replace | temp-write + rename sequence | `tempfile::NamedTempFile` or manual `.tmp` + `std::fs::rename` | rename(2) is atomic on POSIX; partial writes to `.tmp` never become visible |
| File sync to disk | custom flush loop | `File::sync_all()` for fsync, `File::sync_data()` for fdatasync | These are the stdlib wrappers for `fsync(2)` and `fdatasync(2)` |
| CRC for WAL framing | custom checksum | `crc32fast` crate OR rely on Blake3 content addressing | Blake3 hashes already provide integrity verification; explicit CRC per WAL frame is optional |
| Thread-safe reference | custom Rc equivalent | `Arc<Mutex<T>>` + `Weak<T>` | Established pattern throughout existing codebase |
| Background thread lifecycle | complex state machine | `Weak<T>::upgrade()` returning `None` as shutdown signal | Idiomatic Rust: `Weak` upgrade fails when the `Arc` is dropped; zero additional code needed |

**Key insight:** The segment format is ~ 150 lines of straightforward file I/O. The WAL strategy trait is ~80 lines. Neither justifies a third-party dependency over custom code, because the format is private and must evolve with the project.

---

## Common Pitfalls

### Pitfall 1: Dictionary Lock Held Across fsync Syscall
**What goes wrong:** Locking `Mutex<Dictionary>` to serialize entries, then calling `File::sync_all()` while still holding the lock. All FUSE callbacks that need the Dictionary (every read) block for the full fsync duration (can be 10-100ms on a busy SSD).
**Why it happens:** Naively combining "get entries to write" and "write + sync" in one critical section.
**How to avoid:** Copy entries to a local `Vec`, drop the Dictionary lock, then append + sync the segment file without holding any shared lock.
**Warning signs:** FUSE read operations stall during fsync in integration tests.

### Pitfall 2: WAL Replay Applying Already-Applied Entries
**What goes wrong:** On dirty mount, WAL replay re-applies all entries from the WAL including ones that were already persisted (committed to a closed segment). This duplicates Dictionary entries or inflates refcounts.
**Why it happens:** Not tracking which WAL entries have been checkpointed (consumed into a segment).
**How to avoid:** Each closed segment file gets a "committed up to WAL entry N" marker. Replay only applies WAL entries with ID > last committed. OR: replay is idempotent by design (inserting the same Dictionary entry is a no-op since Dictionary keys are content hashes).
**Warning signs:** After dirty-mount replay, refcounts are double what they should be.

### Pitfall 3: GC Deletes Entry Written During Compaction
**What goes wrong:** GC starts mark phase, collects live set. Meanwhile, a new write creates a new Dictionary entry. GC finishes mark (entry not in live set), compacts segment (entry omitted). Entry is now lost.
**Why it happens:** Live set was collected before the new entry existed.
**How to avoid:** New entries written during GC go to the CURRENT (open, not-yet-being-compacted) segment. GC only compacts CLOSED segments. Closed segments are immutable by definition. This is the locked design choice — it eliminates the race entirely without needing locks.
**Warning signs:** Files written during background GC return corrupted data or ENOENT on re-read.

### Pitfall 4: Lock File Not Removed on Panic
**What goes wrong:** Process panics (bug, OOM, signal) after creating `mount.lock` but before `MountLock::drop()`. Next mount detects false dirty state, replays WAL unnecessarily.
**Why it happens:** File cleanup is not automatic in the face of panic.
**How to avoid:** WAL replay is safe even on a clean mount (replaying nothing = no-op). The lock file being present triggers replay — which is conservative but correct. Document that spurious lock file = spurious replay, not corruption.
**Warning signs:** Every mount after a test-suite crash triggers unnecessary WAL replay. Acceptable behavior.

### Pitfall 5: Segment Reader Stops at First Unknown Record Type
**What goes wrong:** A future version writes a new record type to a segment. An old reader hits it, declares EOF, and silently ignores all subsequent entries including live data.
**Why it happens:** Eager truncation on unknown record type is the standard WAL crash-recovery behavior — but it also triggers on valid unknown types.
**How to avoid:** Include `payload_len` in every record header. Unknown record types skip `payload_len` bytes (forward-compatible parsing) rather than stopping. Only stop at truncated length or explicit EOF marker.
**Warning signs:** Entries added by future record types disappear after reload.

---

## Code Examples

Verified patterns from official sources:

### File::sync_all vs File::sync_data
```rust
// Source: https://doc.rust-lang.org/std/fs/struct.File.html
use std::fs::File;
use std::io::Write;

let mut file = File::create("segment-001.seg")?;
file.write_all(&entry_bytes)?;
// fsync: sync all OS-internal metadata + data
file.sync_all()?;
// fdatasync: sync data only (may skip metadata update) — maps to fdatasync(2)
// file.sync_data()?;
```

### Atomic Segment Rotation via Rename
```rust
// Source: standard POSIX atomicity — rename(2) is atomic
let tmp_path = store_path.join("segment-002.tmp");
let final_path = store_path.join("segment-002.seg");
let mut tmp = File::create(&tmp_path)?;
// ... write all surviving entries ...
tmp.sync_all()?;    // ensure new segment is durable BEFORE making it visible
drop(tmp);
std::fs::rename(&tmp_path, &final_path)?;   // atomic on POSIX
std::fs::remove_file(&old_segment_path)?;   // safe to delete old segment now
```

### fuser 0.17 fsync Signature
```rust
// Source: https://docs.rs/fuser/latest/fuser/trait.Filesystem.html
// datasync: bool — true = fdatasync semantics (data only), false = full fsync
fn fsync(
    &self,
    _req: &Request,
    ino: INodeNo,
    fh: FileHandle,
    datasync: bool,
    reply: ReplyEmpty,
) {
    // Phase 5: treat both datasync=true and datasync=false identically
    match self.do_fsync(ino.0, fh.0) {
        Ok(()) => reply.ok(),
        Err(_) => reply.error(Errno::EIO),
    }
}
```

### Background GC Thread Lifecycle
```rust
// Source: standard Rust Arc/Weak pattern for background threads
use std::sync::{Arc, Weak};
use std::time::Duration;

pub struct GcHandle(std::thread::JoinHandle<()>);

pub fn spawn_gc_thread(
    meta_weak: Weak<DictMetadataStore>,
    store_path: PathBuf,
    interval: Duration,
) -> GcHandle {
    let handle = std::thread::spawn(move || {
        loop {
            std::thread::sleep(interval);
            let meta = match meta_weak.upgrade() {
                Some(m) => m,
                None => break,  // Arc dropped (filesystem unmounted) — exit gracefully
            };
            // Run GC cycle — does not hold meta longer than needed
            run_gc_cycle(&meta, &store_path);
        }
    });
    GcHandle(handle)
}
```

### WAL Strategy Trait Skeleton
```rust
// Source: project design — adapted from okaywal Checkpointer trait pattern
// https://github.com/khonsulabs/okaywal

pub trait WalStrategy: Send + Sync + 'static {
    fn log_mutation(&self, entry: &WalEntry) -> Result<(), WalError>;
    fn flush_and_sync(&self) -> Result<(), WalError>;
    fn shutdown(&self) -> Result<(), WalError>;
}

/// No-op implementation for the no-WAL strategy.
pub struct NoWal;
impl WalStrategy for NoWal {
    fn log_mutation(&self, _: &WalEntry) -> Result<(), WalError> { Ok(()) }
    fn flush_and_sync(&self) -> Result<(), WalError> { Ok(()) }
    fn shutdown(&self) -> Result<(), WalError> { Ok(()) }
}
```

---

## State of the Art

| Old Approach | Current Approach | When Changed | Impact |
|--------------|------------------|--------------|--------|
| Single `dictionary.bin` rewritten on clean unmount | Append-only segment files, closed + rotated | Phase 5 | Crash-safe: partial appends are truncated on replay, not a full-file corruption |
| No WAL, data lost on crash | Pluggable WAL strategy, default per-op | Phase 5 | META-01 satisfied: atomic root pointer update via WAL RootUpdate entry |
| No GC, blocks accumulate forever | Mark-and-sweep GC with segment compaction | Phase 5 | GC-01/GC-02/GC-03 satisfied |
| No fsync support (returns ENOSYS by default in fuser) | fsync callback implemented, flushes WAL + segment | Phase 5 | POSIX-11 satisfied |

**Deprecated/outdated:**
- `dictionary.bin` / `serialize_dictionary` / `deserialize_dictionary`: replaced by segment store. The segment reader/writer must include a migration path to import a legacy `dictionary.bin` as the first segment, or the mount command detects `dictionary.bin` presence and performs a one-time conversion.
- `root.bin`: root update is now a `RootUpdate` WAL entry appended to the current segment. A synthetic `root.bin` can still be written on clean unmount for backward compat with the mount command's `load_store()`.

---

## Open Questions

1. **Dictionary.bin migration path**
   - What we know: Phase 4 stores produce `dictionary.bin` + `root.bin`. Phase 5 stores use segments.
   - What's unclear: Should Phase 5 mount auto-convert legacy stores on first mount, or require explicit `slicefs migrate <store>` command?
   - Recommendation: Auto-convert on first mount — treat `dictionary.bin` as a single legacy segment with all entries live. This is invisible to the user and avoids an extra CLI subcommand.

2. **Per-op WAL entry granularity**
   - What we know: Each `DictMetadataStore` mutation (intern_inode, add_dir_entry, set_manifest, increment_refcount) modifies the in-memory Dictionary.
   - What's unclear: Should WAL entries be individual Dictionary key-value pairs, or entire operation batches (e.g., "create_inode = {inode_digest, dir_entry_update}")?
   - Recommendation: Log individual Dictionary key-value pairs. This maps 1:1 to segment entries and makes replay trivial: each entry just `dict.insert(key, value)`. Batch-level journaling would require parsing operation semantics on replay.

3. **Segment rotation policy: size-based vs entry-count-based**
   - What we know: Claude's discretion.
   - What's unclear: At what point does a segment become too large to scan efficiently?
   - Recommendation: Size-based rotation at 4 MB. This bounds replay time to scanning at most one 4 MB segment on dirty mount. 4 MB / 92 bytes per entry = ~44,000 entries per segment — appropriate for a metadata store.

4. **Background GC thread: when to trigger compaction vs when to skip**
   - What we know: Claude's discretion.
   - What's unclear: What orphan threshold is appropriate before triggering compaction?
   - Recommendation: Trigger compaction when any closed segment has >50% dead entries (refcount=0). Check every 60 seconds. This bounds wasted disk space while avoiding continuous GC churn.

5. **WAL corruption handling**
   - What we know: Claude's discretion.
   - What's unclear: Skip corrupted entries (may silently lose operations) vs refuse to mount (may be over-strict for minor truncations)?
   - Recommendation: Stop-at-first-truncation (standard WAL behavior). A truncated record at the end of the WAL (the most common crash scenario) is skipped and the WAL is truncated at that point. A record with a valid length but bad CRC (rare, indicates storage corruption) causes mount to refuse. This matches PostgreSQL and SQLite WAL behavior.

---

## Validation Architecture

### Test Framework
| Property | Value |
|----------|-------|
| Framework | Rust built-in `#[test]` via `cargo test` |
| Config file | none — uses `Cargo.toml` `[dev-dependencies]` |
| Quick run command | `cargo test --workspace -q` |
| Full suite command | `cargo test --workspace` |

### Phase Requirements -> Test Map

| Req ID | Behavior | Test Type | Automated Command | File Exists? |
|--------|----------|-----------|-------------------|-------------|
| META-01 | Dirty-mount WAL replay restores last committed state | integration | `cargo test --package metadata -q wal` | Wave 0 |
| META-01 | Per-op WAL entry written before Dictionary mutation | unit | `cargo test --package metadata -q per_op_wal` | Wave 0 |
| META-01 | Lock file created on mount, removed on clean unmount | unit | `cargo test --package slicefs-cli -q mount_lock` | Wave 0 |
| META-01 | Segment RootUpdate entry is the durable commit marker | unit | `cargo test --package metadata -q segment_root_update` | Wave 0 |
| GC-01 | Zero-refcount entries omitted from compacted segment | unit | `cargo test --package metadata -q compact_segment` | Wave 0 |
| GC-01 | Crash during compaction (old segment still present) leaves data intact | integration | `cargo test --package metadata -q gc_crash_safety` | Wave 0 |
| GC-02 | Mark phase collects all reachable Digest224s from a root | unit | `cargo test --package metadata -q mark_reachable` | Wave 0 |
| GC-02 | Sweep phase produces segment with only live entries | unit | `cargo test --package metadata -q gc_sweep` | Wave 0 |
| GC-03 | Entry reachable from snapshot root survives GC even with refcount=0 in live tree | unit | `cargo test --package metadata -q gc_snapshot_root` | Wave 0 |
| POSIX-11 | fsync callback flushes write buffer + syncs segment file | unit | `cargo test --package slicefs-cli -q fsync` | Wave 0 |
| POSIX-11 | fdatasync (datasync=true) behaves identically to fsync in Phase 5 | unit | `cargo test --package slicefs-cli -q fdatasync` | Wave 0 |

### Sampling Rate
- **Per task commit:** `cargo test --workspace -q`
- **Per wave merge:** `cargo test --workspace`
- **Phase gate:** Full suite green before `/gsd:verify-work`

### Wave 0 Gaps
- [ ] `crates/metadata/src/wal/mod.rs` — WalStrategy trait + WalEntry enum
- [ ] `crates/metadata/src/wal/per_op.rs` — PerOpWal implementation
- [ ] `crates/metadata/src/wal/no_wal.rs` — NoWal no-op
- [ ] `crates/metadata/src/segment/mod.rs` — SegmentStore trait
- [ ] `crates/metadata/src/segment/writer.rs` — SegmentWriter
- [ ] `crates/metadata/src/segment/reader.rs` — SegmentReader + replay
- [ ] `crates/metadata/src/segment/compaction.rs` — compact_segment
- [ ] `crates/metadata/src/gc/mod.rs` — GarbageCollector mark-and-sweep
- [ ] `crates/metadata/src/gc/background.rs` — BackgroundGcThread
- [ ] `crates/metadata/tests/wal_tests.rs` — WAL strategy tests
- [ ] `crates/metadata/tests/segment_tests.rs` — segment read/write/replay tests
- [ ] `crates/metadata/tests/gc_tests.rs` — GC liveness correctness tests
- [ ] `crates/slicefs-cli/src/gc.rs` — run_gc() offline GC command
- [ ] Framework install: none — cargo test already works

---

## Sources

### Primary (HIGH confidence)
- fuser 0.17 docs (https://docs.rs/fuser/latest/fuser/trait.Filesystem.html) — `fsync(datasync: bool)` signature verified
- Rust stdlib docs (https://doc.rust-lang.org/std/fs/struct.File.html) — `sync_all()` = fsync(2), `sync_data()` = fdatasync(2) confirmed
- Existing codebase inspection — `DictMetadataStore`, `serialize_dictionary`, `destroy()`, refcount infrastructure, `Mutex<Dictionary>` pattern, 307 passing tests baseline

### Secondary (MEDIUM confidence)
- OkayWAL architecture (https://github.com/khonsulabs/okaywal) — segment file format with magic bytes + version header + CRC-32 per chunk; LogManager trait pattern for checkpointing/recovery
- OkayWAL blog post (https://bonsaidb.io/blog/introducing-okaywal/) — design rationale for entry/chunk/segment layering
- Rust Arc/Weak background thread pattern — multiple sources agree; standard idiom for graceful thread shutdown on Arc drop

### Tertiary (LOW confidence — use for reference only)
- WebSearch general WAL patterns (https://adambcomer.com/blog/simple-database/wal/) — conceptual overview, not authoritative
- WebSearch log-structured compaction (https://arindas.github.io/blog/segmented-log-rust/) — blog, not verified against production systems

---

## Metadata

**Confidence breakdown:**
- Standard stack: HIGH — stdlib + existing workspace dependencies; no new speculative dependencies
- Architecture: HIGH — all patterns derived from existing codebase conventions + verified fuser API
- WAL entry format: MEDIUM — informed by okaywal and standard WAL framing; exact field widths are Claude's discretion
- Pitfalls: HIGH — derived from well-understood crash-recovery and GC correctness invariants
- GC correctness: HIGH — immutable Merkle tree + closed-segments-only compaction eliminates the write-during-GC race

**Research date:** 2026-03-27
**Valid until:** 2026-06-27 (90 days — stable domain; fuser 0.17 API is current, stdlib is stable)
