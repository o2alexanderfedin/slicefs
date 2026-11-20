# Phase 3: Read-Only FUSE - Research

**Researched:** 2026-03-27
**Domain:** FUSE filesystem (fuser 0.17), clap 4 CLI, blockset file storage, read-only POSIX semantics
**Confidence:** HIGH

## Summary

Phase 3 wires three existing pillars into a real mountable filesystem: the `DictMetadataStore` (Phase 2), the blockset `Dictionary` with `GetBytes` traversal (Phase 1), and the `fuser` FUSE crate already declared in the workspace. The new `crates/slicefs-cli/` binary crate implements three subcommands — `mount`, `unmount`, and `seed` — and a `SliceFsFilesystem` struct that implements `fuser::Filesystem`. The seed command imports a directory tree into a `DictMetadataStore`, serializes the `Dictionary` to disk via `FileStorageAdd`, and writes the root `Digest224`. The mount command deserializes that state, starts a blocking `fuser::mount2` session, and commits the store on SIGTERM.

The critical integration challenge is bridging `fuser`'s callback model (per-request `&self` or `&mut self`) to `DictMetadataStore`'s `Mutex<Dictionary>` interior mutability, and implementing partial `read` via `GetBytes` with manual byte-offset seek. A secondary challenge is correctly mapping `MetaError` variants to POSIX errno values so that `ls`, `stat`, and `cat` all behave correctly.

**Primary recommendation:** Implement `SliceFsFilesystem` as a thin adapter that wraps `Arc<DictMetadataStore>` and translates between fuser's `INodeNo`/`FileAttr`/`ReplyData` types and the existing trait layer. Keep the adapter stateless (no file-handle tracking needed for read-only) and delegate all persistence to `serialize_dictionary`/`load_from_root`.

<user_constraints>
## User Constraints (from CONTEXT.md)

### Locked Decisions
- CLI structure: `slicefs mount <mountpoint> --store <path>` — mount point is positional, store path is a flag
- Foreground by default — blocking process, Ctrl+C or SIGTERM to stop. Daemonize can be added later
- Flush and commit on shutdown — even though read-only, commit the current root Digest224 and serialize the Dictionary to disk on SIGTERM/unmount. Prepares for Phase 4 write support
- Both custom and system unmount — `slicefs unmount <mountpoint>` as convenience wrapper, plus standard `umount`/`fusermount -u` also works via FUSE's destroy callback
- CLI seed command: `slicefs seed <store> <source-dir>` — imports a directory tree into the Dictionary store
- Full content import — reads actual file bytes, chunks via data-id's State CDC, stores in Dictionary
- data-id's State CDC for chunking — validates the full CAS pipeline end-to-end with real content
- Directory-based store — the `--store` path is a directory structure (like data-id's file storage layout), not a single binary file
- Partial reads via GetBytes — use data-id's GetBytes iterator, skip to offset, read requested size
- Incremental readdir with offset — use the fuser offset parameter to paginate directory listings
- Standard FUSE option set — noatime, ro (enforce read-only), allow_other, cache_size, plus standard fuser options
- All unsupported operations fail explicitly and loudly
- `clap` crate for CLI argument parsing — derive macros, typed args, auto-generated help
- Final `slicefs` binary from day one — `crates/slicefs-cli/` with mount, unmount, and seed subcommands
- Standard FUSE options passed through to fuser

### Claude's Discretion
- EROFS vs ENOSYS distinction for write ops vs truly unimplemented ops (recommend: EROFS for write operations that are valid but denied in read-only mode, ENOSYS for operations that aren't implemented at all)
- fuser session management and thread model
- Cache implementation details for the cache_size option
- Error type design for FUSE-specific failures
- Exact directory-based store layout format

### Deferred Ideas (OUT OF SCOPE)
- Daemonize mode (`--daemon` flag) — add when production deployment is relevant
- Write operations (create, write, mkdir, unlink, rename) — Phase 4
- JSON output from CLI commands — Phase 7 (CLI-05)
- Stats and scrub commands — Phase 7
</user_constraints>

<phase_requirements>
## Phase Requirements

| ID | Description | Research Support |
|----|-------------|-----------------|
| POSIX-13 | Correct errno values for all operations | MetaError-to-errno mapping table; EROFS vs ENOSYS distinction |
| POSIX-15 | All POSIX operations that FUSE frontend allows on each platform | fuser Filesystem trait; required callbacks for ls/stat/cat; unsupported ops fail loud |
| PLAT-02 | Linux support via libfuse + fuser | fuser 0.17 on Linux; mount2 blocks until unmount; FUSE kernel module required |
| CLI-01 | Mount command with configurable options | clap derive; mount subcommand with positional mountpoint + --store flag |
| CLI-02 | Unmount command with clean shutdown | unmount subcommand wraps fusermount3 -u; destroy callback for system unmount |
| CLI-06 | Mount options for performance tuning (noatime, writeback cache, cache size) | MountOption::NoAtime, MountOption::RO; cache_size as custom --cache-size arg |
| META-02 | Clean mount/unmount with graceful SIGTERM handling and pending write flush | fuser::mount2 catches signals; destroy() triggers serialize_dictionary + write root |
</phase_requirements>

## Standard Stack

### Core
| Library | Version | Purpose | Why Standard |
|---------|---------|---------|--------------|
| fuser | 0.17 | FUSE kernel interface, Filesystem trait, mount2 | Already in workspace; only maintained pure-Rust FUSE crate; wraps libfuse3 |
| clap | 4.6+ | CLI argument parsing with derive macros | Already chosen by user; derive = zero boilerplate, typed subcommands |
| libc | 0.2 | POSIX errno constants (EROFS, ENOSYS, ENOENT, etc.) | Already in workspace; only source for errno integer constants |

### Supporting
| Library | Version | Purpose | When to Use |
|---------|---------|---------|-------------|
| thiserror | 2 | FuseError / CliError typed enums | Consistent with existing crates |
| tracing | 0.1 | Debug logging per FUSE callback | Already in workspace; essential for diagnosing kernel interactions |
| blockset (data-id submodule) | — | FileStorageAdd/file_storage_get for on-disk Dictionary | On-disk store backend |

### Alternatives Considered
| Instead of | Could Use | Tradeoff |
|------------|-----------|----------|
| fuser mount2 (blocking) | spawn_mount2 (background thread) | spawn_mount2 returns immediately but drops unmount the session; mount2 matches foreground-by-default decision |
| libc errno constants | raw i32 literals | libc gives named, self-documenting constants; literals are fragile |

**Installation:**
```bash
# fuser, clap, libc are not yet in workspace members for slicefs-cli
# Add to Cargo.toml workspace.dependencies if not present, then in slicefs-cli/Cargo.toml:
cargo add clap --features derive
# fuser and libc are already workspace deps
```

## Architecture Patterns

### Recommended Project Structure
```
crates/slicefs-cli/
├── Cargo.toml                  # bin crate; deps: fuser, clap, libc, metadata, slicefs-traits, blockset, thiserror, tracing
├── src/
│   ├── main.rs                 # parse Cli, dispatch subcommand
│   ├── cli.rs                  # #[derive(Parser)] Cli + #[derive(Subcommand)] Cmd enum
│   ├── filesystem.rs           # SliceFsFilesystem: fuser::Filesystem impl
│   ├── seed.rs                 # seed subcommand: walk source dir, import into DictMetadataStore
│   ├── mount.rs                # mount subcommand: load store, call mount2
│   ├── unmount.rs              # unmount subcommand: shell out to fusermount3 -u
│   └── store_io.rs             # StoreIo: blockset::Io impl backed by a real directory path
```

### Pattern 1: CLI Structure with clap Derive

**What:** Three subcommands under a single `slicefs` binary using clap's derive macros.
**When to use:** Single binary, typed subcommand dispatch, auto-generated help.

```rust
// Source: https://docs.rs/clap/4.6.0/clap/_derive/_tutorial/
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "slicefs", about = "SliceFS filesystem tool")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Mount the filesystem at <mountpoint>
    Mount {
        /// Mount point directory
        mountpoint: PathBuf,
        /// Path to the backing store directory
        #[arg(long)]
        store: PathBuf,
        /// Disable access time updates
        #[arg(long, default_value_t = true)]
        noatime: bool,
        /// Read cache size in bytes
        #[arg(long, default_value_t = 0)]
        cache_size: usize,
    },
    /// Unmount the filesystem at <mountpoint>
    Unmount {
        mountpoint: PathBuf,
    },
    /// Import a directory tree into a store
    Seed {
        /// Path to store directory (created if absent)
        store: PathBuf,
        /// Source directory to import
        source_dir: PathBuf,
    },
}
```

### Pattern 2: fuser Filesystem Adapter

**What:** `SliceFsFilesystem` holds `Arc<DictMetadataStore>` and a `Dictionary` (for content reads). Implements `fuser::Filesystem`. All write ops return `EROFS`; ops beyond the read-only set return `ENOSYS`.
**When to use:** Wrapping any MetadataStore for FUSE read-only access.

```rust
// Source: https://docs.rs/fuser/0.17.0/fuser/trait.Filesystem.html
use fuser::{Filesystem, Request, ReplyAttr, ReplyEntry, ReplyDirectory, ReplyData, FileAttr, FileType};
use std::ffi::OsStr;
use std::sync::Arc;
use std::time::Duration;
use metadata::store::DictMetadataStore;
use blockset::Dictionary;

pub struct SliceFsFilesystem {
    meta: Arc<DictMetadataStore>,
    dict: Arc<std::sync::Mutex<Dictionary>>,
}

impl SliceFsFilesystem {
    pub fn new(meta: DictMetadataStore, dict: Dictionary) -> Self {
        Self {
            meta: Arc::new(meta),
            dict: Arc::new(std::sync::Mutex::new(dict)),
        }
    }
}
```

Important: `fuser::Filesystem::init` takes `&mut self` — the only `&mut self` method. All other callbacks take `&self`. This is compatible with the existing `Arc<DictMetadataStore>` pattern.

### Pattern 3: InodeMeta to FileAttr Conversion

**What:** Convert `InodeMeta` (from `DictMetadataStore::get_inode`) to `fuser::FileAttr` for `getattr` and `lookup` replies.

```rust
// Source: https://docs.rs/fuser/0.17.0/fuser/struct.FileAttr.html
use fuser::{FileAttr, FileType};
use slicefs_traits::metadata::InodeMeta;
use std::time::{Duration, UNIX_EPOCH};

fn inode_to_file_attr(meta: &InodeMeta) -> FileAttr {
    let file_type = if meta.mode & 0o170000 == 0o040000 {
        FileType::Directory
    } else if meta.mode & 0o170000 == 0o120000 {
        FileType::Symlink
    } else {
        FileType::RegularFile
    };

    FileAttr {
        ino: meta.ino,
        size: meta.size,
        blocks: (meta.size + 511) / 512,
        atime: UNIX_EPOCH,  // noatime: don't track access time
        mtime: UNIX_EPOCH + Duration::new(meta.mtime_sec as u64, meta.mtime_nsec),
        ctime: UNIX_EPOCH + Duration::new(meta.ctime_sec as u64, meta.ctime_nsec),
        crtime: UNIX_EPOCH,  // macOS only
        kind: file_type,
        perm: (meta.mode & 0o7777) as u16,
        nlink: meta.nlinks,
        uid: meta.uid,
        gid: meta.gid,
        rdev: 0,
        blksize: 4096,
        flags: 0,  // macOS only
    }
}
```

### Pattern 4: readdir with Offset

**What:** FUSE `readdir` must skip already-sent entries using the offset parameter. The offset is a 1-based index into the sorted entry list — each `reply.add()` call receives `offset + index + 1` as the next offset.

```rust
// Source: https://github.com/cberner/fuser/blob/master/examples/simple.rs
fn readdir(&self, _req: &Request, ino: u64, _fh: u64, offset: i64, mut reply: ReplyDirectory) {
    let entries = match self.meta.list_directory(ino) {
        Ok(e) => e,
        Err(_) => { reply.error(libc::ENOENT); return; }
    };
    for (index, entry) in entries.iter().enumerate().skip(offset as usize) {
        let child_meta = self.meta.get_inode(entry.ino).unwrap();
        let file_type = inode_to_fuse_file_type(child_meta.mode);
        let buffer_full = reply.add(
            entry.ino,
            (index + 1) as i64,  // next offset = current index + 1
            file_type,
            OsStr::new(&entry.name),
        );
        if buffer_full { break; }
    }
    reply.ok();
}
```

### Pattern 5: read with GetBytes Offset Skip

**What:** Implement `read` by collecting all manifest block digests, constructing `GetBytes` from the Dictionary, skipping `offset` bytes, then collecting up to `size` bytes.

```rust
// Source: crates/metadata/src/manifest.rs, crates/data-id/blockset/src/get_data.rs
fn read(&self, _req: &Request, ino: u64, _fh: u64, offset: i64, size: u32,
        _flags: i32, _lock_owner: Option<u64>, reply: ReplyData) {
    // Get the manifest (list of Digest224 block hashes)
    let manifest = match self.meta.get_manifest(ino) {
        Ok(m) => m,
        Err(_) => { reply.error(libc::ENOENT); return; }
    };
    // Get file content via GetBytes iterator
    let dict = self.dict.lock().unwrap();
    use slicefs_traits::digest::from_digest224;
    use blockset::{GetBytes, GetData};
    // The manifest encodes the full content as a single CAS entry.
    // The manifest_key was stored during seed; retrieve it by re-interning or caching.
    // Implementation: store the manifest_digest (not manifest blocks) directly.
    // Skip offset bytes, take up to size bytes
    let data: Vec<u8> = get_bytes_iter(&dict, &manifest_key)
        .skip(offset as usize)
        .take(size as usize)
        .collect();
    reply.data(&data);
}
```

**Critical implementation note:** The manifest returned by `get_manifest()` is a `Vec<Digest224>` (the ordered chunk list). To reconstruct file content, the implementation must either:
1. Re-intern the manifest bytes and use that `Digest224` as the GetBytes root, OR
2. Store the content root `Digest224` separately (not the chunk list but the CDC root from `State::push_all` on the file content).

**Recommendation:** During `seed`, call `State::push_all(dict, file_bytes)` to get a content `Digest224`, then store that directly as the manifest key via a new `set_content_root(ino, Digest224)` method, OR store it as the single-element manifest `[content_root]`. The simplest approach: treat the manifest as containing exactly one `Digest224` — the CDC root for the full file content. `GetBytes` can then traverse the tree from that root.

### Pattern 6: Mount and Unmount Lifecycle

**What:** `mount2` blocks the calling thread until FUSE session ends. SIGTERM is caught by fuser's session loop and triggers `destroy()`. `fusermount3 -u` also triggers the session end.

```rust
// Source: https://docs.rs/fuser/0.17.0/fuser/fn.mount2.html
use fuser::{mount2, Config, MountOption};

pub fn run_mount(fs: SliceFsFilesystem, mountpoint: &Path, noatime: bool) -> std::io::Result<()> {
    let mut options = vec![
        MountOption::RO,
        MountOption::FSName("slicefs".to_string()),
        MountOption::DefaultPermissions,
    ];
    if noatime {
        options.push(MountOption::NoAtime);
    }
    let config = Config {
        mount_options: options,
        ..Default::default()
    };
    mount2(fs, mountpoint, &config)
    // Returns when unmounted. destroy() has already been called.
}
```

### Pattern 7: Store Persistence on Disk

**What:** The `--store` path is a directory; `FileStorageAdd` writes files under `<store>/vt0/` (top-level entries) and `<store>/vt0./` (internal nodes). The root `Digest224` must be saved separately alongside the dictionary because `FileStorageAdd::end()` only returns the top-level key.

**Store layout:**
```
<store>/
├── vt0/          # top-level CAS blobs (FileStorageAdd TOP_SUFFIX)
├── vt0./         # internal CAS blobs (FileStorageAdd INTERNAL_SUFFIX)
└── root.bin      # 28 bytes: the root Digest224 from commit() — custom, not from blockset
```

**Seed flow:**
1. Walk `source_dir` recursively (breadth-first or depth-first, children before parents)
2. For each file: read bytes, call `State::push_all(&mut dict, bytes)` → content `Digest224`
3. Create inodes via `DictMetadataStore`, store content digest as manifest
4. Create directories via `create_directory`
5. Call `store.commit()` → root `Digest224`
6. Write all dict entries to disk via `FileStorageAdd` (or equivalent `Io` impl backed by the store path)
7. Write root digest to `<store>/root.bin`

**Mount flow:**
1. Read `<store>/root.bin` → root `Digest224`
2. Read dict from `<store>` via `file_storage_get` for all blocks (or implement `FileStorageGet`)
3. Call `DictMetadataStore::load_from_root(dict, &root)` → store
4. Construct `SliceFsFilesystem`, call `mount2`
5. On `destroy()`: call `store.commit()`, write updated root, re-serialize dict

**CRITICAL:** `blockset::file_storage.rs` has `FileStorageAdd` (write) and `file_storage_get` (read single blob). To load the entire dict from disk, the existing code only exposes `file_storage_get` for individual blobs. `DictMetadataStore::load_from_root` takes a `Dictionary` already in memory. This means:
- The dict must be reconstructed in memory from disk files, OR
- A new `StoreIo` adapter must implement `blockset::Io` to provide `read`/`write` operations backed by the filesystem directory.

Looking at `app.rs` (data-id's existing CLI), it uses `serialize`/`deserialize` on a single `dictionary.bin` file for the entire Dictionary. The `FileStorageAdd` approach writes individual blocks as files. For Phase 3, the simplest approach matching the "directory-based store" decision is to use `FileStorageAdd` for writing during seed, and implement a corresponding load that deserializes the entire dict from those individual files.

**Alternate simpler approach:** Use `serialize_dictionary` / `deserialize_dictionary` from `crates/metadata/src/store.rs` to save the entire Dictionary as one `<store>/dictionary.bin`, plus `<store>/root.bin`. This is simpler than per-file storage and already implemented. The "directory-based store" decision means the store path is a directory (not a single flat file), which is satisfied by having both files inside a directory.

### Anti-Patterns to Avoid

- **Returning 0 or success for write ops in read-only mode:** Write operations (`write`, `create`, `mkdir`, `mknod`, `symlink`, `link`, `unlink`, `rmdir`, `rename`, `setattr`) MUST return `EROFS` (read-only filesystem), not `ENOSYS`. ENOSYS means "not implemented"; EROFS means "filesystem is read-only" — `ls` and shells check errno to distinguish these.
- **Returning ENOSYS for truly POSIX-required reads:** `getattr`, `lookup`, `readdir`, `read`, `open`, `release`, `opendir`, `releasedir`, `statfs`, `access` MUST be implemented — returning ENOSYS for these causes `ls` and `cat` to fail.
- **Collecting all GetBytes at once for large files:** The `GetBytes` iterator works byte-by-byte. For large files, prefer skipping and taking as an iterator rather than collecting all bytes, then slicing.
- **Acquiring Dict mutex inside a Mutex<DictMetadataStore> lock:** Would deadlock with `DictMetadataStore`'s own internal `dict` lock. Separate the `dict` used for content reads (seed-time copy) from the one inside `DictMetadataStore`.

## Don't Hand-Roll

| Problem | Don't Build | Use Instead | Why |
|---------|-------------|-------------|-----|
| FUSE protocol framing | Custom kernel↔userspace IPC | fuser 0.17 | Kernel ABI, protocol versions, buffer management — extremely complex |
| Argument parsing | Custom argv parser | clap 4 derive | Error messages, help generation, type coercion, subcommand routing |
| POSIX errno constants | Raw integer literals | libc crate | Platform-specific values; libc guarantees correct values per target |
| File content reconstruction | Custom CAS traversal | blockset GetBytes + GetData | Already handles the Merkle tree traversal; GetBytes is an Iterator |
| Dictionary serialization | Custom byte encoding | serialize_dictionary/deserialize_dictionary | Already implemented and tested in metadata crate |
| State persistence | Custom format | commit() + load_from_root() | 156-byte root record fully tested in Phase 2 |

**Key insight:** The entire FUSE layer is a thin adapter. Nothing new should be built for storage, hashing, or serialization — those exist. The work is wiring and translation.

## Common Pitfalls

### Pitfall 1: EROFS vs ENOSYS Confusion
**What goes wrong:** Returning `ENOSYS` for write operations (write, create, mkdir, etc.) makes the kernel think the operation is "not implemented" — some tools retry with different syscalls or give confusing error messages. `EROFS` is the correct code for "this filesystem is read-only."
**Why it happens:** Default fuser implementations return `ENOSYS`; developers forget to override write ops in a read-only filesystem.
**How to avoid:** Explicitly implement all write operations (`write`, `create`, `mkdir`, `mknod`, `symlink`, `link`, `unlink`, `rmdir`, `rename`, `setattr`, `fallocate`) with `reply.error(libc::EROFS)`.
**Warning signs:** `touch` on a mounted file gives "Function not implemented" instead of "Read-only file system."

### Pitfall 2: getattr Returns Wrong Inode 1 Attributes
**What goes wrong:** FUSE requires inode 1 to be a directory. If `getattr` for ino=1 returns a regular file or returns ENOENT, the mount point won't show contents under `ls`.
**Why it happens:** The `DictMetadataStore` already creates inode 1 as a directory — the pitfall is a bug in the ino=1 lookup path or mode translation.
**How to avoid:** Add an explicit unit test: mount a fresh store, call `getattr(1)`, assert `FileType::Directory`.

### Pitfall 3: readdir Missing . and ..
**What goes wrong:** `ls -la` shows wrong link counts; some tools fail if `.` and `..` are absent from readdir output.
**Why it happens:** `DictMetadataStore::list_directory` returns all entries including `.` and `..` — the pitfall is filtering them out or forgetting to pass correct inos for dot entries.
**How to avoid:** Verify that `.` has the directory's own ino, `..` has the parent's ino. `list_directory` already includes these.

### Pitfall 4: Dictionary Split Between seed and mount
**What goes wrong:** Content blobs written during `seed` aren't visible during `mount` because they're in separate `Dictionary` instances not merged on disk.
**Why it happens:** `State::push_all` stores content in the in-memory Dictionary. If the Dictionary is serialized separately from the metadata Dictionary, the two must be written together (or as one).
**How to avoid:** Use a single `Dictionary` for both metadata (via `DictMetadataStore`) and content (via `State::push_all`). The `DictMetadataStore` exposes its internal dictionary via `commit()` — but the content blobs must be pushed into the same `Dictionary` before committing. Design: pass `&mut Dictionary` to seed, use it for both content and metadata, then persist the unified Dictionary.

### Pitfall 5: fuser::Filesystem::init is &mut self
**What goes wrong:** `init` is the only `Filesystem` method that takes `&mut self`. This means `SliceFsFilesystem` cannot be wrapped in `Arc<Mutex<...>>` before passing to `mount2` if `init` needs to mutate state.
**Why it happens:** `mount2` takes ownership of the `Filesystem` impl; `init` can mutate it before the session starts. All other methods take `&self`.
**How to avoid:** Accept `&mut self` in `init`, configure `KernelConfig` (e.g., `set_max_readahead(128 * 1024)`), and ensure all runtime mutable state is behind `Mutex` or `Arc` inside the struct.

### Pitfall 6: Mount Hangs on SIGTERM Without Signal Handler
**What goes wrong:** `fuser::mount2` does handle SIGTERM by ending the session — but only if the process receives the signal cleanly. Some process managers send SIGKILL if SIGTERM isn't handled promptly.
**Why it happens:** `destroy()` is the hook; it's called synchronously when the FUSE session ends. The `mount2` call unblocks after `destroy()` returns.
**How to avoid:** Keep `destroy()` fast — write root + serialize dict, no blocking I/O beyond that. The entire dict will be at most a few MB for Phase 3 test content.

### Pitfall 7: GetBytes Byte-by-Byte Iterator is Slow for Large Offsets
**What goes wrong:** `cat` on a large file does many small reads at increasing offsets. Each `read` call recreates `GetBytes` from the root and skips `offset` bytes one at a time — O(offset) per read call.
**Why it happens:** `GetBytes` is a forward-only iterator. There's no seek operation.
**How to avoid:** For Phase 3 (read-only, test content only), collect the full file content once per `open` and cache it in an `OpenFileHandle` map keyed by file handle. Return slices on subsequent `read` calls. Track open files with a `Mutex<HashMap<u64, Vec<u8>>>`. This trades memory for speed and is acceptable for Phase 3 validation.

## Code Examples

Verified patterns from the existing codebase and fuser docs:

### Mount2 Blocking Call
```rust
// Source: https://docs.rs/fuser/0.17.0/fuser/fn.mount2.html
use fuser::{mount2, Config, MountOption};

let config = Config {
    mount_options: vec![
        MountOption::RO,
        MountOption::NoAtime,
        MountOption::FSName("slicefs".to_string()),
        MountOption::DefaultPermissions,
    ],
    ..Default::default()
};
// Blocks until unmount (Ctrl+C, SIGTERM, or fusermount3 -u)
mount2(filesystem, mountpoint, &config)?;
```

### All Write Operations Return EROFS
```rust
// Source: libc crate + fuser Filesystem trait
fn write(&mut self, _req: &Request, _ino: u64, _fh: u64, _offset: i64,
         _data: &[u8], _write_flags: u32, _flags: i32, _lock_owner: Option<u64>,
         reply: ReplyWrite) {
    reply.error(libc::EROFS);
}
fn create(&mut self, _req: &Request, _parent: u64, _name: &OsStr,
          _mode: u32, _umask: u32, _flags: i32, reply: ReplyCreate) {
    reply.error(libc::EROFS);
}
fn mkdir(&mut self, _req: &Request, _parent: u64, _name: &OsStr,
         _mode: u32, _umask: u32, reply: ReplyEntry) {
    reply.error(libc::EROFS);
}
// ... same pattern for: mknod, symlink, link, unlink, rmdir, rename, setattr, fallocate
```

### MetaError to errno Mapping
```rust
// Recommendation: EROFS vs ENOSYS distinction
fn meta_error_to_errno(e: &MetaError) -> i32 {
    match e {
        MetaError::NotFound(_)       => libc::ENOENT,
        MetaError::AlreadyExists(_)  => libc::EEXIST,
        MetaError::NotADirectory(_)  => libc::ENOTDIR,
        MetaError::IsADirectory(_)   => libc::EISDIR,
        MetaError::NotEmpty(_)       => libc::ENOTEMPTY,
        MetaError::InvalidName(_)    => libc::EINVAL,
        MetaError::Corrupted(_)      => libc::EIO,
        MetaError::Io(_)             => libc::EIO,
    }
}
```

### Seed: Import File Content via State::push_all
```rust
// Source: crates/data-id/blockset/src/app.rs lines 30-33
use blockset::{State, Tree, Dictionary};  // Tree must be in scope for push_all
let content = std::fs::read(&file_path)?;
let content_digest: Digest224 = State::push_all(&mut dict, &content);
// Store content_digest as the file's manifest key
meta_store.set_manifest(ino, &[content_digest])?;
// Note: push_all requires Tree to be imported (blockset::Tree must be in scope)
```

### clap Subcommand Structure
```rust
// Source: https://docs.rs/clap/4.6.0/clap/_derive/
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "slicefs", version, about = "SliceFS filesystem tool")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Subcommand)]
pub enum Cmd {
    Mount { mountpoint: PathBuf, #[arg(long)] store: PathBuf, #[arg(long)] noatime: bool },
    Unmount { mountpoint: PathBuf },
    Seed { store: PathBuf, source_dir: PathBuf },
}
```

## State of the Art

| Old Approach | Current Approach | When Changed | Impact |
|--------------|------------------|--------------|--------|
| fuse-rs (zargony) | fuser (cberner fork) | ~2020 | fuser is maintained; fuse-rs is unmaintained |
| mount() | mount2() with Config | fuser 0.14+ | mount2 is the stable API; mount() being deprecated |
| Manual arg parsing | clap 4 derive | clap 4.0 (2022) | Derive macros eliminate boilerplate; builder API still works |

**Deprecated/outdated:**
- `fuser::mount()`: Being replaced by `mount2()` with `Config`; prefer `mount2` for new code.
- `fuse-rs` crate: Unmaintained since ~2020; `fuser` is the maintained fork.

## Open Questions

1. **Content root vs chunk list in manifest**
   - What we know: `get_manifest()` returns `Vec<Digest224>` (chunk hashes). `GetBytes` needs a single root `Digest224` to traverse the tree.
   - What's unclear: Should we store the CDC tree root (from `State::push_all(file_bytes)`) as a single-element manifest, or add a separate `content_root` concept?
   - Recommendation: Store the CDC root as a single-element manifest `[content_root_digest]`. During `read`, get `manifest[0]` as the root for `GetData`/`GetBytes`. This matches the existing `set_manifest`/`get_manifest` interface without any API changes.

2. **Dictionary loading from disk for mount**
   - What we know: `FileStorageAdd` writes individual blob files; `serialize_dictionary` writes a flat binary. `load_from_root` takes an in-memory `Dictionary`.
   - What's unclear: How to reconstruct the full in-memory `Dictionary` from whatever is written during seed.
   - Recommendation: Use `serialize_dictionary`/`deserialize_dictionary` to save/load the entire Dictionary as a single `<store>/dictionary.bin`. Simpler than reconstructing from per-blob files. The "directory-based store" requirement is met by using a directory path with this file inside it.

3. **fuser::Filesystem method signatures may differ from docs**
   - What we know: Docs show `ino: INodeNo`, but the trait may use `u64` directly in the impl.
   - What's unclear: Exact parameter types in fuser 0.17 stable API.
   - Recommendation: Verify by running `cargo doc --open` for the fuser crate in the workspace after adding it to slicefs-cli.

## Validation Architecture

### Test Framework
| Property | Value |
|----------|-------|
| Framework | Rust built-in `#[test]` / `cargo test` |
| Config file | None (standard cargo test) |
| Quick run command | `cargo test -p slicefs-cli` |
| Full suite command | `cargo test --workspace` |

### Phase Requirements -> Test Map

| Req ID | Behavior | Test Type | Automated Command | File Exists? |
|--------|----------|-----------|-------------------|-------------|
| POSIX-13 | EROFS returned for write ops (write, create, mkdir) | unit | `cargo test -p slicefs-cli test_write_ops_return_erofs` | Wave 0 |
| POSIX-13 | ENOENT returned for lookup of nonexistent entry | unit | `cargo test -p slicefs-cli test_lookup_missing_returns_enoent` | Wave 0 |
| POSIX-15 | getattr returns correct FileAttr for files and dirs | unit | `cargo test -p slicefs-cli test_getattr_file_and_dir` | Wave 0 |
| POSIX-15 | readdir returns . and .. and seeded entries | unit | `cargo test -p slicefs-cli test_readdir_includes_dot_entries` | Wave 0 |
| POSIX-15 | read returns correct bytes at offset | unit | `cargo test -p slicefs-cli test_read_at_offset` | Wave 0 |
| PLAT-02 | Filesystem mounts on Linux and ls sees seeded files | integration/manual | `cargo run --bin slicefs -- mount /tmp/testmnt --store /tmp/store` | Wave 0 |
| CLI-01 | Mount subcommand accepts mountpoint + --store flag | unit | `cargo test -p slicefs-cli test_cli_mount_args` | Wave 0 |
| CLI-02 | Unmount subcommand accepted by clap | unit | `cargo test -p slicefs-cli test_cli_unmount_args` | Wave 0 |
| CLI-06 | --noatime flag accepted and applied | unit | `cargo test -p slicefs-cli test_cli_noatime_flag` | Wave 0 |
| META-02 | destroy() is called on session end, store is committed | unit | `cargo test -p slicefs-cli test_destroy_commits_store` | Wave 0 |

**Note:** PLAT-02 requires a real Linux FUSE mount — it is manual-only for automated CI because it requires root or user_allow_other, and a real kernel FUSE device.

### Sampling Rate
- **Per task commit:** `cargo test -p slicefs-cli`
- **Per wave merge:** `cargo test --workspace`
- **Phase gate:** Full suite green before `/gsd:verify-work`

### Wave 0 Gaps
- [ ] `crates/slicefs-cli/src/` — entire crate does not exist yet
- [ ] `crates/slicefs-cli/Cargo.toml` — bin crate with fuser, clap, libc, metadata, blockset deps
- [ ] `crates/slicefs-cli/src/main.rs` — entry point
- [ ] `crates/slicefs-cli/src/filesystem.rs` — SliceFsFilesystem with unit tests
- [ ] Add `crates/slicefs-cli` to workspace `members` in root `Cargo.toml`
- [ ] Add `clap` to workspace `[workspace.dependencies]` with `features = ["derive"]`

## Sources

### Primary (HIGH confidence)
- `https://docs.rs/fuser/0.17.0/fuser/` — Filesystem trait, mount2, spawn_mount2, Config, MountOption, FileAttr, KernelConfig
- `https://docs.rs/fuser/0.17.0/fuser/fn.mount2.html` — mount2 signature and blocking semantics
- `https://docs.rs/fuser/0.17.0/fuser/fn.spawn_mount2.html` — BackgroundSession lifecycle
- `https://docs.rs/fuser/0.17.0/fuser/struct.Config.html` — Config fields: mount_options, acl, n_threads, clone_fd
- `https://docs.rs/fuser/0.17.0/fuser/enum.MountOption.html` — All 18 MountOption variants
- `https://docs.rs/fuser/0.17.0/fuser/struct.KernelConfig.html` — set_max_write, set_max_readahead, set_time_granularity
- `https://docs.rs/clap/4.6.0/clap/` — Derive macros, Parser, Subcommand, ValueEnum
- `https://github.com/cberner/fuser/blob/master/examples/simple.rs` — readdir with offset, read implementation
- `crates/data-id/blockset/src/get_data.rs` — GetBytes/GetData implementation (read directly)
- `crates/data-id/blockset/src/file_storage.rs` — FileStorageAdd, file_storage_get, vt0/ layout
- `crates/metadata/src/store.rs` — commit(), load_from_root(), serialize_dictionary(), locking order
- `crates/metadata/src/manifest.rs` — intern_manifest(), load_manifest() pattern
- `crates/slicefs-traits/src/metadata.rs` — MetaError variants, InodeMeta fields

### Secondary (MEDIUM confidence)
- `https://docs.rs/libc/0.2.183/libc/` — EROFS, ENOSYS, ENOENT, ENOTDIR, EISDIR, EACCES, EPERM constants confirmed present

### Tertiary (LOW confidence)
- fuser simple.rs example patterns — verified via WebFetch but full source not read line-by-line

## Metadata

**Confidence breakdown:**
- Standard stack: HIGH — all libs are in workspace.dependencies; versions confirmed
- Architecture: HIGH — fuser API confirmed via docs.rs; existing code read directly
- Pitfalls: HIGH — derived from direct API inspection and known POSIX semantics
- Content reconstruction: MEDIUM — the single-element manifest approach is a design decision, not a confirmed fact about existing code; needs validation against store.rs internals

**Research date:** 2026-03-27
**Valid until:** 2026-04-27 (fuser 0.17 is stable; clap 4 is stable)
