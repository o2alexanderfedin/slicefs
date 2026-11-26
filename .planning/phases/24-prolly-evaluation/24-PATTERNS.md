# Phase 24: Prolly evaluation - Pattern Map

**Mapped:** 2026-11-26
**Files analyzed:** 14 new, 3 modified
**Analogs found:** 15 / 17 (all analog paths are git-tracked)

## File Classification

| New/Modified File | Role | Data Flow | Closest Analog | Match Quality |
|-------------------|------|-----------|----------------|---------------|
| `crates/prolly/Cargo.toml` | config | n/a | `crates/chunk-store/Cargo.toml`, `benchmarks/index-bench/Cargo.toml` | role-match |
| `crates/prolly/src/lib.rs` (docs, constants, exports) | library root | n/a | `crates/chunk-store/src/file.rs` lines 1-48 (module doc + consts with one comment each) | role-match |
| `crates/prolly/src/node.rs` (encode/decode) | model/codec | transform | `crates/chunk-store/src/file.rs` lines 50-100 | exact |
| `crates/prolly/src/splitter.rs`, `cdf.rs` | utility | transform | `crates/chunk-store/src/file.rs` `is_cut` (lines 20-25) | partial (rule differs) |
| `crates/prolly/src/build.rs` (streaming build) | service | streaming | `FileWriter::push/finish` in `crates/chunk-store/src/file.rs` | role-match |
| `crates/prolly/src/apply.rs`, `cursor.rs` | service | CRUD (batch edit, read path) | `FileReader` / `load_node` in `file.rs` | partial |
| `crates/prolly/src/tests.rs` | test | request-response | `file.rs` `mod tests` (line 750 on) | exact |
| `Cargo.toml` (workspace members) | config | n/a | existing members list (lines 1-10) | exact |
| `benchmarks/prolly-bench/Cargo.toml` | config | n/a | `benchmarks/index-bench/Cargo.toml` | exact |
| `benchmarks/prolly-bench/src/main.rs` | CLI bench binary | batch, file-I/O | `crates/chunk-store/examples/chunk_profile_bench.rs`, `benchmarks/index-bench/src/main.rs` | exact |
| `scripts/prolly-bench/bench.sh` | script | batch | `scripts/chunk-profiles/bench.sh` | exact |
| `scripts/prolly-bench/README.md` | doc | n/a | `scripts/chunk-profiles/README.md` | exact |
| `scripts/docker-linux-test/run.sh` (optional `prolly-smoke` case) | script | batch | `chunk-profiles-smoke` case (lines 73-85) | exact |
| `docs/design/unsafe-review.md` (modified) | doc/gate | n/a | its own tool entries (lines 649-655) and Counts (673+) | exact |
| `docs/research/prolly-trees.md` | doc | n/a | `docs/research/chunk-profiles.md` | exact |
| `docs/design/metadata-maps.md` | doc | n/a | `docs/design/write-log.md` / `chunk-store.md` (style only) | partial |
| `.planning/phases/24-prolly-evaluation/24-ISSUE-19-COMMENT.md` | doc | n/a | none (plain text) | none |

## Pattern Assignments

### `crates/prolly/src/node.rs` (codec, transform)

**Analog:** `crates/chunk-store/src/file.rs` (manifest node)

**Imports** (lines 3-7):
```rust
use crate::{ChunkStore, Error, ObjectId, Result, DIGEST_LEN, ID_LEN};
use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;
```
In `crates/prolly` use `chunk_store::{ChunkStore, Error, ObjectId, Result, ID_LEN}` (all `pub`).

**Header and encode** (lines 33-67): 4-byte magic, level `u8`, count `u32` LE = 9-byte header, then fixed entries.
```rust
const NODE_MAGIC: &[u8; 4] = b"SFM1";
fn encode_node(level: u8, entries: &[Entry]) -> Vec<u8> {
    let mut b = Vec::with_capacity(9 + entries.len() * ENTRY);
    b.extend_from_slice(NODE_MAGIC);
    b.push(level);
    b.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    ...
```
**Decode returns `Option`/error, never panics** (lines 77-100): check `b.len() < 9 || &b[..4] != NODE_MAGIC`, check the exact length against the count, use `checked_add`. For the prolly node, entries are variable length (`klen u16, vlen u16`), so every offset must be bounds-checked, keys must be verified sorted, and a decode failure maps to `Error::Corrupt(..)` (the variant file.rs tests expect). Use a different magic (for example `b"SPT1"`) so a prolly node is never read as a manifest node. Use ASCII only, no `unsafe` token anywhere in `crates/prolly`, comments included (gate counts the word).

### `crates/prolly/src/splitter.rs` / `cdf.rs` (utility, transform)

**Analog:** `file.rs` lines 10-25, constants each with one doc comment, one pure function:
```rust
/// A node ends after about one entry in this many (a power of two).
pub const NODE_CUT_ONE_IN: u32 = 512;
fn is_cut(id: &ObjectId) -> bool {
    let w = u32::from_le_bytes(id.digest()[DIGEST_LEN - 4..].try_into().unwrap());
    w & (NODE_CUT_ONE_IN - 1) == 0
}
```
Copy the style: one doc comment per constant (`NODE_MIN`, `NODE_MAX`, `WEIBULL_L`, `WEIBULL_K`, `MAX_ENTRY`, `FORCE_AT`), one pure `fn` that build and apply both call. The rule itself (Weibull, per-level salt, key hash `blake3::hash(&[level+1] ++ key)` first 4 bytes LE, integer `delta`, basic-ops-only `exp`) comes from RESEARCH.md "Boundary rule"; no codebase analog exists for it.

### `crates/prolly/src/build.rs`, `apply.rs`, `cursor.rs` (service)

**Analog:** `FileWriter::push(store, level, entry)` / `finish(store)` in `file.rs` (one in-progress node per level, cut then push the child entry to the parent level) and `FileReader::open` / `load_node(store, id)` (read one node by ID). Read `file.rs` lines 100-400 for `FileWriter` before writing `build.rs`; the shape (`levels: Vec<..>`, `push` cascades upward, `finish` flushes from level 0 and returns the root ID) is what D-16 asks for. Store access is `ChunkStore::put(&bytes) -> Result<ObjectId>` and `read(&id) -> Result<Vec<u8>>` (hash-verified). Do not use `store_io::put_value` (FastCDC re-cut).

### `crates/prolly/src/tests.rs` (test)

**Analog:** `file.rs` tests (line 750 on).

**Store setup** (lines 752-755, 772):
```rust
use crate::mem::MemIo;
let s = ChunkStore::open_with(std::sync::Arc::new(MemIo::new()), Options::default()).unwrap();
```
In the prolly crate: `chunk_store::mem::MemIo` and `chunk_store::Options` (`pub mod mem;` at lib.rs:44).

**Malformed input is an error, not a panic** (lines 767-780): build crafted nodes with `s.put(&encode_node(..))`, call the reader inside `std::panic::catch_unwind(AssertUnwindSafe(..))`, assert `matches!(got, Ok(Err(Error::Corrupt(_))))`. Copy for: wrong magic, wrong count, unsorted keys, child at wrong level.

**Node-size spread / limits test model** (file.rs lines 785-800): `fn fake(n)` makes entries from `Blake3_256.id(&i.to_le_bytes())`; big counts run under `#[cfg_attr(miri, ignore = "...")]`. Keep tests small (D-06): use a small test-config or a few thousand entries. Required tests (D-13): same set via different insert orders and bulk-vs-batches gives same root; same-length value change moves no boundary; no node above `NODE_MAX`, none below `NODE_MIN` except the last of a level; golden root hex; canonical roots for 0, 1, 2 keys; high-water-bytes bound for D-02. `proptest` and `tempfile` are workspace dev-deps (cases 32-64).

### `crates/prolly/Cargo.toml` (config)

**Analogs:** `benchmarks/index-bench/Cargo.toml` (whole file, below) and `crates/chunk-store/Cargo.toml` (`workspace` deps style).
```toml
[package]
name = "index-bench"
version = "0.1.0"
edition = "2021"
publish = false
description = "Measure the chunk store index: open time, RSS, lookups, recovery (#66)"

[dependencies]
chunk-store = { path = "../../crates/chunk-store" }
libc.workspace = true

[dev-dependencies]
tempfile.workspace = true
```
For `prolly`: deps `chunk-store = { path = "../chunk-store" }` and `blake3.workspace = true`; dev-deps `tempfile.workspace = true`, `proptest.workspace = true`. Verify D-19 with `cargo tree -p metadata | grep prolly` and the same for `slicefs-cli` (must print nothing).

### Workspace `Cargo.toml` (modified, one plan per wave)

Members list (lines 2-10) is flat paths; add `"crates/prolly"` (wave 1) and `"benchmarks/prolly-bench"` (wave 2) as separate lines. `Cargo.lock` changes in the same plan.

### `benchmarks/prolly-bench/src/main.rs` (bench binary, batch)

**Analogs:** `benchmarks/index-bench/src/main.rs` (helpers) and `crates/chunk-store/examples/chunk_profile_bench.rs` (subcommand shape, `gen tree` at line 226, `files_of` at 285, `main` at 178).

**CPU and RSS helpers to copy** (`index-bench/src/main.rs` lines 156-173); these are the only `unsafe` in the bench:
```rust
fn cpu_secs() -> f64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let t = |tv: libc::timeval| tv.tv_sec as f64 + tv.tv_usec as f64 / 1e6;
    t(ru.ru_utime) + t(ru.ru_stime)
}
fn max_rss_bytes() -> u64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let unit = if cfg!(target_os = "macos") { 1 } else { 1024 };
    (ru.ru_maxrss.max(0) as u64) * unit
}
```
Do not copy `current_rss_bytes` (task_info) unless needed; every `unsafe` token counts (see unsafe-review below).

**On-disk size loop** (`chunk_profile_bench.rs` lines 384-391; uses `std::os::unix::fs::MetadataExt`):
```rust
for p in files_of(&dir)? {
    let md = fs::symlink_metadata(&p)?;
    apparent += md.len();
    alloc += md.blocks() * 512;
    files += 1;
}
```
**Output convention:** one line per record, `word key=value ...`, parsed by the shell script with its `kv` awk helper (`println!("open open_ms={open_ms:.3} ... apparent_bytes={apparent} alloc_bytes={alloc} files={files}")`). Progress goes to stderr, CSV to stdout. Machine info is written by the shell script, never the binary (no path, host or user). Read `chunk_profile_bench.rs` lines 178-260 once for the arg parsing and `gen tree` (xorshift, 16 subdirs `dNN/fNNNNNN.bin`, `--seed`, `--change-every`) before copying.

### `scripts/prolly-bench/bench.sh` (script, batch)

**Analog:** `scripts/chunk-profiles/bench.sh` (copy structure, rename vars). Key excerpts:

Header usage block + `set -euo pipefail`, `usage()`, `HERE/REPO/OS`, `SMOKE`, `D=${1:-}; N=${2:-${RUNS:-3}}`; smoke refuses a data dir:
```bash
if [ "$SMOKE" = 1 ]; then [ -z "$D" ] || usage; else [ -n "$D" ] || usage; fi
command -v python3 > /dev/null || { echo "python3 is needed (the median rows)" >&2; exit 2; }
```
Work dir safety (store lives in a dir the script made with `mktemp -d`, `trap 'rm -rf "$STORE_DIR"' EXIT`, refuse `WORK=/`). Build default:
```bash
(cd "$REPO" && cargo build --release -p prolly-bench >&2)
BIN=${CARGO_TARGET_DIR:-$REPO/target}/release/prolly-bench
```
(chunk-profiles uses `--example chunk_profile_bench` and `target/release/examples/`; prolly-bench is a package binary, so no `examples/`.)

SMOKE data: `"$BIN" gen tree --dir "$D/x/boost_$1" --files N --seed 1` then `... --seed 1 --change-every 10`, `BOOST_VERS=${BOOST_VERS:-0_0_1 0_0_2}`, `nodata()` message pointing at `scripts/macos-mount-test/fetch-boost.sh`, page-cache warm (`find "$s" -type f -exec cat {} +`).

Peak RSS per process:
```bash
TIME_OPT=
if [ -x /usr/bin/time ]; then
  if [ "$OS" = Darwin ]; then TIME_OPT=-l
  elif /usr/bin/time -v true > /dev/null 2>&1; then TIME_OPT=-v; fi
fi
peak_rss() { case $TIME_OPT in
  -l) awk '/maximum resident set size/ {print $1; exit}' "$1" ;;
  -v) awk -F: '/Maximum resident set size/ {gsub(/ /, "", $2); printf "%.0f\n", $2 * 1024; exit}' "$1" ;;
  *) echo NA ;; esac; }
```
Run each mode (`whole`, `flat`, `prolly`) as its own `/usr/bin/time "$TIME_OPT" -o "$tf" "$BIN" <mode> ...` process, then `kv` to read fields. Median rows by the embedded Python (`statistics.median_low`, `NA` stays `NA`, run rows filtered by key columns, `r[2] not in ("run","median")`); adjust column range `range(3, 28)` to the new header. Machine info block (Darwin: `sysctl -n machdep.cpu.brand_string`, `hw.ncpu`, `hw.memsize`; Linux: `/proc/cpuinfo`, `nproc`, `/proc/meminfo`) writes `os arch cpu cpus mem_bytes fs rustc commit page_cache load_before load_after start_utc end_utc` to `$WORK/machine.txt`. Copy it verbatim (it already has no path, host or user). Extra env per RESEARCH: `KS`, `COMMITS`, `PATTERNS`.

### `scripts/docker-linux-test/run.sh` (optional `prolly-smoke` case)

**Analog:** `chunk-profiles-smoke` case, lines 73-85 (docker run, no FUSE, no `RUN`):
```bash
  chunk-profiles-smoke)
    docker run --rm -v "$REPO":/src:ro -v "${SLICEFS_TARGET_VOL:-slicefs-target}":/target -v slicefs-cargo:/usr/local/cargo/registry \
      -e CARGO_TARGET_DIR=/target slicefs-fusetest bash -c '
        rustc -V
        timeout 1200 cargo build -q --release --locked -j 1 -p chunk-store --example chunk_profile_bench || exit 1
        echo "# load before: $(cat /proc/loadavg)"
        SMOKE=1 RUNS=1 PROFILES="4/8/32 16/64/256" WORK=/tmp/cp BIN=/target/release/examples/chunk_profile_bench \
          timeout 600 bash /src/scripts/chunk-profiles/bench.sh || exit 1
        ...
        cat /tmp/cp/machine.txt' ;;
```
Copy as `prolly-smoke`: `-p prolly-bench`, `BIN=/target/release/prolly-bench`, `WORK=/tmp/pp`, script path `scripts/prolly-bench/bench.sh`. Add a short comment line like the others and a matching entry wherever the usage/case list is documented in that file (check the header of `run.sh` for a mode list).

### `docs/design/unsafe-review.md` (modified; gate-critical)

Gate: `scripts/safety/unsafe-count.sh` counts `grep -ow unsafe` per `.rs` file under `crates`, `benchmarks`, `vendor/fuser`, compared with sorted `- count: <path> <n>` lines (673-703). A new `benchmarks/prolly-bench/src/main.rs` with `unsafe` needs both of:

1. A bullet in the by-hand tools list (lines 649-655), same style:
```
- `benchmarks/index-bench/src/main.rs`: zeroed `rusage` and `getrusage`
  (three places), ...
```
New bullet: ``- `benchmarks/prolly-bench/src/main.rs`: a zeroed `rusage` and `getrusage` (two functions).``
2. A count line inserted in sorted order after `benchmarks/index-bench/src/main.rs 11`:
```
- count: benchmarks/prolly-bench/src/main.rs N
```
`N` = number of `unsafe` words in the file (4 for the two helpers above, plus any in comments or in `#![...]`; recount with `grep -ow unsafe file | wc -l`). `crates/prolly/**` gets no line because it must have zero. Same plan that adds the bench source edits this doc.

### `docs/research/prolly-trees.md` (doc)

**Analog:** `docs/research/chunk-profiles.md` layout: H1 with issue number; intro paragraph naming plan, run date, data files under `.planning/phases/NN/runs/` and that every table number is a median row value; then `## Summary` (recommendation first, bullets), `## Setup` (machine from machine.txt, inputs, commands), `## Results` (subsections with tables), `## Trade-offs`, `## Recommendation`, `## Reproduce`. The Reproduce block form:
```sh
scripts/macos-mount-test/fetch-boost.sh DIR 1_84_0 1_85_0
cargo build --release -p chunk-store --example chunk_profile_bench
WORK=WORKDIR BIN=... timeout 5400 bash scripts/chunk-profiles/bench.sh DIR 3 > results.csv
```
For prolly: `cargo build --release -p prolly-bench`, `bash scripts/prolly-bench/bench.sh DIR 3`. Add sections the Phase 22 doc lacks: "Boundary rule" (constants table from RESEARCH), "Rust crate survey", "File manifests are already content-cut trees" (cite `file.rs` `NODE_MIN` 64, `LEAF_ENTRIES` 4096, `NODE_CUT_ONE_IN` 512, entries with byte lengths), "What adoption would change" (D-28). No licensing words, no home paths.

### `scripts/prolly-bench/README.md`

**Analog:** `scripts/chunk-profiles/README.md` (inputs, columns, notes). Copy its headings; list env vars, CSV columns and the three modes.

## Shared Patterns

### Store handle in tests and benches
**Source:** `crates/chunk-store/src/file.rs` line 772; bench uses `ChunkStore::open(&dir, Options{..Options::default()})` as in `chunk_profile_bench.rs` line 383.
**Apply to:** prolly tests (MemIo), bench `flat` and `prolly` modes (scratch dir store).
```rust
ChunkStore::open_with(Arc::new(MemIo::new()), Options::default()).unwrap()
```
Bytes-written metric: `store.stats()` before and after (`Stats { objects, raw_bytes, stored_bytes, packs }`), record bytes = `stored_bytes + 42 * objects`.

### Errors never panic on bad bytes
**Source:** `file.rs` `decode_node` returns `Option`, `load_node` maps to `Error::Corrupt`; tested by `a_malformed_tree_is_an_error_not_a_panic`.
**Apply to:** `crates/prolly` node decode, cursor, `get`, `apply`.

### Shell scripts
**Source:** `scripts/chunk-profiles/bench.sh`: `set -euo pipefail`, progress to stderr, results to stdout and `$WORK/results.csv`, work under `mktemp -d`, refuse bad paths, `NA` for unavailable metrics.
**Apply to:** `scripts/prolly-bench/bench.sh`, the Docker target.

### Reopen a real store (bench `whole` mode)
**Source:** `crates/slicefs-cli/src/snapshot.rs` lines 63-72 (per RESEARCH: `metadata::segment::load_store_from_segments`, `slicefs_cli::content::store_io`, `DictMetadataStore::load_from_root`); seed via `slicefs_cli::seed::run_seed_with`. Read `snapshot.rs` 55-80 before writing the bench `whole` mode; copy the three calls, do not change `metadata` (D-01).

### Test-time budget and gating
**Apply to:** all new tests: short, no mount, bench never in `cargo test` (D-06); only build the bench and an optional tiny smoke unit test.

## No Analog Found

| File | Role | Data Flow | Reason |
|------|------|-----------|--------|
| Weibull cut rule (`cdf.rs`) | utility | transform | Nothing like a size-aware boundary rule exists; `is_cut` is a fixed bit mask. Use RESEARCH.md "Boundary rule" (deterministic integer `delta`, basic-ops `exp`). |
| `apply.rs` re-sync merge (rewrite until a new cut lands on an old node end) | service | batch | `file.rs` only has append-style `FileWriter` plus a "shift changes few nodes" test; no in-place path rewrite exists. Use RESEARCH.md "apply(batch)". |
| `24-ISSUE-19-COMMENT.md` | doc | n/a | Plain text for the issue; no template. No `gh` write command (D-27). |

## Metadata

**Analog search scope:** `crates/chunk-store/src`, `crates/chunk-store/examples`, `benchmarks/`, `scripts/chunk-profiles`, `scripts/docker-linux-test`, `docs/design`, `docs/research`, workspace `Cargo.toml`
**Files read:** about 9 (targeted ranges)
**Pattern extraction date:** 2026-11-26
