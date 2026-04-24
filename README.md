# SliceFS

**A content-addressed, deduplicating POSIX filesystem in Rust — mountable via FUSE on macOS and Linux.**

SliceFS splits files into variable-sized content-addressed blocks, stores each unique block once, and presents the result as a full POSIX filesystem. Dedup is transparent: it's a daily-driver filesystem, not a backup format.

- **Content-addressable storage** with pluggable hashers (Blake3 default), chunkers, block stores, and dedup indexes.
- **Full POSIX semantics** via [`fuser`](https://crates.io/crates/fuser) — FUSE-T on macOS, libfuse on Linux.
- **Crash-safe**: WAL, GC, refcounts, and scrub.
- **Snapshots**: point-in-time create / list / switch.
- **Pluggable compression**: Zstd, LZ4, or None (see v2.0 notes below).

## Status

- **v1.0 shipped** — all POSIX semantics, mount/unmount/seed/gc/snapshot/stats/scrub, Linux + macOS CI with [pjdfstest](https://github.com/pjd/pjdfstest).
- **v2.0 in progress** — streaming writes via incremental push API, removal of write-path compression to allow cross-compressor dedup, refcount-overflow fix, realistic `statfs`, O(1) snapshot lookup.
- **1287 tests** passing workspace-wide.

Planning artifacts live in `.planning/` (PROJECT.md, ROADMAP.md, phase plans).

## Platforms

| Platform | Status | FUSE backend |
|----------|--------|--------------|
| macOS (Apple Silicon / Intel) | Supported | [FUSE-T](https://www.fuse-t.org/) |
| Linux | Supported | libfuse (`fuse`, `libfuse-dev`) |
| Windows | Deferred (v2.x+) | — |

Windows is deferred because `winfsp-rs` is GPL-3; native Projected FS or Dokan are the v2.x candidates.

## Repo layout

```
.
├── crates/
│   ├── slicefs-traits/        # CAS trait contracts (ContentHasher, Chunker, BlockStore, DedupIndex)
│   ├── cas-local/             # Local/test impls: Blake3 hasher, fixed chunker, disk + in-memory stores
│   ├── slicefs-compression/   # Pluggable compressors: Zstd, LZ4, None
│   ├── metadata/              # Inode / directory / xattr metadata layer
│   ├── slicefs-cli/           # `slicefs` binary — mount, unmount, seed, gc, snapshot, stats, scrub
│   └── data-id/               # [git submodule] blockset — upstream Merkle-CAS engine (Sergey Shandar)
├── benchmarks/
├── .github/workflows/ci.yml   # Linux CI: cargo test + pjdfstest
└── .planning/                 # Design docs, roadmap, phase artifacts
```

## Build

Requires Rust **1.95+** (edition 2024).

```sh
git clone --recurse-submodules https://github.com/o2alexanderfedin/slicefs.git
cd slicefs
cargo build --release
```

If you already cloned without submodules:

```sh
git submodule update --init --recursive
```

### Platform prerequisites

**macOS** — install [FUSE-T](https://www.fuse-t.org/):

```sh
brew install macos-fuse-t/cask/fuse-t
```

**Linux** — install libfuse:

```sh
sudo apt-get install libfuse-dev fuse
sudo modprobe fuse
```

## Quick start

```sh
# Create a store and seed it from an existing directory
slicefs seed --store ./my-store --source-dir ~/Documents

# Mount the store
mkdir /tmp/sfs
slicefs mount --store ./my-store --mountpoint /tmp/sfs

# Use it like any filesystem
ls /tmp/sfs
cp file.bin /tmp/sfs/

# Inspect
slicefs stats --store ./my-store
slicefs snapshot list --store ./my-store

# Unmount
slicefs unmount --mountpoint /tmp/sfs
```

Run `slicefs --help` (and `slicefs <subcommand> --help`) for the full CLI.

## Test

```sh
cargo test --workspace
```

Linux users can also run the POSIX conformance suite (pjdfstest) — see `.github/workflows/ci.yml` for the exact recipe.

## Architecture (one paragraph)

The CLI opens a `StoreIo` handle, which composes a `BlockStore` (default: on-disk), a `ContentHasher` (default: Blake3), a `Chunker` (default: fixed-size), a `DedupIndex`, and an optional `Compressor`. The FUSE layer (`fuser`) translates VFS operations into reads/writes against a metadata tree held in `redb`, whose leaves are content IDs. On write, chunks are hashed, looked up in the dedup index, compressed (v1.x — being removed in v2.0), and persisted if novel. On read, CIDs are fetched, decompressed, and returned. Snapshots are copy-on-write references to metadata roots.

For deep detail, see `.planning/PROJECT.md` and the per-phase artifacts under `.planning/phases/`.

## License

Not yet specified for this repository. The upstream `blockset` crate (submodule at `crates/data-id/blockset`) is **GPL-3.0-or-later**; any license chosen here must be compatible with that.

## Contributors

See [CONTRIBUTORS.md](./CONTRIBUTORS.md).
