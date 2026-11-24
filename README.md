# SliceFS

**A content-addressed, deduplicating POSIX filesystem in Rust — mountable via FUSE on macOS and Linux.**

SliceFS splits files into variable-sized content-addressed blocks, stores each unique block once, and presents the result as a full POSIX filesystem. Dedup is transparent: it's a daily-driver filesystem, not a backup format.

- **Content-addressable storage** in the `chunk-store` crate: FastCDC content-defined chunking (average 8 KiB; boundaries come from the data, so a byte inserted or removed only changes the chunks near it), self-describing object IDs (a hash tag + the full 256-bit digest, BLAKE3 by default), and append-only pack files. File content and metadata nodes are both stored this way (`docs/design/chunk-store.md`).
- **Full POSIX semantics** via [`fuser`](https://crates.io/crates/fuser) — FUSE-T on macOS, libfuse on Linux.
- **Crash-safe**: write log with crash replay, scrub, and online and offline GC (it deletes, compacts, or punches holes in pack files).
- **Snapshots**: point-in-time create / list / switch.
- **Compression**: zstd per object (dedup works on the original bytes, before compression). `slicefs mount --compression zstd[:LEVEL]|none` sets it for the new data of a mount. 8 Boost releases (1.80 to 1.87, as tars) in one store: 6,222 MiB of data in 891 MiB (6.99x).
- **Hash agility**: an object ID names its hash (a tag byte, then a 32-byte digest). `slicefs mount --hash NAME` (or `seed --hash`) changes the hash of new objects and records it in the store; objects of the old hash stay readable and verifiable.

## Status

- **v1.0 shipped** — all POSIX semantics, mount/unmount/seed/gc/snapshot/stats/scrub. CI runs `cargo test` and [pjdfstest](https://github.com/pjd/pjdfstest) on Linux, and `cargo test` on macOS.
- **v2.0 complete** — streaming writes via the incremental push API, removal of write-path compression (dedup on raw content), refcount-overflow fix, realistic `statfs`, O(1) snapshot lookup.
- **1287 tests** passing workspace-wide (last full run: 2026-04-03, end of Phase 12).

Planning artifacts live in `.planning/` (PROJECT.md, ROADMAP.md, phase plans).

## Platforms

| Platform | Status | FUSE backend |
|----------|--------|--------------|
| macOS (Apple Silicon / Intel) | Supported | [FUSE-T](https://www.fuse-t.org/) 1.2.x with its FSKit backend (macOS 26+; enable FUSE-T in System Settings > General > Login Items & Extensions > File System Extensions) |
| Linux | Supported | libfuse (`fuse`, `libfuse-dev`) |
| Windows | Deferred (v2.x+) | — |

Windows is deferred because `winfsp-rs` is GPL-3; native Projected FS or Dokan are the v2.x candidates.

## Repo layout

```
.
├── crates/
│   ├── slicefs-traits/        # Trait contracts (metadata, ContentHasher, Chunker, BlockStore, DedupIndex) + ObjectKey
│   ├── cas-local/             # Reference/test impls of the CAS traits (Blake3 hasher, fixed chunker, stores) — not used by the binary
│   ├── chunk-store/           # FastCDC + BLAKE3 objects in pack files: index, zstd, crash scan, GC
│   ├── metadata/              # Inodes, directories, manifests, xattrs, WAL, segments, snapshots, GC mark
│   └── slicefs-cli/           # `slicefs` binary — mount, unmount, seed, gc, snapshot, stats, scrub
├── benchmarks/
├── .github/workflows/ci.yml   # CI: cargo test + pjdfstest (Linux), cargo test (macOS)
└── .planning/                 # Design docs, roadmap, phase artifacts
```

## Build

Requires Rust **1.95+** (edition 2024).

```sh
git clone https://github.com/o2alexanderfedin/slicefs.git
cd slicefs
cargo build --release
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

### Dirty data (mount options)

Dirty data is written file data that is not stored yet. It is kept in memory. Four `slicefs mount` options control it:

| Option | Default | Meaning |
|---|---|---|
| `--dirty-budget <SIZE>` | `64M` | The most dirty data of all open files together. This is memory that the daemon may use for it. |
| `--dirty-limit <SIZE>` | `32M`, or `--dirty-budget` if that is less | The most dirty data of one file. It must not be above `--dirty-budget`. |
| `--dirty-idle <SECS>` | `5` | Store the dirty data of a file after this time with no write to it. |
| `--dirty-expire <SECS>` | `30` | Store dirty data that is this old. |

- SIZE is bytes, or a number with K, M or G. The units are 1024-based, so `64M` is 67108864 bytes. Each SIZE must be at least `4M`. There is no upper bound.
- SECS must be at least 1.
- A background flusher stores dirty data when a file is idle, when its data is old, or when the total is at half the budget or more.
- A writer over the budget waits up to 200 ms for the flusher. Then it writes, and stores part of its own file. The write does not fail. When the flusher cannot store data (for example, after a store error), the writer stops its wait after that flusher pass.
- A file that is written with no pause of 300 ms or more is kept under its limit by its own writer.
- `--dirty-limit` applies to a file that is changed in place (an overlay). A file that is written from start to end (a stream) can also hold writes that come out of order. These writes can use up to 64 MiB (a fixed window), whatever the limit is.
- With a `--dirty-budget` below 64M, a stream with out-of-order writes can become an overlay when the total reaches the budget. The file then does not go back to a stream.

## Test

```sh
cargo test --workspace
```

Linux users can also run the POSIX conformance suite (pjdfstest) — see `.github/workflows/ci.yml` for the exact recipe.

## Architecture (one paragraph)

The `slicefs` binary is built from `slicefs-traits`, `metadata`, and `chunk-store`. The FUSE layer (`fuser`) maps VFS operations onto `DictMetadataStore`, which keeps inodes, directories, file manifests, xattrs, and refcounts in memory. On write, file bytes are cut into FastCDC chunks; each chunk is stored once, under its ID (the hash tag + BLAKE3-256 digest by default), in an append-only pack file, and a file is a manifest tree of chunk IDs. At a commit, the changed metadata nodes are stored in the same packs (in chunks too, so a large map costs only its changed chunks), the active pack is synced once, and a `RootUpdate` is appended to the write log in the segment files. A crash replays the log after the last root. A snapshot is a named, immutable pointer to a committed root.

For deep detail, see `.planning/PROJECT.md` and the per-phase artifacts under `.planning/phases/`.

## License

SliceFS is **dual-licensed**:

- **AGPL-3.0-or-later** for open-source and internal use — see [LICENSE](./LICENSE).
- **Commercial license** for organizations that need to avoid AGPL's source-disclosure obligations (e.g. embedding in closed-source products or offering a proprietary hosted service) — see [COMMERCIAL-LICENSE.md](./COMMERCIAL-LICENSE.md).

## Contributors

See [CONTRIBUTORS.md](./CONTRIBUTORS.md).
