# Phase 4: Full POSIX Write Path - Context

**Gathered:** 2026-03-28
**Status:** Ready for planning

<domain>
## Phase Boundary

Complete read/write POSIX filesystem with inline deduplication. Files can be created, written, modified, renamed, deleted, and linked through the mount point. Real tools (editors, package managers, build systems) work correctly. Custom POSIX compliance test suite validates correctness on macOS; pjdfstest >95% on Linux (run manually, CI deferred to Phase 7).

Requirements: POSIX-01, POSIX-02, POSIX-03, POSIX-04, POSIX-05, POSIX-09, POSIX-12, POSIX-14, CAS-04, CAS-06

</domain>

<decisions>
## Implementation Decisions

### Write buffering strategy
- Buffer full file in `Vec<u8>` per file handle, then `State::push_all` on flush/close — simple, correct, memory = file size
- data-id's CDC has no configurable chunk size — boundaries emerge from content via digest comparison. No changes to data-id needed
- Dictionary serialization stays as `dictionary.bin` for Phase 4 — upgrade to page-aligned format deferred to Phase 5/7 when crash safety needs it
- data-id untouched — all serialization is in our code, not data-id's

### Storage format
- Keep current `dictionary.bin` + `root.bin` format — compact Merkle tree (54 entries for 1MB file, logarithmic growth)
- data-id's Dictionary entries are 92 bytes each, packed contiguously — no per-block file alignment concern with this format
- Page-aligned or append-only format deferred to Phase 5/7

### Atomic rename semantics
- Full cross-directory rename — rename(old_parent, old_name, new_parent, new_name) works across directories. Covers mv, editor save-to-temp patterns, POSIX-03
- Rename overwrites defer cleanup — remove the directory entry for the overwritten file but leave orphaned content in Dictionary. GC in Phase 5 reclaims it

### Hard link and refcount model
- nlinks tracking in InodeMeta — increment on link(), decrement on unlink(). When nlinks reaches 0, remove inode from inode map but leave content in Dictionary for GC
- Per-Digest224 reference counting (CAS-04) — implement refcounts now so Phase 5 GC has them ready. Atomic increment on store, decrement on content replacement/deletion
- No content removal from Dictionary in Phase 4 — GC (Phase 5) handles physical cleanup

### Dedup-aware space reporting (CAS-06)
- statfs reports both logical and physical byte counts showing the dedup ratio
- Logical = sum of all file sizes (what users see)
- Physical = Dictionary entry count * 92 bytes (what's actually stored)

### pjdfstest compliance
- Custom POSIX test suite in Rust for local macOS testing — integration tests that exercise POSIX operations through the mount point
- pjdfstest on Linux run manually for Phase 4 verification — the >95% compliance gate
- GitHub Actions CI for pjdfstest deferred to Phase 7 (Production Hardening)

### Claude's Discretion
- Open file handle table design (HashMap<FileHandle, OpenFileState> with write buffer)
- Truncate/ftruncate implementation strategy (rebuild content tree from truncated buffer)
- Symlink storage model (target path as file content or inline in inode)
- POSIX locking (fcntl/flock) implementation approach
- Which pjdfstest categories to skip for the remaining <5%

</decisions>

<code_context>
## Existing Code Insights

### Reusable Assets
- `SliceFsFilesystem` with all read callbacks — write callbacks currently return EROFS, need to be replaced with real implementations
- `DictMetadataStore` with inode CRUD, directories, manifests, xattrs, commit/load_from_root
- data-id `State::push_all` for content chunking (used in seed command)
- `serialize_dictionary`/`deserialize_dictionary` for persistence
- clap CLI with mount/unmount/seed subcommands

### Established Patterns
- `&self` + `Mutex<Dictionary>` for thread-safe FUSE callbacks
- Dual Arc: `meta` (DictMetadataStore) + `dict` (Dictionary clone) to avoid deadlocks
- `meta_error_to_fuse_errno` for error translation
- `inode_to_file_attr` for InodeMeta → FileAttr conversion

### Integration Points
- `SliceFsFilesystem` write callbacks: replace EROFS returns with real implementations
- Need per-file-handle write buffer (new state management in filesystem.rs)
- `DictMetadataStore` already has all needed mutation methods (create_inode, link, unlink, set_manifest, set_xattr)
- Mount command needs to remove the read-only MountOption::RO

</code_context>

<specifics>
## Specific Ideas

- data-id's Merkle tree is extremely compact: 54 Dictionary entries for a 1MB file, logarithmic growth. Storage overhead is minimal
- Dedup is implicit and already proven: 10 identical files share content blocks (tested end-to-end)
- The write path is "buffer → State::push_all → update manifest → commit" — straightforward pipeline
- Editors (vim/emacs) use write-to-temp + rename — this is the critical path to test
- The nlinks + refcount model gives Phase 5 GC everything it needs without content deletion complexity in Phase 4

</specifics>

<deferred>
## Deferred Ideas

- **Page-aligned dictionary format** — Phase 5/7 when crash safety and mmap() matter
- **GitHub Actions CI with pjdfstest** — Phase 7 (Production Hardening)
- **Content removal from Dictionary** — Phase 5 (GC handles physical cleanup)
- **data-id modifications** — no changes needed; all optimization is in our serialization layer
- **Incremental State feeding** — potential optimization for streaming writes without full-file buffering (future)

</deferred>

---

*Phase: 04-full-posix-write-path*
*Context gathered: 2026-03-28*
