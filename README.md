# SliceFS

**A content-addressed, deduplicating POSIX filesystem in Rust — mountable via FUSE on macOS and Linux.**

SliceFS splits files into variable-sized content-addressed blocks, stores each unique block once, and presents the result as a full POSIX filesystem. Dedup is transparent: it's a daily-driver filesystem, not a backup format.

- **Content-addressable storage** built on the [`blockset`](https://github.com/o2alexanderfedin/data-id) engine: content-defined chunking (boundaries come from the data, so a byte inserted or removed only changes the chunks near it) and a SHA-224 Merkle tree. Hashers, chunkers, block stores, and dedup indexes are defined as traits for future backends.
- **Full POSIX semantics** via [`fuser`](https://crates.io/crates/fuser) — FUSE-T on macOS, libfuse on Linux.
- **Crash-safe**: WAL, refcounts, scrub, and GC (segment compaction; orphan-block cleanup is not implemented yet).
- **Snapshots**: point-in-time create / list / switch.
- **No write-path compression** since v2.0: blocks are stored raw, so dedup works on the original bytes. The `slicefs-compression` crate (Zstd, LZ4, None) is kept for planned segment-level compression (v2.1).

## Status

- **v1.0 shipped** — all POSIX semantics, mount/unmount/seed/gc/snapshot/stats/scrub. Linux CI runs `cargo test` and [pjdfstest](https://github.com/pjd/pjdfstest) (no macOS CI job yet).
- **v2.0 complete** — streaming writes via the incremental push API, removal of write-path compression (dedup on raw content), refcount-overflow fix, realistic `statfs`, O(1) snapshot lookup.
- **1287 tests** passing workspace-wide (last full run: 2026-04-03, end of Phase 12).

Planning artifacts live in `.planning/` (PROJECT.md, ROADMAP.md, phase plans).

## Platforms

| Platform | Status | FUSE backend |
|----------|--------|--------------|
| macOS (Apple Silicon / Intel) | Supported | [FUSE-T](https://www.fuse-t.org/) 1.2.x (NFS backend by default; `--backend smb` also works; FSKit untested) |
| Linux | Supported | libfuse (`fuse`, `libfuse-dev`) |
| Windows | Deferred (v2.x+) | — |

Windows is deferred because `winfsp-rs` is GPL-3; native Projected FS or Dokan are the v2.x candidates.

## Repo layout

```
.
├── crates/
│   ├── slicefs-traits/        # Trait contracts (metadata, ContentHasher, Chunker, BlockStore, DedupIndex) + Digest224
│   ├── cas-local/             # Reference/test impls of the CAS traits (Blake3 hasher, fixed chunker, stores) — not used by the binary
│   ├── slicefs-compression/   # Compressors (Zstd, LZ4, None) — not on the write path since v2.0
│   ├── metadata/              # Inodes, directories, manifests, xattrs, WAL, segments, snapshots, GC
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

**macOS** — install `pkg-config` and [FUSE-T](https://www.fuse-t.org/) 1.2.x (tested with 1.2.9):

```sh
brew install pkg-config
brew install macos-fuse-t/cask/fuse-t
```

The build finds FUSE-T through the bundled `.pkgconfig/fuse.pc` shim (set in `.cargo/config.toml`); no extra environment is needed.

SliceFS builds against a patched copy of `fuser` in `vendor/fuser` (see `vendor/fuser/PATCHES.md`). Stock `fuser` 0.17/0.18 cannot mount on FUSE-T 1.2.x, and with older FUSE-T, `cp` into the mount hangs.

**Linux** — install libfuse:

```sh
sudo apt-get install libfuse-dev fuse
sudo modprobe fuse
```

## Quick start

```sh
# Create a store and seed it from an existing directory
slicefs seed ./my-store ~/Documents

# Mount the store
slicefs mount --store ./my-store /tmp/sfs   # creates /tmp/sfs if missing

# Use it like any filesystem
ls /tmp/sfs
cp file.bin /tmp/sfs/

# Inspect
slicefs stats ./my-store
slicefs snapshot list ./my-store

# Unmount
slicefs unmount /tmp/sfs
```

Run `slicefs --help` (and `slicefs <subcommand> --help`) for the full CLI.

## Test

```sh
cargo test --workspace
```

Linux users can also run the POSIX conformance suite (pjdfstest) — see `.github/workflows/ci.yml` for the exact recipe.

## Architecture (one paragraph)

The `slicefs` binary is built from `slicefs-traits`, `metadata`, and the `blockset` engine (the `data-id` submodule). The FUSE layer (`fuser`) maps VFS operations onto `DictMetadataStore`, which keeps inodes, directories, file manifests, xattrs, and refcounts in memory and persists them through blockset file storage (`StoreIo` is the blockset I/O adapter rooted at the store directory). On write, file bytes are pushed into blockset (`State::push_all`), which splits them into content-defined chunks and builds a SHA-224 Merkle tree; identical chunks are stored once. A file manifest is an ordered list of 28-byte `Digest224` hashes. Each commit appends a `RootUpdate` to the WAL; WAL entries and snapshot records live in append-only segment files that GC compacts. On read, the digests are resolved through blockset and the raw bytes are returned. A snapshot is a named, immutable pointer to a committed root `Digest224`.

For deep detail, see `.planning/PROJECT.md` and the per-phase artifacts under `.planning/phases/`.

## License

SliceFS is **dual-licensed**:

- **AGPL-3.0-or-later** for open-source and internal use — see [LICENSE](./LICENSE).
- **Commercial license** for organizations that need to avoid AGPL's source-disclosure obligations (e.g. embedding in closed-source products or offering a proprietary hosted service) — see [COMMERCIAL-LICENSE.md](./COMMERCIAL-LICENSE.md).

The upstream `blockset` crate (`crates/data-id/blockset`) is GPL-3.0-or-later; AGPL-3.0-or-later is compatible with it via GPLv3 §13.

## Contributors

See [CONTRIBUTORS.md](./CONTRIBUTORS.md).
