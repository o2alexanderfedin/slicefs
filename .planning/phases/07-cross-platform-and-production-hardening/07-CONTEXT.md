# Phase 7: Cross-Platform and Production Hardening - Context

**Gathered:** 2026-03-29
**Status:** Ready for planning

<domain>
## Phase Boundary

The filesystem runs on macOS (via FUSE-T) and Linux with full POSIX compliance testing on both platforms. The CLI is complete with stats, scrub, and structured JSON output. Benchmark baselines document daily-driver performance. GitHub Actions CI enforces regression prevention on both platforms. Windows support is deferred to v2.

Requirements: PLAT-01, PLAT-04, CLI-03, CLI-04, CLI-05

</domain>

<decisions>
## Implementation Decisions

### FUSE-T write path fix (blocking prerequisite)
- **Fix first, everything else depends on it** — the FUSE-T write issue (data written but reads return empty) blocks all macOS live testing, POSIX compliance, and benchmarks
- Known symptoms: writes succeed (files appear in ls), fsync returns success, but cat returns empty data. Likely FUSE-T NFS translation layer issue
- Investigate: getattr returning stale file size after writes, write buffer not flushing on release() through NFS, whether direct_io or allow_other mount options help
- **Automate build ergonomics:**
  - `.cargo/config.toml` sets `PKG_CONFIG_PATH=.pkgconfig` so `cargo build` just works on macOS with FUSE-T
  - `build.rs` or cargo config sets `-rpath /usr/local/lib` on macOS — no manual `install_name_tool` step after build

### macOS POSIX compliance (PLAT-01)
- **Same level as Linux** — run the custom POSIX test suite from Phase 4 on macOS via FUSE-T. Expect >95% pass rate
- macOS-specific behaviors (resource forks, .DS_Store, ._files) handled gracefully — not errors, just filtered or stored as xattrs

### Windows support scope (PLAT-03)
- **Deferred to v2** — winfsp-rs is GPL-3, incompatible with most commercial licensing. Remove PLAT-03 from Phase 7 scope
- Windows support requires either a license decision or alternative backend (dokan-rs, ProjFS) — v2 milestone

### Platform testing (PLAT-04)
- **macOS + Linux both covered** — GitHub Actions CI with runners for both platforms
- Linux CI: cargo test + pjdfstest (deferred from Phase 4) — >95% compliance gate
- macOS CI: cargo test + custom POSIX test suite via FUSE-T
- CI runs on every push to main and on PRs

### CLI stats command (CLI-03)
- **Core + distribution + per-snapshot metrics:**
  - Core: dedup ratio, logical bytes, physical bytes, block count, snapshot count, compressor in use
  - Distribution: reference count distribution (blocks with refcount 1, 2, 3+)
  - Per-snapshot: unique block count and size for each snapshot (requires walking snapshot trees)
- Both human-readable table and JSON output (via global --json flag)

### CLI scrub command (CLI-04)
- **Report only, never modify** — log corrupted block hash, which files reference it, type of corruption. Exit with non-zero status if corruption found. Safe for production
- **Both online + offline** — works while mounted (read-only scan) and unmounted. Online scrub coordinates with active writes/GC
- Scrub walks all stored blocks, re-verifies hashes against stored content

### Structured JSON output (CLI-05)
- **Global --json flag on all commands** — mount, unmount, seed, gc, snapshot, stats, scrub all output structured JSON when --json is passed. Enables scripting and tooling integration for every operation

### Benchmark baselines
- **Target, not hard gate** — 200 MB/s sequential write on NVMe is a target. Document actual results and identify bottlenecks. Not a blocker for v1 release
- **Comprehensive suite:** sequential write, sequential read, random read, small file create/delete, large file dedup ratio, metadata-heavy (many small files), GC throughput
- **Manual with documented process** — benchmark script in repo, results in BENCHMARKS.md. Not in CI (hardware-dependent). Developer runs on target hardware
- **Dedup index memory:** verify + document current bloom filter memory under 100GB load. The bloom filter is already fixed-size from Phase 1. If bounded, document. If not, add a cap

### Claude's Discretion
- FUSE-T write path debugging approach and fix
- GitHub Actions workflow configuration details
- Benchmark script implementation (fio configs, measurement methodology)
- JSON schema for structured output
- How online scrub coordinates with active writes
- pjdfstest skip list for the <5% expected failures

</decisions>

<code_context>
## Existing Code Insights

### Reusable Assets
- `.pkgconfig/fuse.pc` — FUSE-T compatibility shim already committed
- `SliceFsFilesystem` with all FUSE callbacks — write path exists, needs FUSE-T debugging
- `test_*` helper methods bypass FUSE — integration tests work without mount
- `DictMetadataStore::list_snapshots()`, `snapshot_roots()` — stats can use these
- `collect_live_set()` + `GarbageCollector` — scrub can reuse the tree-walking logic
- Existing clap CLI with mount/unmount/seed/gc/snapshot subcommands — stats/scrub follow same pattern
- Segment reader/writer — scrub reads segments directly to verify entries

### Established Patterns
- Pluggable traits for all core components (hash, chunk, compress, WAL)
- Offline CLI commands check `mount.lock` before running (GC does this)
- 92-byte Dictionary entries with Digest224 keys — scrub re-hashes and compares
- `--wal-strategy` and `--compressor` as mount-time CLI flags — `--json` follows same pattern

### Integration Points
- `slicefs stats <store>` — new clap subcommand, reads segments + snapshots
- `slicefs scrub <store>` — new clap subcommand, walks all segment entries
- `--json` global flag — add to top-level clap Args
- `.cargo/config.toml` — new file for PKG_CONFIG_PATH and macOS linker flags
- `.github/workflows/` — new CI configuration
- `benchmarks/` — new directory for fio configs and benchmark scripts

</code_context>

<specifics>
## Specific Ideas

- The FUSE-T write issue is the single most important blocker — everything else (macOS testing, benchmarks, live UAT) depends on it
- Windows deferral keeps Phase 7 focused: macOS + Linux + CLI + benchmarks is already substantial
- Stats with per-snapshot breakdown gives users actionable storage management insights
- Online scrub (read-only while mounted) is a premium feature — most filesystems only support offline scrub
- Global --json flag is better than per-command flags for consistency and scripting

</specifics>

<deferred>
## Deferred Ideas

- **Windows support (PLAT-03)** — deferred to v2 due to winfsp-rs GPL-3 license. Investigate dokan-rs or ProjFS as alternatives
- **CI benchmark regression detection** — requires consistent CI hardware. Defer to when project has dedicated infra
- **Scrub with quarantine** — move corrupted blocks to quarantine directory. Defer to v2 if report-only is insufficient
- **fdatasync optimization** — skip metadata-only writes. Deferred from Phase 5
- **TRIM/discard hints** — SSD TRIM support for deleted segments. Deferred from Phase 5

</deferred>

---

*Phase: 07-cross-platform-and-production-hardening*
*Context gathered: 2026-03-29*
