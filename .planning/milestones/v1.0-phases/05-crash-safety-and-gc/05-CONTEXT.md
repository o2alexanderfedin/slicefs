# Phase 5: Crash Safety and GC - Context

**Gathered:** 2026-03-28
**Status:** Ready for planning

<domain>
## Phase Boundary

The filesystem survives crashes and power loss without data loss or block leaks. Garbage collection reclaims orphaned blocks safely without racing against active writes or snapshots. WAL provides configurable durability strategies. fsync/fdatasync guarantee data is on disk.

Requirements: META-01, GC-01, GC-02, GC-03, POSIX-11

</domain>

<decisions>
## Implementation Decisions

### WAL design and crash recovery
- **Pluggable WAL strategy** via a trait, selected at mount time via CLI flag (`--wal-strategy`). Strategies:
  1. **Per-operation** (safest): every mutation logs a WAL entry before modifying Dictionary. Zero data loss window. Default for production
  2. **Periodic checkpoint**: full Dictionary snapshot every N seconds + delta WAL between checkpoints. Bounded data loss window
  3. **Flush-on-fsync**: WAL entries written only when fsync() is called. Application controls durability
  4. **No WAL** (unsafe, fast): Dictionary only persisted on clean unmount. For scratch/temp mounts explicitly optimized for speed
- **Dirty mount detection**: lock file in store directory. Created on mount, removed on clean unmount. If lock file exists at next mount = dirty → WAL replay
- WAL replay on dirty mount restores last committed state without manual intervention

### GC storage model — log-structured segments
- **Log-structured append-only segments** for Dictionary persistence (replaces single dictionary.bin)
- New Dictionary entries appended to the current segment file. Segments are immutable once closed
- **Compaction** = read segment, skip entries with refcount=0 (unreachable), write survivors to new segment, delete old segment
- **All writes sequential** — appends and compaction. No random in-place mutations. SSD-optimal
- Matches CAS immutability: Dictionary entries are never modified, only inserted and eventually garbage-collected
- The dictionary.bin format from Phases 2-4 is replaced by the segment-based format in this phase

### GC trigger and reclamation
- **Both background and on-demand GC**:
  - Background GC thread during mount: runs periodically or when orphan count exceeds threshold. Non-blocking to FUSE operations
  - Offline CLI command: `slicefs gc <store>` runs GC with filesystem unmounted. For deep compaction

### Snapshot-aware GC safety
- **Multi-root mark-and-sweep** for liveness determination
- GC walks ALL live roots (current root + all pinned snapshot roots). Any entry reachable from ANY root is live. Unreachable entries are garbage
- Safe with immutable Merkle tree: tree doesn't change during GC scan. New entries created during GC go into current segment (not being compacted)
- No refcount mutation during liveness check — mark-and-sweep avoids mutating state during the scan
- Shared entries between snapshots are naturally handled (reached from multiple roots, marked once)

### fsync/fdatasync durability
- **fsync flushes WAL + buffer to segment** — flush file's write buffer through CAS pipeline, append new entries to current segment, fsync the segment file. Behavior depends on active WAL strategy
- **fdatasync treated same as fsync for now** — optimization to skip metadata-only writes deferred to Phase 7 if needed

### Claude's Discretion
- WAL entry format (binary, self-describing records)
- Segment file naming and rotation policy (size-based or time-based)
- Compaction heuristics (which segments to compact, when to trigger)
- Background GC thread scheduling (interval, orphan threshold)
- Lock file format and cleanup strategy
- How to handle WAL corruption (skip corrupted entries vs refuse to mount)

</decisions>

<code_context>
## Existing Code Insights

### Reusable Assets
- `DictMetadataStore` with refcounts (increment/decrement/get_refcount) — Phase 4
- `serialize_dictionary`/`deserialize_dictionary` — will be replaced by segment format but informs the entry serialization
- `commit()`/`load_from_root()` — root record persistence (184 bytes)
- `destroy()` in SliceFsFilesystem — already wired for persistence on unmount
- All FUSE write callbacks implemented — fsync needs to be added

### Established Patterns
- `Mutex<Dictionary>` for thread-safe access
- 92-byte entry format (Digest224 key + Branches value)
- Root record with refcount digest

### Integration Points
- New `crates/slicefs-wal/` or module in metadata crate — WAL engine
- `SliceFsFilesystem::fsync()` callback — currently not implemented
- `slicefs gc <store>` CLI subcommand — new clap subcommand
- Mount command gets `--wal-strategy` flag
- Background GC thread spawned in mount command

</code_context>

<specifics>
## Specific Ideas

- The WAL strategy is mount-time configurable because different use cases need different tradeoffs: production mounts want per-operation safety, development/scratch mounts want speed
- Log-structured segments are the natural fit for immutable CAS entries — append-only matches how SSDs work internally
- Multi-root mark-and-sweep for GC liveness is the gold standard — correct, precise, and doesn't require mutating refcounts during the scan
- The transition from dictionary.bin to segments is the major storage format change in v1

</specifics>

<deferred>
## Deferred Ideas

- **fdatasync optimization** — skip metadata writes when only data changed. Phase 7 if needed
- **TRIM/discard hints** — SSD TRIM support for deleted segments. Phase 7
- **Concurrent compaction** — compact while filesystem is mounted with minimal locking. Optimize in Phase 7 if background GC is insufficient
- **WAL compression** — compress WAL entries to reduce write amplification. Future optimization

</deferred>

---

*Phase: 05-crash-safety-and-gc*
*Context gathered: 2026-03-28*
