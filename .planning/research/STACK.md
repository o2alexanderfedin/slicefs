# Stack Research

**Domain:** Deduplicating FUSE filesystem in Rust (CAS-backed, cross-platform)
**Researched:** 2026-03-27
**Confidence:** MEDIUM-HIGH — FUSE/Rust layer is HIGH; cross-platform Windows path is MEDIUM

---

## Recommended Stack

### Core Technologies

| Technology | Version | Purpose | Why Recommended |
|------------|---------|---------|-----------------|
| `fuser` | 0.17.0 | FUSE filesystem interface for Linux/macOS | The only actively maintained pure-Rust FUSE implementation; 2,100+ dependent crates; recent release Feb 2026; covers the full FUSE protocol without libfuse on Linux |
| `redb` | 3.1.1 | Metadata storage (inode table, path index, chunk refs) | Pure Rust, ACID, MVCC, zero-copy reads, stable file format, fastest individual writes in its class, no C dependencies; stable since 1.0 |
| `blake3` | 1.8.x | Primary CAS hash function | 80M+ downloads; fastest cryptographic hash available; SIMD-accelerated; 256-bit digests; designed for content-addressable storage; pluggable via trait |
| `fastcdc` | 3.2.1 | Content-defined chunking reference implementation | Official Rust implementation of FastCDC v2016/v2020; async-capable (`AsyncStreamCDC` with tokio feature); deterministic — same input always produces same chunks |
| `tokio` | 1.x | Async runtime | Ecosystem standard; fuser uses blocking threads internally so runtime is needed for background I/O, compaction tasks, and future distributed work |
| `serde` + `bincode` | serde 1.x, bincode 2.x | Metadata serialization | serde is universal; bincode produces compact fixed-width binary records optimal for B-tree storage in redb |

### Supporting Libraries

| Library | Version | Purpose | When to Use |
|---------|---------|---------|-------------|
| `sha2` | 0.10.x | SHA-256 pluggable hash | When security-sensitive dedup proofs are required or BLAKE3 is not acceptable; part of RustCrypto project |
| `winfsp` | 0.12.4 | WinFSP bindings for Windows filesystem | Required for Windows support; wraps WinFSP C library; passes ntptfs test suite; GPL-3 licensed |
| `xattr` | latest | Extended attribute read/write on Unix | Required for POSIX xattr support on macOS and Linux; abstracts platform differences |
| `libc` | 0.2.x | POSIX types and errno constants | Needed for correct FUSE reply construction (uid, gid, mode, nlink types) |
| `thiserror` | 2.x | Structured error types | Domain errors for each subsystem (chunk, hash, metadata, fuse); zero-cost with `?` propagation |
| `tracing` | 0.1.x | Structured logging/instrumentation | async-aware; integrates with tokio; essential for debugging FUSE operation traces |
| `criterion` | 0.5.x | Micro-benchmarking | Statistical benchmarks for hash throughput, chunk throughput, metadata ops; supports bytes/sec reporting |
| `proptest` | 1.x | Property-based testing | Randomized filesystem operation sequences; finds edge cases in inode reference counting |
| `tempfile` | 3.x | Temporary file/dir management in tests | Test isolation; auto-cleanup; works on all target platforms |
| `nix` | 0.29.x | Unix system calls | Needed for low-level mount/unmount helpers and signal handling on Linux/macOS |

### Development Tools

| Tool | Purpose | Notes |
|------|---------|-------|
| `cargo nextest` | Fast parallel test runner | Significantly faster than `cargo test`; required for integration tests that spawn mount processes |
| `cargo clippy` | Linting | Enable `#![deny(clippy::all)]` from the start; FUSE code has subtle lifetime issues clippy catches |
| `cargo flamegraph` | CPU profiling | Mount filesystem, run workload, profile; essential for finding hot paths in FUSE dispatch |
| `pjdfstest` | POSIX conformance test suite | Industry-standard filesystem conformance tool; run on Linux under fuser and on macOS under FUSE-T |
| `rust-analyzer` | IDE language server | Required for productivity in a large trait-heavy codebase |

---

## Platform Matrix

| Platform | FUSE Layer | Status | Notes |
|----------|-----------|--------|-------|
| Linux | `fuser` + kernel FUSE module | HIGH confidence, primary target | fuser is tested on Linux stable; no libfuse required at runtime with `--no-default-features` |
| macOS | `fuser` + FUSE-T | MEDIUM confidence | FUSE-T is drop-in libfuse replacement using NFSv4 under the hood; fuser should work since API headers unchanged; some known NFS client quirks on Sonoma (see Pitfalls) |
| Windows | `winfsp` crate (separate) | MEDIUM confidence, later phase | winfsp-rs 0.12.4 passes ntptfs tests; GPL-3 license may affect distribution; Windows path differs significantly from fuser |

---

## Installation

```toml
# Cargo.toml (workspace root)
[workspace]
members = [
    "crates/slicefs-core",    # CAS engine, chunking traits, hash traits
    "crates/slicefs-meta",    # Metadata store (redb-backed inode table)
    "crates/slicefs-fuse",    # fuser integration, FUSE filesystem impl
    "crates/slicefs-cli",     # mount/umount CLI
]
resolver = "2"

[workspace.dependencies]
fuser      = "0.17"
redb       = "3.1"
blake3     = "1.8"
fastcdc    = "3.2"
tokio      = { version = "1", features = ["full"] }
serde      = { version = "1", features = ["derive"] }
bincode    = "2"
sha2       = "0.10"
xattr      = "1"
libc       = "0.2"
thiserror  = "2"
tracing    = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }

# Dev/test
criterion  = { version = "0.5", features = ["html_reports"] }
proptest   = "1"
tempfile   = "3"

# macOS/Linux
nix        = { version = "0.29", features = ["fs", "mount", "signal"] }

# Windows (conditional)
# winfsp   = "0.12"   # enable in slicefs-fuse with cfg(windows)
```

---

## Alternatives Considered

| Category | Recommended | Alternative | Why Not |
|----------|-------------|-------------|---------|
| FUSE library | `fuser` 0.17 | `fuse3` crate | fuse3 is less maintained, fewer dependents, overlapping scope |
| FUSE library | `fuser` 0.17 | `fuse-rs` (zargony) | Archived/unmaintained — last commit years ago |
| Metadata DB | `redb` 3.x | SQLite (`rusqlite`) | SQLite has better query flexibility but C dependency, slower point writes, and no zero-copy reads; redb is purpose-built for embedded KV |
| Metadata DB | `redb` 3.x | `sled` 0.34 | sled is **beta, pre-1.0, file format unstable**; last release September 2021; maintainer recommends SQLite for reliability-first use cases |
| Metadata DB | `redb` 3.x | RocksDB (`rocksdb` crate) | RocksDB is well-proven but brings large C++ build dependency; build times are painful; overkill for single-node metadata |
| Hash | `blake3` | SHA-256 (`sha2`) | BLAKE3 is 3-10x faster on modern hardware while maintaining 256-bit security; better for CAS where hashing is on the critical path |
| Chunking | `fastcdc` | Custom from owner's repo | Owner's existing chunking technology should be **integrated as the pluggable backend** — the fastcdc crate serves as the default/fallback until owner's chunker is wired in |
| Serialization | `bincode` 2.x | `postcard` | postcard targets no_std/embedded; bincode has better performance on std targets with fixed-width types |
| Async runtime | `tokio` | `async-std` | tokio is the ecosystem standard; fuser's async examples use tokio; broader library compatibility |

---

## What NOT to Use

| Avoid | Why | Use Instead |
|-------|-----|-------------|
| `fuse-rs` (zargony/fuse-rs) | Archived, unmaintained, stuck at FUSE2 API | `fuser` (cberner/fuser) |
| `sled` | Beta, pre-1.0, file format changes between releases, last release 2021, maintainer says "use SQLite if you need reliability" | `redb` |
| `macfuse` (kernel extension) | Requires kext signing and user approval on modern macOS; broken on Apple Silicon without SIP changes | FUSE-T (userspace, kext-free) |
| Direct `libfuse` C bindings | Unsafe, brittle, loses Rust memory safety guarantees | `fuser` which wraps or reimplements libfuse in Rust |
| `rocksdb` crate | C++ build dependency makes CI painful; 30+ minute clean builds; overkill for this use case | `redb` for metadata; flat files for block data |
| `bincode` 1.x | Breaking API changes in 2.x; 1.x has known soundness issues | `bincode` 2.x with explicit configuration |
| Global `SHA-256` only hashing | Locks in single algorithm; breaks pluggable CAS promise | Abstract behind a `Hasher` trait; BLAKE3 as default |

---

## Stack Patterns by Variant

**If chunk storage needs to survive crashes without journal replay:**
- Use redb for chunk reference counts (ACID)
- Store raw block data as flat files keyed by hash (first 2 bytes as directory sharding, e.g. `ab/cdef...`)
- This avoids storing large blobs in redb B-trees which degrades performance

**If plugging in owner's chunking technology:**
- Define `trait Chunker: Send + Sync { fn chunk(&self, data: &[u8]) -> Vec<Chunk>; }`
- Wire `fastcdc` as the default impl
- Owner's algorithm slots in as an alternate impl without touching core CAS logic

**If Windows support is added later:**
- `winfsp` crate provides a separate filesystem trait; create a platform-abstracted `FilesystemBackend` trait
- Do NOT attempt to route Windows through fuser — winfsp-rs has its own API surface
- The GPL-3 license of winfsp-rs requires the binary distribution to also be GPL-3

**If distributed backend is added later (future milestone):**
- The storage backend trait already isolates local vs. remote; swap the redb backend for a networked one
- No fuser or metadata schema changes needed if interfaces are clean from the start

---

## Version Compatibility

| Package | Compatible With | Notes |
|---------|-----------------|-------|
| `fuser` 0.17 | Rust stable (1.75+) | Tests on Linux and FreeBSD; macOS marked "untested" in README but works with FUSE-T via libfuse API compatibility |
| `redb` 3.x | Rust stable | File format stable since 1.0; 3.x has breaking API changes from 2.x — use 3.x from the start |
| `blake3` 1.8.x | Rust stable | No breaking changes expected; pure Rust with optional C SIMD via feature flags |
| `fastcdc` 3.2.1 | Rust stable; tokio 1.x | `tokio` feature enables `AsyncStreamCDC`; `futures` feature for futures-compatible async |
| `winfsp` 0.12 | Rust stable (Windows only) | Requires WinFSP runtime installed on target machine; links against WinFSP import lib by default |
| `bincode` 2.x | serde 1.x | bincode 2.x has incompatible wire format with 1.x — do not mix versions |

---

## Sources

- [fuser on GitHub (cberner/fuser)](https://github.com/cberner/fuser) — version 0.17.0 confirmed, platform support, Feb 2026 release
- [redb on GitHub (cberner/redb)](https://github.com/cberner/redb) — version 3.1.1 confirmed, ACID/MVCC features, Mar 2026 release
- [blake3 on crates.io](https://crates.io/crates/blake3) — version 1.8.x, 80M downloads, SIMD acceleration confirmed
- [fastcdc on GitHub (nlfiedler/fastcdc-rs)](https://github.com/nlfiedler/fastcdc-rs) — version 3.2.1 confirmed, async API confirmed
- [winfsp-rs on GitHub (SnowflakePowered/winfsp-rs)](https://github.com/SnowflakePowered/winfsp-rs) — version 0.12.4, GPL-3, ntptfs test passing
- [FUSE-T on GitHub (macos-fuse-t/fuse-t)](https://github.com/macos-fuse-t/fuse-t) — kext-free NFSv4 backend, drop-in libfuse compatibility
- [sled on GitHub (spacejam/sled)](https://github.com/spacejam/sled) — last release 2021, pre-1.0, file format unstable, explicitly NOT recommended for production
- [Rust Serialization Benchmarks](https://github.com/djkoloski/rust_serialization_benchmark) — bincode vs postcard performance comparison
- WebSearch: RocksDB Rust crate 0.24.0 — MEDIUM confidence
- WebSearch: sha2 0.10.x RustCrypto — MEDIUM confidence

---

*Stack research for: DedupFS — deduplicating FUSE filesystem in Rust*
*Researched: 2026-03-27*
