# Phase 3: Read-Only FUSE - Context

**Gathered:** 2026-03-28
**Status:** Ready for planning

<domain>
## Phase Boundary

Mount a real filesystem read-only; a human can `ls`, `cat`, and `stat` files through the mount point using pre-populated content. Validates the kernel interface before write complexity is introduced. Includes the `slicefs` CLI binary with mount, unmount, and seed subcommands.

Requirements: POSIX-13, POSIX-15, PLAT-02, CLI-01, CLI-02, CLI-06, META-02

</domain>

<decisions>
## Implementation Decisions

### Mount/unmount lifecycle
- CLI structure: `slicefs mount <mountpoint> --store <path>` — mount point is positional, store path is a flag
- Foreground by default — blocking process, Ctrl+C or SIGTERM to stop. Daemonize can be added later
- Flush and commit on shutdown — even though read-only, commit the current root Digest224 and serialize the Dictionary to disk on SIGTERM/unmount. Prepares for Phase 4 write support
- Both custom and system unmount — `slicefs unmount <mountpoint>` as convenience wrapper, plus standard `umount`/`fusermount -u` also works via FUSE's destroy callback

### Pre-seeding content
- CLI seed command: `slicefs seed <store> <source-dir>` — imports a directory tree into the Dictionary store
- Full content import — reads actual file bytes, chunks via data-id's State CDC (content-dependent tree), stores in Dictionary. `cat` through the mount returns real file content
- data-id's State CDC for chunking — validates the full CAS pipeline end-to-end with real content
- Directory-based store — the `--store` path is a directory structure (like data-id's file storage layout), not a single binary file

### FUSE callback mapping
- Partial reads via GetBytes — use data-id's GetBytes iterator, skip to offset, read requested size. Efficient for large files
- Incremental readdir with offset — use the fuser offset parameter to paginate directory listings. Handles arbitrarily large directories
- Standard FUSE option set — noatime, ro (enforce read-only), allow_other, cache_size, plus standard fuser options
- All unsupported operations fail explicitly and loudly — operations that don't make sense in read-only mode must fail clearly, not silently

### CLI design
- `clap` crate for CLI argument parsing — derive macros, typed args, auto-generated help
- Final `slicefs` binary from day one — `crates/slicefs-cli/` with mount, unmount, and seed subcommands. This IS the production binary
- Standard FUSE options passed through to fuser

### Claude's Discretion
- EROFS vs ENOSYS distinction for write ops vs truly unimplemented ops (recommend: EROFS for write operations that are valid but denied in read-only mode, ENOSYS for operations that aren't implemented at all)
- fuser session management and thread model
- Cache implementation details for the cache_size option
- Error type design for FUSE-specific failures
- Exact directory-based store layout format

</decisions>

<code_context>
## Existing Code Insights

### Reusable Assets
- `DictMetadataStore` — full inode CRUD, directory ops, manifest storage, xattr, persistence round-trip
- `InodeMeta` with 56-byte binary serialization
- `InodeMap` for stable inode number allocation
- data-id `State` CDC for content chunking
- data-id `GetBytes`/`GetData` for tree traversal (reading file content back)
- `serialize_dictionary`/`deserialize_dictionary` for Dictionary persistence
- `commit()`/`load_from_root()` for full state persistence cycle

### Established Patterns
- `&self` + `Mutex<Dictionary>` for thread-safe access from fuser callbacks
- data-id types (Digest224/Digest256) throughout the stack
- `MetaError` for metadata-specific failures
- `slicefs-traits` crate for trait definitions

### Integration Points
- New `crates/slicefs-cli/` binary crate — depends on metadata, slicefs-traits, blockset, fuser, clap
- fuser's `Filesystem` trait implementation wraps `DictMetadataStore`
- CLI subcommands (mount, unmount, seed) in separate modules
- `slicefs seed` writes Dictionary to disk via directory-based store
- `slicefs mount` loads Dictionary from disk, creates DictMetadataStore, starts fuser session

</code_context>

<specifics>
## Specific Ideas

- The seed command validates the entire CAS pipeline end-to-end: read files → CDC chunking via data-id State → store in Dictionary → serialize to disk
- The mount command validates the reverse: load Dictionary → reconstruct metadata → serve via FUSE → kernel reads files correctly
- This is the first time SliceFS becomes a real mountable filesystem — the "hello world" moment
- Phase 4 will add write operations on top of the same fuser Filesystem impl — the read-only impl is the foundation

</specifics>

<deferred>
## Deferred Ideas

- Daemonize mode (`--daemon` flag) — add when production deployment is relevant
- Write operations (create, write, mkdir, unlink, rename) — Phase 4
- JSON output from CLI commands — Phase 7 (CLI-05)
- Stats and scrub commands — Phase 7

</deferred>

---

*Phase: 03-read-only-fuse*
*Context gathered: 2026-03-28*
