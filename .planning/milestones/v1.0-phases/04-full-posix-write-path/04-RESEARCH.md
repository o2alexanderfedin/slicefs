# Phase 4: Full POSIX Write Path - Research

**Researched:** 2026-03-27
**Domain:** FUSE write path, POSIX semantics, CDC dedup, file handle state management
**Confidence:** HIGH

---

<user_constraints>
## User Constraints (from CONTEXT.md)

### Locked Decisions
- **Write buffering:** Buffer full file in `Vec<u8>` per file handle, then `State::push_all` on flush/close — simple, correct, memory = file size
- **No data-id changes:** data-id's CDC has no configurable chunk size — boundaries emerge from content via digest comparison. No changes to data-id needed
- **Dictionary serialization stays as `dictionary.bin`** for Phase 4 — upgrade to page-aligned format deferred to Phase 5/7
- **data-id untouched** — all serialization is in our code, not data-id's
- **Keep current `dictionary.bin` + `root.bin` format** — compact Merkle tree (54 entries for 1MB file, logarithmic growth)
- **Full cross-directory rename** — rename(old_parent, old_name, new_parent, new_name) works across directories. Covers mv, editor save-to-temp patterns, POSIX-03
- **Rename overwrites defer cleanup** — remove the directory entry for the overwritten file but leave orphaned content in Dictionary. GC in Phase 5 reclaims it
- **nlinks tracking in InodeMeta** — increment on link(), decrement on unlink(). When nlinks reaches 0, remove inode from inode map but leave content in Dictionary for GC
- **Per-Digest224 reference counting (CAS-04)** — implement refcounts now so Phase 5 GC has them ready. Atomic increment on store, decrement on content replacement/deletion
- **No content removal from Dictionary in Phase 4** — GC (Phase 5) handles physical cleanup
- **statfs reports both logical and physical byte counts** showing the dedup ratio
- **Logical = sum of all file sizes; Physical = Dictionary entry count * 92 bytes**
- **Custom POSIX test suite in Rust** for local macOS testing — integration tests that exercise POSIX operations through the mount point
- **pjdfstest on Linux run manually** for Phase 4 verification — the >95% compliance gate
- **GitHub Actions CI for pjdfstest deferred to Phase 7**

### Claude's Discretion
- Open file handle table design (HashMap<FileHandle, OpenFileState> with write buffer)
- Truncate/ftruncate implementation strategy (rebuild content tree from truncated buffer)
- Symlink storage model (target path as file content or inline in inode)
- POSIX locking (fcntl/flock) implementation approach
- Which pjdfstest categories to skip for the remaining <5%

### Deferred Ideas (OUT OF SCOPE)
- Page-aligned dictionary format — Phase 5/7 when crash safety and mmap() matter
- GitHub Actions CI with pjdfstest — Phase 7 (Production Hardening)
- Content removal from Dictionary — Phase 5 (GC handles physical cleanup)
- data-id modifications — no changes needed; all optimization is in our serialization layer
- Incremental State feeding — potential optimization for streaming writes without full-file buffering (future)
</user_constraints>

---

<phase_requirements>
## Phase Requirements

| ID | Description | Research Support |
|----|-------------|-----------------|
| POSIX-01 | File read/write/create/delete operations | fuser write callbacks, create/unlink FUSE ops, write buffer flush pipeline |
| POSIX-02 | Directory create/delete/list (readdir with . and .. entries) | mkdir/rmdir callbacks, DictMetadataStore.create_directory/unlink already exists |
| POSIX-03 | Atomic rename (rename(2)) for editors, package managers | fuser rename callback with RenameFlags, cross-dir move in DictMetadataStore |
| POSIX-04 | Symbolic links (symlink/readlink) | fuser symlink/readlink callbacks, symlink target stored as file content |
| POSIX-05 | Hard links (link(2)) with correct inode-level reference counting | fuser link callback, nlinks increment in InodeMeta, unlink decrements |
| POSIX-09 | Truncate/ftruncate with correct partial block handling | setattr(size=N) path, truncate buffer then State::push_all |
| POSIX-12 | POSIX locking (fcntl locks, flock) | getlk/setlk callbacks in fuser; kernel handles local locking if ENOSYS returned |
| POSIX-14 | pjdfstest pass rate >95% | Custom Rust test suite for macOS; pjdfstest manually on Linux |
| CAS-04 | Reference counting per block with atomic increment/decrement | Refcount BTreeMap<Digest224, u64> in DictMetadataStore, incremented on push_all |
| CAS-06 | Dedup-aware space reporting (logical size vs physical size via statfs) | statfs: logical=sum(inode.size), physical=dict.len()*92 bytes |
</phase_requirements>

---

## Summary

Phase 4 transforms SliceFS from read-only to fully writable POSIX by replacing the EROFS stub returns in `SliceFsFilesystem` with real implementations. The write path is a buffer-then-flush pipeline: `open` allocates a `Vec<u8>` in a per-file-handle table, `write` appends to that buffer, and `release`/`flush` calls `State::push_all` to deduplicate the content and updates the manifest. All required FUSE callbacks are already present as stubs in `filesystem.rs`; none need to be added to the struct layout.

The critical concurrency concern is that `SliceFsFilesystem` uses `&self` throughout (required by fuser's `Filesystem` trait), so the per-handle write buffer table must use interior mutability (`Mutex<HashMap<FileHandle, OpenFileState>>`). The dual-Arc pattern already established (meta + dict as separate Arcs) continues to work — writes go through `meta.dict()` accessor, which is the same Dictionary used for content.

Post-write persistence requires removing the `MountOption::RO` from `build_mount_options`, and after `mount2` returns (when the session ends), serializing the Dictionary and root to `dictionary.bin` + `root.bin`. The `destroy()` callback already calls `meta.commit()`, so the root digest is available; what's missing is the serialization step after `mount2` returns.

**Primary recommendation:** Implement write operations in `filesystem.rs` using a `Mutex<HashMap<FileHandle, OpenFileState>>` for the write buffer table. The buffer→push_all→manifest→commit pipeline mirrors what `seed.rs` already does. The MountOption::RO removal and post-session serialization in `mount.rs` completes the picture.

---

## Standard Stack

### Core

| Library | Version | Purpose | Why Standard |
|---------|---------|---------|--------------|
| fuser | 0.17 | FUSE kernel interface — Filesystem trait with all POSIX callbacks | Already in workspace; all stubs present |
| blockset (data-id) | workspace | CDC content addressing — State::push_all for dedup write | Already proven in seed.rs; no changes needed |
| metadata (DictMetadataStore) | workspace | Inode CRUD, directories, manifests, xattrs | Fully implemented; all write methods exist |
| std::sync::Mutex | stdlib | Interior mutability for &self write buffer table | Zero-cost synchronization for single-mount use case |
| std::collections::HashMap | stdlib | Per-file-handle open state storage | O(1) lookup by FileHandle(u64) |

### Supporting

| Library | Version | Purpose | When to Use |
|---------|---------|---------|-------------|
| libc | 0.2 | POSIX error constants (ENOTSUP, etc.) for locking stubs | POSIX-12 — return ENOTSUP for setlk when local kernel handles it |
| tempfile | 3 | Tempdir for integration tests | Test harness for Rust POSIX test suite |

### Alternatives Considered

| Instead of | Could Use | Tradeoff |
|------------|-----------|----------|
| Mutex<HashMap> for handle table | RwLock<HashMap> | RwLock gains nothing — writes modify the map exclusively |
| Buffer full file in Vec<u8> | Incremental State feeding | Incremental is more complex and deferred per user decision |
| Store symlink target as file content | Inline in InodeMeta | Content approach reuses existing manifest/read path; no inode format changes |

**Installation:** No new dependencies needed. All required crates already in workspace.

---

## Architecture Patterns

### Recommended Project Structure

The phase stays within existing crate layout. Changes are:
```
crates/slicefs-cli/src/
├── filesystem.rs     # PRIMARY: replace EROFS stubs with real write implementations
├── mount.rs          # Remove RO option, add post-session serialization
└── (new) write_state.rs  # OpenFileState struct + WriteHandleTable type alias (optional extraction)

crates/metadata/src/
└── store.rs          # Add refcount BTreeMap<Digest224, u64>, increment/decrement methods
```

### Pattern 1: Per-Handle Write Buffer (POSIX-01, POSIX-09)

**What:** `SliceFsFilesystem` gains a `Mutex<HashMap<FileHandle, OpenFileState>>` field.
`OpenFileState` holds `ino: u64` and `buf: Vec<u8>`.

**When to use:** Every open file that was opened with a writable flag gets an entry. Read-only opens skip the table (return FileHandle(0), no entry needed).

**Example:**
```rust
// Source: filesystem.rs analysis + fuser 0.17 API
struct OpenFileState {
    ino: u64,
    buf: Vec<u8>,
}

pub struct SliceFsFilesystem {
    pub(crate) meta: Arc<DictMetadataStore>,
    pub(crate) dict: Arc<Mutex<Dictionary>>,
    // NEW: write state per open file handle
    open_files: Mutex<HashMap<u64, OpenFileState>>, // key: FileHandle.0
}
```

**FileHandle allocation:** Use a simple atomic counter (`AtomicU64`) to issue unique handles. FileHandle(0) reserved for read-only opens.

```rust
// In open(): if flags contain O_WRONLY or O_RDWR, allocate a handle
let fh = self.next_fh.fetch_add(1, Ordering::Relaxed) + 1; // 1-based
self.open_files.lock().unwrap().insert(fh, OpenFileState { ino: ino.0, buf: vec![] });
reply.opened(FileHandle(fh), FopenFlags::empty());
```

### Pattern 2: Write → Buffer Accumulation (POSIX-01)

**What:** `write()` callback extends the buffer at the specified offset. For the simple full-buffer approach: treat writes as sequential (offset == buf.len() is the common case for new files). For overwrites/random writes: resize buffer and copy at offset.

**Example:**
```rust
// Source: filesystem.rs analysis
fn write(&self, _req: &Request, _ino: INodeNo, fh: FileHandle,
         offset: u64, data: &[u8], ..., reply: ReplyWrite) {
    let mut open_files = self.open_files.lock().unwrap();
    if let Some(state) = open_files.get_mut(&fh.0) {
        let end = offset as usize + data.len();
        if end > state.buf.len() {
            state.buf.resize(end, 0);
        }
        state.buf[offset as usize..end].copy_from_slice(data);
        reply.written(data.len() as u32);
    } else {
        reply.error(Errno::EBADF);
    }
}
```

### Pattern 3: Flush → push_all → manifest → commit (POSIX-01, CAS-04)

**What:** On `release()` (the definitive close), flush the buffer through CDC and persist.

**When to use:** `release` is called exactly once per `open`; `flush` may be called multiple times (on `close()` of each dup'd fd). Flush buffer in `release`, not `flush`.

**Example:**
```rust
// Source: seed.rs pattern (State::push_all usage proven)
fn release(&self, _req: &Request, ino: INodeNo, fh: FileHandle, ..., reply: ReplyEmpty) {
    let state = {
        let mut open_files = self.open_files.lock().unwrap();
        open_files.remove(&fh.0)
    };
    if let Some(s) = state {
        // CDC dedup via data-id
        let content_digest = {
            let mut dict = self.dict.lock().unwrap();
            State::push_all(&mut *dict, &s.buf)
        };
        // Update manifest
        let _ = self.meta.set_manifest(s.ino, &[content_digest]);
        // Update inode size + mtime
        if let Ok(mut inode) = self.meta.get_inode(s.ino) {
            inode.size = s.buf.len() as u64;
            inode.mtime_sec = now_secs();  // SystemTime::now()
            let _ = self.meta.update_inode(&inode);
        }
        // Increment refcount for content_digest (CAS-04)
        self.meta.increment_refcount(&content_digest);
    }
    reply.ok();
}
```

### Pattern 4: File Creation — create() callback (POSIX-01)

**What:** `create()` atomically creates an inode, adds the directory entry, opens the file, and returns both the inode attrs and a file handle.

**Example:**
```rust
fn create(&self, req: &Request, parent: INodeNo, name: &OsStr,
          mode: u32, umask: u32, _flags: i32, reply: ReplyCreate) {
    let meta = InodeMeta::new_file(0, req.uid(), req.gid(),
                                   S_IFREG | (mode & !umask & 0o7777));
    let ino = match self.meta.create_inode(&meta) { ... };
    let _ = self.meta.link(parent.0, name.to_str().unwrap_or(""), ino);
    // Open with write buffer
    let fh = self.next_fh.fetch_add(1, Ordering::Relaxed) + 1;
    self.open_files.lock().unwrap().insert(fh, OpenFileState { ino, buf: vec![] });
    let attr = inode_to_file_attr(&self.meta.get_inode(ino).unwrap());
    reply.created(&TTL, &attr, Generation(0), FileHandle(fh), FopenFlags::empty());
}
```

### Pattern 5: Rename — cross-directory (POSIX-03)

**What:** rename() must handle: same-dir rename, cross-dir rename, overwrite of existing target. The DictMetadataStore has `link()` + `unlink()` — compose them.

**Logic:**
1. Check if `newname` already exists in `newparent` — if so, `unlink(newparent, newname)` (orphans old content, GC handles it)
2. `link(newparent, newname, src_ino)` — add new directory entry
3. `unlink(oldparent, oldname)` — remove old directory entry
4. nlinks stays unchanged (same inode, just moved)

**Note on RenameFlags:** fuser passes `RENAME_EXCHANGE` or `RENAME_NOREPLACE`. Return ENOSYS for RENAME_EXCHANGE in Phase 4.

### Pattern 6: Hard Link (POSIX-05)

**What:** `link()` adds another directory entry pointing to the same inode, increments nlinks.

```rust
fn link(&self, _req: &Request, ino: INodeNo, newparent: INodeNo,
        newname: &OsStr, reply: ReplyEntry) {
    let _ = self.meta.link(newparent.0, newname.to_str().unwrap_or(""), ino.0);
    // Increment nlinks
    let mut inode = self.meta.get_inode(ino.0).unwrap();
    inode.nlinks += 1;
    let _ = self.meta.update_inode(&inode);
    let attr = inode_to_file_attr(&inode);
    reply.entry(&TTL, &attr, Generation(0));
}
```

### Pattern 7: Unlink / nlinks to 0 (POSIX-01, POSIX-05)

**What:** `unlink()` removes directory entry, decrements nlinks. At nlinks == 0: delete inode from map but leave Dictionary content (GC Phase 5).

```rust
fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
    let ino = self.meta.lookup(parent.0, name_str).unwrap();
    let _ = self.meta.unlink(parent.0, name_str);
    let mut inode = self.meta.get_inode(ino).unwrap();
    inode.nlinks -= 1;
    if inode.nlinks == 0 {
        let _ = self.meta.delete_inode(ino);
        // Decrement refcount for content (CAS-04) — content stays in Dictionary
        if let Ok(manifest) = self.meta.get_manifest(ino) {
            for digest in manifest {
                self.meta.decrement_refcount(&digest);
            }
        }
    } else {
        let _ = self.meta.update_inode(&inode);
    }
    reply.ok();
}
```

### Pattern 8: Symlink storage (POSIX-04)

**What:** Store symlink target as file content (same pipeline as regular file write). Mode bits use S_IFLNK. `readlink()` reads the content bytes and returns them as the path.

**Why content over inline:** Reuses the existing manifest + GetBytes read path with zero new code paths. Symlink targets rarely exceed 256 bytes so CDC overhead is trivial.

```rust
fn symlink(&self, req: &Request, parent: INodeNo, link_name: &OsStr,
           target: &Path, reply: ReplyEntry) {
    let target_bytes = target.as_os_str().as_bytes();
    let meta = InodeMeta { mode: S_IFLNK | 0o777, nlinks: 1, size: target_bytes.len() as u64, ... };
    let ino = self.meta.create_inode(&meta).unwrap();
    let _ = self.meta.link(parent.0, link_name_str, ino);
    let content_digest = { let mut dict = self.dict.lock().unwrap(); State::push_all(&mut *dict, target_bytes) };
    let _ = self.meta.set_manifest(ino, &[content_digest]);
    reply.entry(&TTL, &inode_to_file_attr(...), Generation(0));
}
```

### Pattern 9: setattr — truncate path (POSIX-09)

**What:** `setattr(size=N)` implements truncate. Read current content, truncate the Vec<u8> to N bytes (pad with zeros if extending), push_all with new content, update manifest and inode size.

**For in-flight files (open fh):** Truncate the open buffer in the write table.
**For closed files:** Read via GetBytes, truncate, re-push_all, update manifest.

### Pattern 10: Persistence after session end (POSIX-01)

**What:** After `mount2` returns in `mount.rs`, serialize the updated Dictionary and root to disk. This is the missing step for Phase 4.

```rust
// In run_mount() after mount2 returns:
// fs has been consumed by mount2; destroy() called meta.commit().
// Phase 4: re-load is not needed — we need to persist before mount2 consumes fs.
// Solution: persist via destroy() callback, OR capture root/dict before mount2.
```

**Recommended approach:** The `destroy()` callback already calls `meta.commit()`. Extend it to also serialize:
```rust
fn destroy(&mut self) {
    if let Ok(root) = self.meta.commit() {
        let dict = self.dict.lock().unwrap();
        let bytes = serialize_dictionary(&*dict);
        drop(dict);
        if let Some(store_path) = &self.store_path {
            let _ = std::fs::write(store_path.join("dictionary.bin"), &bytes);
            let mut root_bytes = Vec::with_capacity(28);
            for word in &root { root_bytes.extend_from_slice(&word.to_le_bytes()); }
            let _ = std::fs::write(store_path.join("root.bin"), &root_bytes);
        }
    }
}
```

This requires `SliceFsFilesystem` to hold `store_path: Option<PathBuf>`.

### Pattern 11: statfs — dedup-aware (CAS-06)

**What:** Walk all inodes to sum logical bytes; count Dictionary entries for physical bytes.

```rust
fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
    let logical_bytes: u64 = self.meta.all_inodes().iter().map(|i| i.size).sum();
    let physical_entries: u64 = {
        let dict = self.dict.lock().unwrap();
        dict.len() as u64
    };
    let physical_bytes = physical_entries * 92; // 92 bytes/entry verified empirically
    let bsize: u32 = 4096;
    let blocks = logical_bytes / bsize as u64 + 1;
    let bfree = u64::MAX / 2; // effectively unlimited (dedup means true free is hard to state)
    reply.statfs(blocks, bfree, bfree, u64::MAX, u64::MAX, bsize, 255, 0);
}
```

**Note:** DictMetadataStore needs an `all_inodes()` method to enumerate inode sizes for the logical sum. Alternative: track logical_total as a running counter in DictMetadataStore.

### Pattern 12: Refcount table (CAS-04)

**What:** A `BTreeMap<Digest224, u64>` in `DictMetadataStore` tracking how many inodes reference each content digest. Incremented when a manifest is set, decremented when a manifest is replaced or inode deleted.

**Where:** Add to `DictMetadataStore` as `refcounts: Mutex<BTreeMap<Digest224, u64>>`.

**Methods to add:**
- `increment_refcount(&self, digest: &Digest224)` — atomically increment
- `decrement_refcount(&self, digest: &Digest224)` — decrement; remove entry at 0
- `get_refcount(&self, digest: &Digest224) -> u64` — for GC (Phase 5)

**Serialization:** Persist refcounts in the root record or a separate `refcounts.bin` alongside `dictionary.bin`. Simplest for Phase 4: add to root record expansion (already 156 bytes; add refcount_data Digest224 to reach 184 bytes).

### Anti-Patterns to Avoid

- **Holding dict lock while calling meta methods:** DictMetadataStore acquires `dict` lock internally. Release dict before any meta call to prevent deadlock (documented locking order in store.rs).
- **Using FileHandle(0) for writable files:** 0 is the sentinel for read-only opens; allocate from atomic counter starting at 1.
- **Committing on every write:** `commit()` is expensive (re-serializes all maps). Only commit in `destroy()` or on explicit `fsync()`.
- **Calling State::push_all with empty data:** Returns a valid digest for empty content. Empty files are valid — manifest will be `[zero_digest]` or `[]`. Keep consistent: use `[]` (empty manifest) for empty files.
- **Decrementing nlinks below 0:** `u32` wraps. Guard with `if inode.nlinks > 0` before decrement.

---

## Don't Hand-Roll

| Problem | Don't Build | Use Instead | Why |
|---------|-------------|-------------|-----|
| Content-defined chunking | Custom CDC | `blockset::State::push_all` | Already proven: 54 entries/1MB, dedup verified across 10 identical files |
| Dictionary key storage | Custom hash map | `blockset::Dictionary` (BTreeMap<Digest224, Branches>) | Already integrated; serialize_dictionary/deserialize_dictionary exist |
| File content reads on readlink | Custom byte extraction | `GetData` + `GetBytes` iterators | Used in `read()` callback; same pattern works for symlink targets |
| Serialization of new struct fields | bincode/serde | Existing manual LE byte packing | Consistent with InodeMeta's 56-byte layout; no new dependencies |
| POSIX lock enforcement | Custom lock table | Return ENOSYS for getlk/setlk | Linux kernel handles local locking when ENOSYS is returned — pjdfstest skips these tests on ENOSYS |

**Key insight:** The seed.rs file already implements the entire write pipeline (create inode → push_all → set_manifest). Phase 4 wires the same pipeline to FUSE callbacks instead of a bulk import walk.

---

## Common Pitfalls

### Pitfall 1: MountOption::RO not removed
**What goes wrong:** All write callbacks receive EROFS — nothing works even after implementing the methods.
**Why it happens:** `build_mount_options()` in `mount.rs` hardcodes `MountOption::RO`. FUSE enforces read-only at the kernel level before callbacks are invoked.
**How to avoid:** Remove `MountOption::RO` from `build_mount_options()` as the first change.
**Warning signs:** write/create returning EROFS immediately in logs despite implementation.

### Pitfall 2: Dictionary not persisted after writable session
**What goes wrong:** Files written through the mount disappear after unmount.
**Why it happens:** Phase 3's `run_mount()` has a comment "Phase 4 will need post-session serialization". `mount2` consumes `fs` — there is no way to access state after `mount2` returns unless `destroy()` persists it.
**How to avoid:** Move serialization into `destroy()` callback (see Pattern 10). `SliceFsFilesystem` must hold `store_path`.
**Warning signs:** Load_store after remount shows no files written during previous session.

### Pitfall 3: Double-lock deadlock on write path
**What goes wrong:** Rust panics with "cannot lock a Mutex that is already locked by the current thread" (or silent deadlock if not using std Mutex).
**Why it happens:** `DictMetadataStore` internally locks `self.dict`. If caller holds `fs.dict` lock and then calls any `meta.*` method, the same mutex is attempted twice.
**How to avoid:** In write pipeline — lock `fs.dict` only briefly for `State::push_all`, drop before any `meta.*` call. Never hold both locks simultaneously.
**Warning signs:** Process hangs on first file write.

### Pitfall 4: setattr size field handling for truncate
**What goes wrong:** Truncate extends file with garbage bytes, or truncation of in-flight files doesn't affect the open buffer.
**Why it happens:** Two cases: (1) file is closed — must read content, truncate, re-push; (2) file is open — must find the open handle in `open_files` table by ino (the setattr doesn't pass a reliable fh for truncate). fh is `Option<FileHandle>` in setattr — use it when present.
**How to avoid:** In setattr: if `fh` is Some, truncate the open buffer directly. If None, read via GetBytes, truncate Vec, push_all.
**Warning signs:** pjdfstest truncate tests failing.

### Pitfall 5: nlinks inconsistency on rmdir
**What goes wrong:** `rmdir` on a directory leaves stale nlinks or orphaned entries, causing future lookups to fail mysteriously.
**Why it happens:** Directory nlinks has the parent's `..` pointing at it. Removing a directory must: (1) verify it's empty, (2) unlink entry from parent, (3) update parent nlinks (decrement for each removed subdir). The `..` link from the deleted dir to the parent should also be accounted for.
**How to avoid:** `rmdir` path: assert directory is empty (only `.` and `..`), call `meta.unlink(parent, name)`, `meta.delete_inode(ino)`, decrement parent nlinks.
**Warning signs:** `stat` on parent directory shows incorrect nlinks; pjdfstest rmdir failures.

### Pitfall 6: rename() RENAME_EXCHANGE flag
**What goes wrong:** Some tools use `renameat2` with `RENAME_EXCHANGE` — atomically swap two files. Returning EINVAL causes unexpected failures.
**Why it happens:** fuser 0.17 passes `RenameFlags` to `rename()`. The default unimplemented stub returns ENOSYS.
**How to avoid:** Return `ENOSYS` for `RENAME_EXCHANGE` flag explicitly. For normal rename (flags == 0), implement fully. For `RENAME_NOREPLACE`, check if target exists and return EEXIST if so.
**Warning signs:** mv or atomic editor saves failing unexpectedly.

### Pitfall 7: write() offset does not match buffer length
**What goes wrong:** File content is corrupted with zero-padding or data written at wrong offset.
**Why it happens:** Some programs use non-sequential writes (e.g., write at offset 1000 before filling offset 0). The buffer must be resized and zero-padded at gaps.
**How to avoid:** Always `resize(max(end, buf.len()), 0)` before copying data at offset. This covers both sequential and sparse writes.
**Warning signs:** Text editors (vim) save files with corruption.

---

## Code Examples

Verified patterns from official sources and existing codebase:

### State::push_all usage (from seed.rs, confirmed working)
```rust
// Source: crates/slicefs-cli/src/seed.rs:98-100
let content_digest: Digest224 = {
    let mut dict = meta_store.dict().lock().unwrap();
    State::push_all(&mut *dict, &bytes)
};
```

### GetBytes for content reads (from filesystem.rs:read(), confirmed working)
```rust
// Source: crates/slicefs-cli/src/filesystem.rs:234-240
let dict = self.dict.lock().unwrap();
let get_data = GetData::new(&*dict, &root_digest256);
let bytes: Vec<u8> = GetBytes::new(get_data)
    .skip(offset as usize)
    .take(size as usize)
    .collect();
```

### DictMetadataStore mutation methods (all confirmed in store.rs)
```rust
// Confirmed method signatures:
meta.create_inode(&meta) -> Result<InodeId, MetaError>
meta.update_inode(&meta) -> Result<(), MetaError>
meta.delete_inode(ino) -> Result<(), MetaError>
meta.link(parent_ino, name, ino) -> Result<(), MetaError>
meta.unlink(parent_ino, name) -> Result<(), MetaError>
meta.set_manifest(ino, &[digest]) -> Result<(), MetaError>
meta.create_directory(parent_ino, name, &meta) -> Result<InodeId, MetaError>
```

### fuser 0.17 create() reply signature
```rust
// Source: fuser 0.17 lib.rs line 842
reply.created(&TTL, &attr, Generation(0), FileHandle(fh), FopenFlags::empty());
```

### fuser 0.17 rename flags
```rust
// Source: fuser 0.17 lib.rs line 537-551
fn rename(&self, _req: &Request, parent: INodeNo, name: &OsStr,
          newparent: INodeNo, newname: &OsStr, flags: RenameFlags, reply: ReplyEmpty)
// flags.contains(RenameFlags::RENAME_EXCHANGE) -> return Errno::ENOSYS
// flags.contains(RenameFlags::RENAME_NOREPLACE) -> check target, return EEXIST if exists
// flags == RenameFlags::empty() -> normal rename with overwrite
```

### Dictionary entry count for statfs (CAS-06)
```rust
// Source: Phase 2 decision: 92 bytes/entry verified empirically
// Dictionary is BTreeMap<[u32;7], [[u32;8];2]> — each entry is 28 + 64 = 92 bytes
let physical_bytes = dict.len() as u64 * 92;
```

### Mount without RO
```rust
// Source: mount.rs:88-101 (remove RO from vec)
let mut mount_options = vec![
    // Remove: MountOption::RO,
    MountOption::FSName("slicefs".to_string()),
    MountOption::DefaultPermissions,
];
```

---

## State of the Art

| Old Approach | Current Approach | When Changed | Impact |
|--------------|------------------|--------------|--------|
| Write callbacks return EROFS | Real write implementations | Phase 4 | Filesystem becomes writable |
| Hardcoded RO mount option | RW mount, persist on destroy | Phase 4 | Session data survives unmount |
| No per-handle state | Mutex<HashMap<FileHandle, OpenFileState>> | Phase 4 | Enables buffered writes |
| No refcounts | Mutex<BTreeMap<Digest224, u64>> | Phase 4 | Enables Phase 5 GC |

**Deprecated/outdated in Phase 4:**
- `MountOption::RO` in `build_mount_options()` — must be removed
- Hardcoded `statfs` values (1M blocks, 0 free) — must reflect real dedup accounting

---

## Open Questions

1. **DictMetadataStore all_inodes() enumeration for statfs logical size**
   - What we know: inode_data is a `BTreeMap<u64, Digest224>` — we can iterate keys and load each inode
   - What's unclear: Whether to add a dedicated `all_inodes() -> Vec<InodeMeta>` method or track a running `logical_total: Mutex<u64>` counter
   - Recommendation: Running counter is O(1) vs O(N) walk. Add `logical_bytes: Mutex<u64>` to `DictMetadataStore`, increment on create/update, decrement on delete. More complex but better for large filesystems.

2. **pjdfstest categories to skip (<5%)**
   - What we know: pjdfstest covers chflags, flock, NFSv4 ACLs, O_EXLOCK, and platform-specific behaviors. Many are macOS-only or Linux-only extensions.
   - What's unclear: Which specific test categories produce FUSE failures we can't fix in Phase 4
   - Recommendation: When running on Linux, skip: `chflags` (macOS-only), `mkfifo`/`mknod` device nodes (FUSE limitation), `flock` across processes (FUSE option needed). Target: skip <5% by count.

3. **Refcount persistence format**
   - What we know: Root record is 156 bytes; we need to add a `refcount_data: Digest224` (28 bytes) to reach 184 bytes
   - What's unclear: Whether to store refcounts as a CAS blob (intern via push_all) or a separate `refcounts.bin` file
   - Recommendation: CAS blob via push_all — consistent with how all other metadata is stored. Serialization: BTreeMap as sorted `(Digest224, u64)` pairs in 36-byte records.

---

## Validation Architecture

### Test Framework

| Property | Value |
|----------|-------|
| Framework | Rust built-in test harness (cargo test) |
| Config file | none — uses `#[test]` attributes |
| Quick run command | `cargo test --package metadata --package slicefs-traits` |
| Full suite command (non-FUSE) | `cargo test --package metadata --package slicefs-traits --package cas-local` |
| FUSE unit tests | `cargo test --package slicefs-cli --features macos-no-mount` |

**Note:** FUSE integration tests (through actual mount) require Linux with libfuse or macOS with macFUSE/FUSE-T. The `fuser` crate's `macos-no-mount` feature allows compiling and unit-testing `SliceFsFilesystem` callbacks without a real mount.

### Phase Requirements → Test Map

| Req ID | Behavior | Test Type | Automated Command | File Exists? |
|--------|----------|-----------|-------------------|-------------|
| POSIX-01 | File create/write/read/delete through meta store | unit | `cargo test --package slicefs-cli write_buffer` | ❌ Wave 0 |
| POSIX-01 | Buffer→push_all→manifest pipeline round-trip | unit | `cargo test --package slicefs-cli flush_pipeline` | ❌ Wave 0 |
| POSIX-02 | mkdir/rmdir via DictMetadataStore | unit | `cargo test --package metadata create_directory` | ✅ exists |
| POSIX-03 | Cross-directory rename removes old entry, adds new | unit | `cargo test --package slicefs-cli rename_cross_dir` | ❌ Wave 0 |
| POSIX-03 | Rename overwrite removes old target entry | unit | `cargo test --package slicefs-cli rename_overwrite` | ❌ Wave 0 |
| POSIX-04 | Symlink target stored as content, readlink returns correct path | unit | `cargo test --package slicefs-cli symlink_round_trip` | ❌ Wave 0 |
| POSIX-05 | Hard link increments nlinks; unlink decrements; delete at 0 | unit | `cargo test --package slicefs-cli hard_link_nlinks` | ❌ Wave 0 |
| POSIX-09 | Truncate shrinks file (setattr size < current) | unit | `cargo test --package slicefs-cli truncate_shrink` | ❌ Wave 0 |
| POSIX-09 | Truncate extends file with zeros (setattr size > current) | unit | `cargo test --package slicefs-cli truncate_extend` | ❌ Wave 0 |
| POSIX-12 | POSIX lock returns ENOSYS — kernel handles locally | unit | `cargo test --package slicefs-cli lock_enosys` | ❌ Wave 0 |
| POSIX-14 | pjdfstest >95% pass rate | manual | Run pjdfstest on Linux (see manual steps) | ❌ manual-only |
| CAS-04 | Refcount increments on manifest set, decrements on delete | unit | `cargo test --package metadata refcount` | ❌ Wave 0 |
| CAS-04 | Two files with same content share one refcount entry | unit | `cargo test --package metadata refcount_shared` | ❌ Wave 0 |
| CAS-06 | statfs logical bytes = sum of inode sizes | unit | `cargo test --package slicefs-cli statfs_logical` | ❌ Wave 0 |
| CAS-06 | statfs physical bytes = dict.len() * 92 | unit | `cargo test --package slicefs-cli statfs_physical` | ❌ Wave 0 |
| CAS-06 | Dedup ratio > 1.0 when writing identical files | unit | `cargo test --package slicefs-cli statfs_dedup_ratio` | ❌ Wave 0 |

**POSIX-14 manual test steps (Linux):**
```bash
# Build SliceFS on Linux
cargo build --release

# Initialize store
./target/release/slicefs seed /tmp/slicefs-store /some/dir

# Mount
./target/release/slicefs mount /mnt/slicefs --store /tmp/slicefs-store

# Run pjdfstest (from pjdfstest repo)
cd /mnt/slicefs
prove -r /path/to/pjdfstest/tests/

# Unmount
./target/release/slicefs unmount /mnt/slicefs
```

### Sampling Rate
- **Per task commit:** `cargo test --package metadata --package slicefs-traits`
- **Per wave merge:** `cargo test --package metadata --package slicefs-traits --package slicefs-cli --features macos-no-mount`
- **Phase gate:** Full non-FUSE suite green + FUSE unit tests green before `/gsd:verify-work`

### Wave 0 Gaps
- [ ] `crates/slicefs-cli/tests/write_path.rs` — covers POSIX-01 write buffer + flush pipeline
- [ ] `crates/slicefs-cli/tests/rename_tests.rs` — covers POSIX-03 cross-dir + overwrite
- [ ] `crates/slicefs-cli/tests/symlink_tests.rs` — covers POSIX-04
- [ ] `crates/slicefs-cli/tests/hard_link_tests.rs` — covers POSIX-05 nlinks
- [ ] `crates/slicefs-cli/tests/truncate_tests.rs` — covers POSIX-09
- [ ] `crates/slicefs-cli/tests/statfs_tests.rs` — covers CAS-06
- [ ] `crates/metadata/tests/refcount_tests.rs` — covers CAS-04
- [ ] Feature flag setup: ensure `macos-no-mount` is properly configured in slicefs-cli/Cargo.toml for unit testing without libfuse

---

## Sources

### Primary (HIGH confidence)
- fuser 0.17.0 source (`~/.cargo/registry/src/.../fuser-0.17.0/src/lib.rs`) — all Filesystem trait method signatures verified directly
- `crates/slicefs-cli/src/filesystem.rs` — existing EROFS stubs confirmed, all callbacks present
- `crates/slicefs-cli/src/seed.rs` — State::push_all pipeline confirmed working
- `crates/metadata/src/store.rs` — all DictMetadataStore mutation methods confirmed
- `crates/metadata/src/inode.rs` — InodeMeta 56-byte serialization confirmed
- `.planning/phases/04-full-posix-write-path/04-CONTEXT.md` — locked decisions

### Secondary (MEDIUM confidence)
- fuser README and documentation — write callback semantics (flush vs release distinction)
- POSIX specification — rename(2), link(2), unlink(2), symlink(2) semantics
- pjdfstest GitHub (https://github.com/pjd/pjdfstest) — test categories and skip patterns

### Tertiary (LOW confidence)
- pjdfstest pass rate estimates for FUSE filesystems — 95%+ is achievable based on community reports; actual number validated only by running tests

---

## Metadata

**Confidence breakdown:**
- Standard stack: HIGH — all libraries already in workspace and proven
- Architecture: HIGH — patterns directly derived from existing working code (seed.rs, filesystem.rs)
- Pitfalls: HIGH — deadlock documented in store.rs comments; MountOption::RO confirmed as blocker
- POSIX-12 (locking): MEDIUM — kernel-handles-ENOSYS behavior is standard FUSE pattern but not verified against pjdfstest behavior

**Research date:** 2026-03-27
**Valid until:** 2026-09-27 (fuser 0.17 is stable; blockset API is internal)
