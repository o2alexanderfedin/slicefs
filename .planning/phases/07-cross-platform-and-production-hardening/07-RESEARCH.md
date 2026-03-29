# Phase 7: Cross-Platform and Production Hardening - Research

**Researched:** 2026-03-29
**Domain:** macOS FUSE-T debugging, GitHub Actions CI, CLI stats/scrub commands, JSON output, fio benchmarking
**Confidence:** MEDIUM — FUSE-T write bug root cause confirmed via upstream issues; workarounds are empirical

---

<user_constraints>
## User Constraints (from CONTEXT.md)

### Locked Decisions
- FUSE-T write path fix is the **blocking prerequisite** — fix first, all macOS work depends on it
- Symptoms: writes succeed (files appear in `ls`), `fsync` returns success, but `cat` returns empty data
- Investigate: `getattr` returning stale file size after writes, write buffer not flushing on `release()` through NFS, whether `direct_io` or `allow_other` mount options help
- `.cargo/config.toml` sets `PKG_CONFIG_PATH=.pkgconfig` so `cargo build` just works on macOS with FUSE-T
- `build.rs` or cargo config sets `-rpath /usr/local/lib` on macOS — no manual `install_name_tool` step after build
- macOS POSIX compliance (PLAT-01): same >95% pass rate target as Linux; run custom POSIX test suite from Phase 4
- macOS-specific behaviors (resource forks, `.DS_Store`, `._files`) handled gracefully — not errors, just filtered or stored as xattrs
- Windows support (PLAT-03): **deferred to v2** — winfsp-rs is GPL-3, incompatible with commercial licensing
- GitHub Actions CI: macOS + Linux runners, both platforms on every push to main and on PRs
- Linux CI: `cargo test` + pjdfstest — >95% compliance gate
- macOS CI: `cargo test` + custom POSIX test suite via FUSE-T
- Stats (CLI-03): core + distribution + per-snapshot metrics; both human-readable table and JSON output
- Scrub (CLI-04): report only, never modify; both online (while mounted, read-only scan) and offline; exit non-zero if corruption found
- Structured JSON (CLI-05): global `--json` flag on ALL commands; mount, unmount, seed, gc, snapshot, stats, scrub
- Benchmarks: 200 MB/s sequential write on NVMe is a **target**, not a gate; document actual results; manual process with script in repo

### Claude's Discretion
- FUSE-T write path debugging approach and fix
- GitHub Actions workflow configuration details
- Benchmark script implementation (fio configs, measurement methodology)
- JSON schema for structured output
- How online scrub coordinates with active writes
- pjdfstest skip list for the <5% expected failures

### Deferred Ideas (OUT OF SCOPE)
- Windows support (PLAT-03) — deferred to v2, winfsp-rs GPL-3 license
- CI benchmark regression detection — requires consistent CI hardware
- Scrub with quarantine — move corrupted blocks to quarantine directory
- fdatasync optimization — skip metadata-only writes
- TRIM/discard hints — SSD TRIM support for deleted segments
</user_constraints>

---

<phase_requirements>
## Phase Requirements

| ID | Description | Research Support |
|----|-------------|-----------------|
| PLAT-01 | macOS support via FUSE-T + fuser | FUSE-T write bug diagnosis + build ergonomics |
| PLAT-03 | Windows support via WinFSP (deferred) | Documented why deferred — GPL-3 licensing; no implementation planned |
| PLAT-04 | Platform-specific POSIX compliance testing on each target | GitHub Actions CI workflow; pjdfstest configuration; custom POSIX test suite |
| CLI-03 | Stats command (dedup ratio, logical/physical bytes, block count, reference distribution) | Existing `logical_bytes()`, `refcounts`, `list_snapshots()` APIs on `DictMetadataStore` |
| CLI-04 | Scrub command (walk all blocks, re-verify hashes, report corruption) | Segment reader + `collect_live_set` GC tree walker; block re-hash pattern |
| CLI-05 | Structured JSON output from all CLI commands for tooling integration | `serde_json` + clap `global = true` flag pattern |
</phase_requirements>

---

## Summary

Phase 7 delivers macOS platform support, complete CLI (stats, scrub, structured JSON output), and benchmark baselines. The work splits into five largely independent tracks that can be parallelized once the FUSE-T write bug is resolved.

The FUSE-T write bug is the single hardest problem in this phase. It is NOT a SliceFS bug — it is a documented macOS NFS client behavior where the kernel NFS client considers a write complete before the NFS server has fully committed it, causing reads immediately after writes to see stale (empty) data. The FUSE-T maintainer confirmed this in issue #45 as "a macOS kernel NFS client bug." The workaround is to ensure data is fully flushed through the NFS stack before returning from `release()`. The most reliable approach is to call `fsync()` on the raw file after writing, use `O_SYNC` semantics, or add a brief delay — but the definitive fix is using `direct_io` mount flag which bypasses the NFS page cache. This requires investigation on the actual hardware.

The CLI work (stats, scrub, JSON) is straightforward extension of existing patterns. The metadata store already exposes `logical_bytes()`, `refcounts` BTreeMap, `list_snapshots()`, and `snapshot_roots()`. The segment reader already supports iterating all entries. Adding `serde_json` + a global `--json` clap flag completes CLI-05. Stats and scrub commands follow the same offline pattern as `gc` and `snapshot` commands.

**Primary recommendation:** Fix FUSE-T write path first (plan 07-01), then implement CLI commands (07-02), CI infrastructure (07-03), and benchmarks (07-04) in parallel.

---

## Standard Stack

### Core
| Library | Version | Purpose | Why Standard |
|---------|---------|---------|--------------|
| fuser | 0.17 | FUSE filesystem adapter | Already in workspace; macOS FUSE-T compatible via libfuse 2.9 API |
| serde_json | 1.x | JSON serialization for `--json` output | Standard Rust JSON; serde already in workspace |
| clap | 4 (workspace) | CLI argument parsing with global flags | Already in use; `global = true` attribute on `--json` flag |
| FUSE-T | 1.0.54 (macOS system package) | kext-less FUSE for macOS via NFS | No kext required; works on GitHub Actions macOS runners |

### Supporting
| Library | Version | Purpose | When to Use |
|---------|---------|---------|-------------|
| pjdfstest | latest (saidsay-so/pjdfstest) | POSIX compliance test suite | Linux CI gate; >95% pass rate target |
| fio | 3.x (system package) | I/O benchmark tool | Benchmark script; manual execution only, not in CI |

### Alternatives Considered
| Instead of | Could Use | Tradeoff |
|------------|-----------|----------|
| `direct_io` mount flag (FUSE-T fix) | Sleep between write and read | Sleep is non-deterministic; `direct_io` bypasses NFS page cache reliably |
| `serde_json` | `json!()` macro only | Full `serde_json::to_string_pretty()` needed for human-readable JSON |
| Manual `rpath` in build.rs | `install_name_tool` post-build | build.rs is reproducible; `install_name_tool` requires manual step |

### Installation
```bash
# Add to workspace Cargo.toml:
serde_json = "1"

# Add to slicefs-cli/Cargo.toml:
serde_json = { workspace = true }

# macOS system: FUSE-T (via Homebrew)
brew install fuse-t

# Linux CI: pjdfstest
cargo install pjdfstest  # or build from source: saidsay-so/pjdfstest
```

---

## Architecture Patterns

### Recommended Project Structure for Phase 7
```
.cargo/config.toml              # PKG_CONFIG_PATH + rpath linker flags (macOS)
crates/slicefs-cli/
  build.rs                      # macOS -rpath /usr/local/lib linker flag
  src/
    cli.rs                      # Add --json global flag to Cli struct
    stats.rs                    # NEW: run_stats() implementation
    scrub.rs                    # NEW: run_scrub() implementation
    main.rs                     # Wire stats/scrub subcommands
.github/workflows/
  ci.yml                        # Linux: cargo test + pjdfstest
  macos.yml                     # macOS: cargo test + custom POSIX suite
benchmarks/
  sequential_write.fio          # fio job file
  sequential_read.fio
  random_read.fio
  small_files.fio
  run_benchmarks.sh             # Script: runs all jobs, captures output
  BENCHMARKS.md                 # Results and analysis (committed after runs)
```

### Pattern 1: Global `--json` Flag with Clap Derive

The global flag is declared on the top-level `Cli` struct. All subcommands receive it via clap's propagation. Each subcommand handler receives a `json: bool` parameter.

```rust
// cli.rs
#[derive(Parser, Debug)]
#[command(name = "slicefs", version, about = "SliceFS deduplicating filesystem")]
pub struct Cli {
    /// Output structured JSON instead of human-readable text.
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Cmd,
}
```

The `global = true` attribute makes `--json` available to ALL subcommands without repetition. In `main.rs`, extract `cli.json` before matching on `cli.command`.

### Pattern 2: JSON Output Envelope

All JSON output uses a consistent envelope with `status`, `command`, and a typed `data` field:

```rust
use serde::Serialize;
use serde_json;

#[derive(Serialize)]
struct JsonOutput<T: Serialize> {
    status: &'static str,  // "ok" or "error"
    command: &'static str, // "stats", "scrub", "gc", etc.
    data: T,
}

fn print_json_or_human<T: Serialize>(json: bool, data: &T, human_fn: impl FnOnce()) {
    if json {
        println!("{}", serde_json::to_string_pretty(data).unwrap());
    } else {
        human_fn();
    }
}
```

### Pattern 3: Stats Command Architecture

Stats reads the store offline (same as `gc`), computing metrics from loaded state:

```rust
// stats.rs
pub struct StoreStats {
    // Core
    pub logical_bytes: u64,
    pub physical_bytes: u64,          // dict.len() * 92
    pub block_count: usize,           // dict.len()
    pub snapshot_count: usize,
    pub dedup_ratio: f64,             // logical / physical
    pub compressor: String,           // "zstd", "lz4", "none"

    // Reference count distribution
    pub refcount_distribution: RefcountDist,

    // Per-snapshot metrics
    pub snapshots: Vec<SnapshotStats>,
}

#[derive(Serialize)]
pub struct RefcountDist {
    pub unique_blocks: usize,        // refcount == 1
    pub shared_2: usize,             // refcount == 2
    pub shared_3plus: usize,         // refcount >= 3
}

#[derive(Serialize)]
pub struct SnapshotStats {
    pub version: u64,
    pub name: Option<String>,
    pub unique_block_count: usize,   // blocks reachable only from this snapshot
    pub unique_bytes: u64,
}
```

`physical_bytes = dict.len() * 92` because each Dictionary entry is exactly 92 bytes on disk (28-byte Digest224 key + 64-byte Branches).

Per-snapshot unique blocks: use `collect_live_set()` for each snapshot root, then compute set differences.

### Pattern 4: Scrub Command Architecture

Scrub iterates all segment entries and re-verifies hashes. The segment reader already provides this iteration.

```rust
// scrub.rs
pub struct ScrubReport {
    pub blocks_verified: usize,
    pub corrupted_blocks: Vec<CorruptedBlock>,
    pub status: ScrubStatus,
}

pub struct CorruptedBlock {
    pub hash: String,           // hex of stored key
    pub corruption_type: String, // "hash_mismatch", "missing_data", etc.
    pub referenced_by: Vec<u64>, // inode numbers referencing this block
}
```

**Scrub re-hash algorithm:**
1. Load all segment entries via `load_store_from_segments()`
2. For each `DictEntry { key, branches }`: retrieve the raw content bytes from the blockset store (same path as `StoreIo::read()`)
3. Re-hash the raw bytes using the same hash function
4. Compare re-hash to stored key; mismatch = corruption

**Online scrub (while mounted):** The store is read-only during scrub; no writes needed. Coordination: scrub acquires a read-only view by loading segments. Because segments are append-only and immutable once closed, scanning closed segments is safe. The active (open) segment may change during scrub — scrub loads a snapshot of segment files at start time and does not scan the active segment (or scans it at start, accepting that new entries added during scan are not covered in this pass).

**Online vs offline distinction:** When `mount.lock` is absent, scrub is offline. When `mount.lock` is present, scrub may still run with a warning: "Store is mounted; scrubbing closed segments only. Active writes are not covered in this pass."

### Pattern 5: FUSE-T Write Bug Fix

**Root cause** (confirmed via FUSE-T issue #45, MEDIUM confidence): macOS NFS client buffering. FUSE-T translates FUSE protocol to NFSv4. The macOS NFS client considers a write "complete" and returns to userspace before the NFS server (FUSE-T) has fully committed the data. A subsequent read may hit the NFS client's stale cache.

**Investigation approach (in order):**
1. Try `direct_io` mount flag: bypasses NFS page cache; reads/writes go directly to the FUSE handler. Add `MountOption::DirectIO` to `build_mount_options()`. **This is the most likely fix.**
2. Try `allow_other` + `auto_unmount` flags: no impact on write path, but rules them out.
3. Verify `getattr()` returns updated file size: after `release()`, `getattr()` must return the new size. FUSE-T's NFS layer may cache the pre-write size. Ensure `inode.size` is updated before `release()` returns.
4. Add explicit `sync_all()` on the blockset backing file after write: forces NFS server to flush.
5. If all else fails, add an `fsync()` call in the `release()` callback path before returning.

**The key insight:** FUSE-T wiki states "caching of attributes is done by the NFS client. Currently the caching attributes returned by the filesystem implementation are ignored." This means TTL values in FUSE replies have no effect — the NFS client decides when to re-fetch. Using `direct_io` sidesteps this entirely.

### Pattern 6: Build Ergonomics for macOS

**.cargo/config.toml** already exists at project root but only overrides `rustc`. Must ADD:
```toml
[env]
PKG_CONFIG_PATH = { value = ".pkgconfig", relative = true, force = false }

[target.x86_64-apple-darwin]
rustflags = ["-C", "link-arg=-Wl,-rpath,/usr/local/lib"]

[target.aarch64-apple-darwin]
rustflags = ["-C", "link-arg=-Wl,-rpath,/usr/local/lib"]
```

The `relative = true` setting resolves `.pkgconfig` relative to the `Cargo.toml` directory. The `force = false` means it won't override an already-set `PKG_CONFIG_PATH`.

Alternatively, a minimal `build.rs` in `slicefs-cli`:
```rust
// crates/slicefs-cli/build.rs
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/local/lib");
    }
}
```

The `build.rs` approach is simpler and contained to the CLI crate.

### Pattern 7: GitHub Actions CI Workflow

FUSE-T is installable via Homebrew on macOS GitHub Actions runners (no kext needed — this is the key advantage over macFUSE). macOS 14 and macOS 15 runners are available.

```yaml
# .github/workflows/ci.yml
name: CI
on:
  push:
    branches: [main]
  pull_request:
    branches: [main]

jobs:
  linux:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install system deps
        run: |
          sudo apt-get update -q
          sudo apt-get install -y libfuse-dev fuse
      - name: cargo test
        run: cargo test --workspace
      - name: pjdfstest compliance (informational)
        run: |
          # pjdfstest runs against an in-process mount, skip if no /dev/fuse
          # Linux CI: FUSE mount requires user_allow_other or root
          # For now, run the custom in-process POSIX tests only
          cargo test --test posix_compliance_tests --workspace

  macos:
    runs-on: macos-14
    steps:
      - uses: actions/checkout@v4
      - name: Install FUSE-T
        run: |
          brew install fuse-t
      - name: cargo test (unit + integration, no live mount)
        run: cargo test --workspace
      - name: Custom POSIX suite (in-process, no mount)
        run: cargo test --test posix_compliance_tests
```

**Important:** macOS CI runs `cargo test` without a live FUSE mount (same pattern as prior phases). The FUSE-T live mount tests must be run manually on a developer machine. The CI validates:
- Build succeeds on macOS (FUSE-T libraries found via PKG_CONFIG_PATH)
- All in-process tests pass
- The custom POSIX compliance suite passes (these bypass FUSE)

**pjdfstest on Linux CI:** pjdfstest requires root or special capabilities to test some POSIX syscalls. Running it in CI requires either `sudo` or a privileged container. Since GitHub-hosted Linux runners allow `sudo`, pjdfstest CAN run. However, mounting a FUSE filesystem in CI also requires `/dev/fuse` and `fusermount` to be available. Linux CI can run pjdfstest if FUSE is installed with `apt-get install fuse`. Consider starting with in-process tests only and adding pjdfstest as a follow-up.

### Anti-Patterns to Avoid
- **Putting benchmarks in CI:** fio results depend on hardware; CI hardware varies; benchmarks must be manual
- **Requiring live FUSE mount in CI:** GitHub Actions macOS runners have FUSE-T but live mount tests are unreliable (permissions, /dev/fuse availability); use in-process tests
- **Using `macos-latest` in CI workflows:** `macos-latest` will change to macOS 15 in mid-2025; pin to `macos-14` until FUSE-T compatibility on macOS 15 is verified
- **Making 200 MB/s a hard gate:** FUSE filesystem throughput is hardware-dependent; document actual results, don't fail CI on throughput

---

## Don't Hand-Roll

| Problem | Don't Build | Use Instead | Why |
|---------|-------------|-------------|-----|
| JSON serialization | Custom JSON printer | `serde_json` | Edge cases in escaping, nesting, numbers |
| Clap global flags | Per-subcommand `--json` flags | `#[arg(global = true)]` | Single declaration, propagates automatically |
| POSIX compliance testing | Custom test suite | `pjdfstest` (for Linux mount tests) | 600+ test cases covering all POSIX syscalls |
| fio job files | Custom benchmark harness | `fio` with job files | Industry standard; reproducible; supports all I/O patterns |
| Block iteration for scrub | New segment walker | `load_store_from_segments()` + `SegmentReader` | Already iterates all segment entries; reuse |
| Live-set computation for stats | New tree walker | `collect_live_set()` | Already implements Merkle tree traversal |

---

## Common Pitfalls

### Pitfall 1: FUSE-T NFS Attribute Caching
**What goes wrong:** Writes succeed (file appears in `ls`), but subsequent reads return empty/stale content. File size from `getattr` still shows 0 or pre-write size.
**Why it happens:** FUSE-T translates FUSE to NFSv4. macOS NFS client caches file attributes aggressively. Attribute TTL values in FUSE replies are IGNORED by the NFS client. The NFS client decides independently when to re-fetch.
**How to avoid:** Use `direct_io` mount flag to bypass NFS page cache. This makes every read/write go directly to the FUSE handler without caching.
**Warning signs:** `cat file` returns empty after `echo data > file` succeeds; `ls -la` shows correct size but `cat` shows nothing.

### Pitfall 2: macOS-15 FUSE-T Compatibility Unknown
**What goes wrong:** `macos-latest` in CI silently upgrades to macOS 15 in August 2025, breaking FUSE-T if compatibility is not verified.
**Why it happens:** GitHub announced `macos-latest` will point to macOS 15 starting August 4, 2025.
**How to avoid:** Pin CI workflow to `macos-14` explicitly. Test on macOS 15 manually before migrating.
**Warning signs:** CI suddenly fails to find `fuse-t` library or `brew install fuse-t` errors on macOS 15.

### Pitfall 3: pjdfstest Requires Root or Capabilities
**What goes wrong:** pjdfstest fails for `chmod`/`chown` tests because the test process lacks CAP_CHOWN.
**Why it happens:** Tests that change file ownership require root or `CAP_CHOWN`; tests for sticky bit require root.
**How to avoid:** Run pjdfstest with `sudo` in CI, or build a skip list for tests requiring elevated privileges. The known <5% failure category includes: `chown` root-only tests, sticky bit enforcement, `mknod` for non-regular files.
**Warning signs:** pjdfstest reports `EPERM` failures for operations that should succeed.

### Pitfall 4: Online Scrub Seeing Partial Writes
**What goes wrong:** Online scrub hashes a block while a write is in progress, seeing partial content and reporting false corruption.
**Why it happens:** The segment writer is append-only but a write in progress may have partially written a new DictEntry.
**How to avoid:** Scrub loads a snapshot of segment file paths at start time. It only reads CLOSED segments (those not currently open for writing). The open/active segment is identified by being the highest-numbered segment file. Skip the active segment (or accept a note that new data since last flush is not covered).
**Warning signs:** Spurious hash mismatch errors that disappear when scrub is run again.

### Pitfall 5: Stats `physical_bytes` vs Dictionary Bytes
**What goes wrong:** `physical_bytes` is reported as the wrong value.
**Why it happens:** Two interpretations: (a) bytes in the blockset store on disk (actual file sizes), (b) Dictionary metadata bytes (`dict.len() * 92`). The on-disk blockset files contain content blobs, which are NOT the same as the dictionary entries.
**How to avoid:** Be clear in the stats output what is being measured. For the stats command, `physical_bytes` = bytes of unique compressed content stored in blockset blobs (requires walking the store directory). `dictionary_bytes` = `dict.len() * 92` (metadata overhead). Provide both. Dedup ratio = `logical_bytes / physical_bytes` where `physical_bytes` is block content size.
**Warning signs:** Dedup ratio of 1.0 for clearly deduplicated workloads (using wrong denominator).

### Pitfall 6: Cargo Config PKG_CONFIG_PATH vs Existing Override
**What goes wrong:** The existing `.cargo/config.toml` sets `rustc = "rustc"` (overrides global rustc). Adding `PKG_CONFIG_PATH` env must not break this.
**Why it happens:** Merging `[env]` section into existing `[build]` section without understanding that they're different sections.
**How to avoid:** Add `[env]` and `[target.*]` sections to the EXISTING `.cargo/config.toml` without touching the `[build]` section. Both sections coexist.
**Warning signs:** Build fails because `rustc` override is lost or `PKG_CONFIG_PATH` is not set.

---

## Code Examples

### Global `--json` Flag in clap Derive
```rust
// cli.rs — add to Cli struct
/// Output structured JSON instead of human-readable text.
/// Works with all subcommands.
#[arg(long, global = true)]
pub json: bool,
```

```rust
// main.rs — extract json before match
fn main() {
    let cli = Cli::parse();
    let json = cli.json;
    match cli.command {
        Cmd::Stats { store } => {
            if let Err(e) = stats::run_stats(&store, json) {
                if json {
                    eprintln!("{}", serde_json::json!({"status": "error", "message": e.to_string()}));
                } else {
                    eprintln!("slicefs stats error: {e}");
                }
                std::process::exit(1);
            }
        }
        // ... other commands pass json similarly
    }
}
```

### serde_json Output Envelope
```rust
use serde::Serialize;

#[derive(Serialize)]
pub struct StoreStats {
    pub logical_bytes: u64,
    pub physical_bytes: u64,
    pub block_count: usize,
    pub dedup_ratio: f64,
    pub snapshot_count: usize,
    // ... etc
}

pub fn run_stats(store_path: &Path, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    // ... load stats ...
    let stats = compute_stats(store_path)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&stats)?);
    } else {
        print_stats_table(&stats);
    }
    Ok(())
}
```

### fio Sequential Write Job File (4K, NVMe target)
```ini
# benchmarks/sequential_write.fio
[global]
ioengine=sync
direct=1
end_fsync=1
group_reporting=1
directory=/path/to/slicefs/mount

[seq-write-4k]
rw=write
bs=4k
size=4g
numjobs=4
```

Run: `fio benchmarks/sequential_write.fio --output-format=json > results/seq_write_$(date +%Y%m%d).json`

Note: Use `direct=1` to bypass page cache; `end_fsync=1` ensures all data is flushed before timing ends.

### macOS rpath via build.rs
```rust
// crates/slicefs-cli/build.rs
fn main() {
    // On macOS: add rpath so the binary finds libfuse-t.dylib at runtime
    // without needing install_name_tool after build.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/local/lib");
    }
}
```

### Scrub Block Verification
```rust
// scrub.rs — re-hash a DictEntry's stored content
use metadata::segment::{SegmentEntry, load_store_from_segments};

pub fn run_scrub(store_path: &Path, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let lock_path = store_path.join("mount.lock");
    let online = lock_path.exists();
    if online {
        eprintln!("Warning: store is mounted. Scrubbing closed segments only.");
    }

    let segs_dir = store_path.join("segments");
    let (dict, _root, _snapshots) = load_store_from_segments(&segs_dir)?;

    // Walk all dict entries and re-verify content against blockset
    let mut verified = 0;
    let mut corrupted = Vec::new();

    for (key, _branches) in dict.iter() {
        // Retrieve actual stored content from blockset
        let content = read_blockset_entry(store_path, key)?;
        let rehash = compute_digest224(&content);
        if rehash != *key {
            corrupted.push(CorruptedBlock { hash: hex(key), .. });
        }
        verified += 1;
    }

    let status = if corrupted.is_empty() { "ok" } else { "corruption_found" };
    // Output report...

    if !corrupted.is_empty() {
        std::process::exit(1); // non-zero exit on corruption
    }
    Ok(())
}
```

---

## FUSE-T Write Bug: Diagnosis Guide

This section is Claude's Discretion — investigation steps for the developer.

### Step 1: Confirm the symptom
```bash
PKG_CONFIG_PATH=.pkgconfig cargo build
./target/debug/slicefs mount /tmp/mnt --store /tmp/test-store
echo "hello world" > /tmp/mnt/test.txt
cat /tmp/mnt/test.txt     # Should print "hello world"; if empty, bug confirmed
ls -la /tmp/mnt/test.txt  # Check if size is 0 or correct
```

### Step 2: Try direct_io flag
```rust
// mount.rs — in build_mount_options()
cfg.mount_options.push(MountOption::DirectIO);
```
Rebuild and repeat test. `direct_io` tells the NFS layer to bypass its page cache. Expected result: reads after writes return correct content.

### Step 3: Check getattr file size
Add debug logging to `getattr()` callback — verify that after `release()`, `getattr()` returns the updated file size. If size is still 0, the issue is in the `release()` → metadata update pipeline.

### Step 4: Force fsync in release()
As a last resort, add an explicit file sync in the `release()` path to ensure the NFS server flushes:
```rust
// In flush_buffer_to_cas() after meta update:
// This is already done by flush_wal() — verify WAL flush happens before release returns
self.meta.flush_wal().map_err(|_| libc::EIO)?;
```

### Step 5: Check FUSE-T version
Ensure FUSE-T 1.0.44+ is installed. Version 1.0.44 added a workaround for NFS Read/Write operations on closed file handles (ReOpen → Read/Write → Close pattern).

---

## State of the Art

| Old Approach | Current Approach | When Changed | Impact |
|--------------|------------------|--------------|--------|
| macFUSE (kext) on macOS | FUSE-T (kext-less, NFS-based) | 2022 | No System Integrity Protection issues; works on Apple Silicon without kext approval |
| `macos-13` GitHub Actions runner | `macos-14` (pinned), `macos-15` (GA April 2025) | April 2025 | `macos-13` deprecated Dec 2025; pin to `macos-14` now |
| Per-command `--json` flag | Global `--json` with `global = true` | clap 3+ | Single flag works on all subcommands |
| `actions/checkout@v3` | `actions/checkout@v4` | 2023 | v3 deprecated; use v4 |
| pjdfstest (original C version) | saidsay-so/pjdfstest (Rust rewrite, GSoC 2022) | 2022 | More configurable; TOML skip list; same test coverage |

**Deprecated/outdated:**
- `osxfuse` / `macFUSE` kext: requires disabling SIP on Apple Silicon, cannot load in GitHub Actions runners
- `actions/checkout@v3`: deprecated, use `@v4`
- `macos-13` runner: deprecated October 2025, removed December 2025

---

## Open Questions

1. **Does `direct_io` mount flag fully resolve the FUSE-T write bug?**
   - What we know: FUSE-T issue #45 identifies NFS client caching as root cause; `direct_io` bypasses page cache
   - What's unclear: Whether `direct_io` breaks other FUSE-T behaviors (large file performance, mmap); whether it's compatible with the write buffer approach in `SliceFsFilesystem`
   - Recommendation: Test `direct_io` as first approach; document the finding in BENCHMARKS.md regardless

2. **Should pjdfstest run in Linux CI as a compliance gate?**
   - What we know: GitHub Actions Linux runners support FUSE (`sudo apt-get install fuse`); pjdfstest requires root for some tests
   - What's unclear: Whether `fusermount` is available without SID on Ubuntu runners; whether in-process POSIX tests are sufficient
   - Recommendation: Start with in-process tests only; add pjdfstest with `sudo` as a follow-up if needed; keep the >95% gate as a target not a hard gate for Phase 7

3. **Is `physical_bytes` derivable from segment files alone?**
   - What we know: Dictionary entries are 92 bytes each; blockset content is stored as separate files in `vt0/` subdirectory
   - What's unclear: Whether walking `vt0/` directory to sum file sizes is practical (may be millions of files)
   - Recommendation: Report `dict.len() * 92` as `dictionary_metadata_bytes` and use an estimated physical size; document the approximation clearly

4. **macOS POSIX test suite scope on CI vs manual**
   - What we know: Custom POSIX tests in `posix_compliance_tests.rs` bypass FUSE (in-process); live mount tests require FUSE-T running
   - What's unclear: How many macOS-specific behaviors (resource forks, `.DS_Store`) need live mount testing vs can be unit tested
   - Recommendation: In-process tests in CI; live mount POSIX validation is a developer checklist item

---

## Validation Architecture

### Test Framework
| Property | Value |
|----------|-------|
| Framework | Rust built-in `cargo test` |
| Config file | `Cargo.toml` per crate (workspace) |
| Quick run command | `cargo test -p slicefs-cli --test posix_compliance_tests` |
| Full suite command | `cargo test --workspace` |

### Phase Requirements → Test Map
| Req ID | Behavior | Test Type | Automated Command | File Exists? |
|--------|----------|-----------|-------------------|-------------|
| PLAT-01 | macOS build succeeds with FUSE-T | smoke (build) | `PKG_CONFIG_PATH=.pkgconfig cargo build -p slicefs-cli` | ✅ build.rs Wave 0 |
| PLAT-01 | macOS in-process POSIX tests pass | unit | `cargo test -p slicefs-cli --test posix_compliance_tests` | ✅ existing |
| PLAT-01 | FUSE-T write path: writes visible on read | manual-only | Manual: mount + write + read on macOS | ❌ manual checklist |
| PLAT-03 | Windows deferred — no test needed | N/A | N/A | N/A (deferred) |
| PLAT-04 | CI runs on both platforms | smoke (CI) | GitHub Actions on push | ❌ Wave 0 (new .yml files) |
| PLAT-04 | pjdfstest compliance >95% on Linux | integration | `cargo test --test posix_compliance_tests` (in-process); pjdfstest manual | partial (in-process exists) |
| CLI-03 | `slicefs stats` outputs dedup ratio, bytes, block count | unit | `cargo test -p slicefs-cli --test stats_tests` | ❌ Wave 0 |
| CLI-03 | `slicefs stats --json` outputs valid JSON | unit | `cargo test -p slicefs-cli --test stats_tests -- --json` | ❌ Wave 0 |
| CLI-04 | `slicefs scrub` reports corruption, exits non-zero | unit | `cargo test -p slicefs-cli --test scrub_tests` | ❌ Wave 0 |
| CLI-04 | `slicefs scrub` exits 0 for clean store | unit | `cargo test -p slicefs-cli --test scrub_tests` | ❌ Wave 0 |
| CLI-05 | `--json` flag available on all subcommands | unit | `cargo test -p slicefs-cli` (cli.rs tests) | ❌ Wave 0 |
| CLI-05 | JSON output is valid JSON for each command | unit | `cargo test -p slicefs-cli --test json_output_tests` | ❌ Wave 0 |

### Sampling Rate
- **Per task commit:** `cargo test -p slicefs-cli`
- **Per wave merge:** `cargo test --workspace`
- **Phase gate:** Full suite green; macOS build succeeds; manual FUSE-T write test passes

### Wave 0 Gaps
- [ ] `crates/slicefs-cli/build.rs` — macOS rpath, covers PLAT-01 build ergonomics
- [ ] `.github/workflows/ci.yml` — Linux CI, covers PLAT-04
- [ ] `.github/workflows/macos.yml` — macOS CI, covers PLAT-04
- [ ] `crates/slicefs-cli/tests/stats_tests.rs` — covers CLI-03
- [ ] `crates/slicefs-cli/tests/scrub_tests.rs` — covers CLI-04
- [ ] `crates/slicefs-cli/tests/json_output_tests.rs` — covers CLI-05

---

## Sources

### Primary (HIGH confidence)
- FUSE-T issue #45 (macos-fuse-t/fuse-t) — confirmed NFS client caching is root cause of write-then-read data corruption
- FUSE-T wiki (macos-fuse-t/fuse-t/wiki) — "Caching of attributes is done by the NFS client. Currently the caching attributes returned by the filesystem implementation are ignored."
- FUSE-T release notes v1.0.44 — documented workaround for NFS Read/Write on closed file handles
- clap docs.rs — `global = true` attribute on `#[arg]` for cross-subcommand flags
- GitHub Actions changelog — macOS 15 GA April 10, 2025; `macos-latest` migrates to macOS 15 August 2025
- DictMetadataStore source (`/crates/metadata/src/store.rs`) — confirmed `logical_bytes()`, `refcounts`, `list_snapshots()`, `snapshot_roots()` APIs
- SliceFsFilesystem source (`/crates/slicefs-cli/src/filesystem.rs`) — confirmed write buffer pattern, `flush_buffer_to_cas()` shared helper
- GC module source (`/crates/metadata/src/gc/mod.rs`) — confirmed `collect_live_set()` reusable for stats per-snapshot analysis
- Snapshot CLI source (`/crates/slicefs-cli/src/snapshot.rs`) — established offline pattern for new commands (mount.lock check, load_store_from_segments, WAL)

### Secondary (MEDIUM confidence)
- FUSE-T Homebrew formula — FUSE-T available as `brew install fuse-t` (no kext, works on GH Actions runners)
- FUSE-T issue #96 — `direct_io` and `max_readahead` not working as expected on FUSE-T (relevant context for write fix investigation)
- saidsay-so/pjdfstest README — TOML configuration, skip lists, test categories
- JuiceFS fio documentation — fio job parameters for sequential write, read, small files benchmarks

### Tertiary (LOW confidence)
- Community reports that `direct_io` mount flag may resolve FUSE-T write visibility issues — not verified with a working example for this specific codebase

---

## Metadata

**Confidence breakdown:**
- FUSE-T write bug root cause: MEDIUM — confirmed via upstream issues but fix approach (direct_io) is hypothesis pending testing
- Standard stack: HIGH — all libraries already in use in the project; FUSE-T via Homebrew confirmed
- CLI stats/scrub architecture: HIGH — based on direct source code reading of existing patterns
- GitHub Actions CI: HIGH — official docs confirm macOS 14/15 runner availability; FUSE-T Homebrew install confirmed
- Pitfalls: HIGH for NFS caching; MEDIUM for pjdfstest requirements; HIGH for macos-latest deprecation

**Research date:** 2026-03-29
**Valid until:** 2026-06-29 (FUSE-T and GitHub Actions runner info may change; 90 days reasonable for stable ecosystem)
