# Phase 22: Chunk profiles - Pattern Map

**Mapped:** 2026-11-25
**Files analyzed:** 16 new or modified
**Analogs found:** 15 / 16

All analog paths are git-tracked source (checked with `git ls-files` for the scripts, docs and stats file; the crate sources are tracked Rust files).

## Constant and call sites to replace (D-13)

From `grep -rnE "CHUNK_MIN|CHUNK_AVG|CHUNK_MAX|chunk_ranges|zero_chunk" crates`:

| File | Lines | Use |
|------|-------|-----|
| `crates/chunk-store/src/lib.rs` | 108-118 | the constants and `chunk_ranges` (keep as default-profile helpers) |
| `crates/chunk-store/src/file.rs` | 27 (import), 136 (writer buffer `2 * CHUNK_MAX`), 156 (`cut`: `chunk_ranges`), 238-245 (`zero_chunk`), 307 (reader zero path), 559-575 (`known_zero`), 671-672 and 691-729 (splice: max, zeros, step `CHUNK_AVG`, `chunk_ranges`) | non-test uses: all must read the store profile |
| `crates/chunk-store/src/file.rs` (tests) | 942, 1042, 1066-1072, 1200, 1258-1260 | tests: use the default profile or loop over profiles |
| `crates/slicefs-cli/src/content.rs` | 373 (test) | `chunk_store::CHUNK_MAX` in a test |
| `crates/slicefs-cli/tests/write_path_tests.rs` | 503 | `chunk_ranges(&old_model)` in a test |
| `crates/chunk-store/tests/store_tests.rs` | 3, 178, 193 | tests |
| `crates/chunk-store/tests/file_tests.rs` | 67, 136-165 | tests ("32 zero chunks of CHUNK_MAX") |
| `crates/slicefs-cli/src/filesystem.rs` | 2578 (comment), ~7017 (`8 * 32768`) | stays valid (default profile) |

`crates/metadata/src/store_io.rs:62` uses `FileWriter::default()` and passes the store, so it needs no change (D-12).

## File Classification

| New/Modified File | Role | Data Flow | Closest Analog | Match Quality |
|-------------------|------|-----------|----------------|---------------|
| `crates/chunk-store/src/lib.rs` (`ChunkProfile`, `Options.profile`, `ChunkStore::profile()`) | model/config | transform | same file: `CHUNK_*`, `chunk_ranges`, `Options.hasher` (lines 105-135, 184-191) | exact |
| `crates/chunk-store/src/file.rs` (constant reads, per-store zero chunk) | service | streaming + CRUD | same file: `zero_chunk` (238-245), `known_zero` (559-580) | exact |
| `crates/chunk-store/tests/` golden cut-point test (new file, for example `profile_tests.rs`) | test | transform | `crates/chunk-store/tests/store_tests.rs` (lines 178, 193) | role-match |
| `crates/chunk-store/tests/` two-store test (same new file) | test | CRUD | `crates/chunk-store/tests/file_tests.rs` | role-match |
| `crates/chunk-store/src/file.rs` tests (`zero_runs_cut_at_chunk_max` per profile) | test | transform | same file, lines 1066-1072 | exact |
| `crates/slicefs-cli/src/header.rs` (`chunks` line, `chunk-profile` flag) | model/parser | request-response | same file: `Line::Hash`, `parse`, `KNOWN_FLAGS`, `marker`, `create_with` | exact |
| `crates/slicefs-cli/src/content.rs` (profile from header at the four open points, `--chunk-profile` check) | service | request-response | same file: `hasher_of`, `check_write`, `select_hash`, `open*` (40-157) | exact |
| `crates/slicefs-cli/src/cli.rs` (`--chunk-profile` on Seed, Mount) | config | request-response | same file: `--hash` arg (lines 93-96, 151) and its parse tests (293-310) | exact |
| `crates/slicefs-cli/src/seed.rs`, `mount.rs` (pass the profile) | controller | request-response | same files: `hash` parameter (`seed.rs:36-42`, `mount.rs:58-111`) | exact |
| `crates/slicefs-cli/src/stats.rs` (profile line and `--json` field) | controller | request-response | same file (read it for the human and JSON form) | exact |
| `crates/slicefs-cli/src/header.rs` tests (header and refusal tests) | test | request-response | same file tests (lines 581-806) | exact |
| `crates/chunk-store/examples/chunk_profile_bench.rs` (new) | utility | batch | none in `chunk-store` (no `examples/` dir) | no analog |
| `scripts/chunk-profiles/bench.sh` + `README.md` (new) | utility | batch | `scripts/pack-measure/seed-measure.sh`, `scripts/pack-measure/README.md` | role-match |
| `docs/research/chunk-profiles.md` (new report) | docs | n/a | `docs/research/arena-spikes.md`, `docs/research/nvme2.md` | role-match |
| `docs/design/store-format.md`, `docs/design/chunk-store.md`, README | docs | n/a | same files | exact |
| `docs/design/unsafe-review.md` (only if unsafe count changes) | docs | n/a | itself; `scripts/safety/unsafe-count.sh` | exact |

## Pattern Assignments

### `crates/chunk-store/src/lib.rs` (model, transform)

**Analog:** same file. Constants and `chunk_ranges` (lines 105-118):
```rust
pub const CHUNK_MIN: u32 = 4 * 1024;
pub const CHUNK_AVG: u32 = 8 * 1024;
pub const CHUNK_MAX: u32 = 32 * 1024;

pub fn chunk_ranges(data: &[u8]) -> Vec<Range<usize>> {
    fastcdc::v2020::FastCDC::new(data, CHUNK_MIN, CHUNK_AVG, CHUNK_MAX)
        .map(|c| c.offset..c.offset + c.length)
        .collect()
}
```
Plan: `ChunkProfile { min, avg, max }` with `DEFAULT`, a validating `new` (powers of two, min < avg < max, fastcdc limits, zero check from D-16) and `ranges(&self, &[u8])` using the same `FastCDC::new(data, min, avg, max)` call. Keep `chunk_ranges` as `ChunkProfile::DEFAULT.ranges`.

**Options field pattern** (lines 135 and 191): a public field with a doc comment, and `Default` sets it (`hasher: DEFAULT_HASHER`). Add `pub profile: ChunkProfile` (default `ChunkProfile::DEFAULT`) and `ChunkStore::profile()` (store struct at line 852, `open_with` at 1342). Tests build `Options { ..Options::default() }`, so a new field with a default does not break them.

### `crates/chunk-store/src/file.rs` (service, streaming)

**Analog:** same file. Replace the process-wide zero chunk (lines 235-245):
```rust
fn zero_chunk() -> &'static (Vec<ObjectId>, Vec<u8>) {
    static ZERO: OnceLock<(Vec<ObjectId>, Vec<u8>)> = OnceLock::new();
    ZERO.get_or_init(|| {
        let z = vec![0u8; CHUNK_MAX as usize];
        (hashers().iter().map(|h| h.id(&z)).collect(), z)
    })
}
```
with the same body computed per store (or a cache keyed by `max`) and reached as `store.zero_chunk()`. The three users take `store` already: reader (line 307, `let (zids, zeros) = zero_chunk();`), `known_zero(store, ...)` (568), splice (672). Keep edits small (D-04): swap the constant for `store.profile().max` / `.avg` and `chunk_ranges(x)` for `store.profile().ranges(x)`.

**Constants to swap:** writer limit `if self.buf.len() >= 2 * CHUNK_MAX as usize` (136); `known_zero`'s `let e = s + CHUNK_MAX as u64;` and `x.len != CHUNK_MAX as u64` (559, 575); splice `let max = CHUNK_MAX as usize;` and `let mut step = CHUNK_AVG as u64;` (671, 691).

**Test to generalize** (lines 1066-1072):
```rust
#[test]
fn zero_runs_cut_at_chunk_max() {
    let max = CHUNK_MAX as usize;
    let lens: Vec<usize> = chunk_ranges(&vec![0u8; 4 * max + 100]).iter().map(|r| r.len()).collect();
    assert_eq!(lens, vec![max, max, max, max, 100]);
}
```
Loop it over the five profiles. Helpers in that test module: `fast_store()`, `Rng(..)`, `VecSource`.

### Golden and two-store tests (`crates/chunk-store/tests/`)

**Analog:** `crates/chunk-store/tests/store_tests.rs` (imports line 3: `use chunk_store::{chunk_ranges, Blake3_256, ChunkStore, ObjectHasher, ObjectId, Options};`) and `file_tests.rs`. Two stores: open two `ChunkStore::open_with(io, Options { profile, ..Options::default() })` over temp dirs, as the existing tests open stores. For the spliced-edit-key check, copy the "splice key equals fresh write key" shape from `a_splice_over_zeros_and_repeating_data_gives_the_same_key` (`file.rs` ~1074). Input generator: xorshift `Rng` in `file.rs` tests / `random_bytes(n, seed)` in `content.rs` tests (line ~373).

### `crates/slicefs-cli/src/header.rs` (parser, request-response)

**Analog:** same file. Flag list and marker (lines 28-35):
```rust
pub const KNOWN_FLAGS: &[&str] = &[];
pub fn marker(hasher: &dyn ObjectHasher) -> String {
    format!("{FORMAT}hash {}\n", hasher.name())
}
```
Add `"chunk-profile"` to `KNOWN_FLAGS`; add one release constant (placeholder `v2.2`); `marker` takes the profile and appends `chunks <min> <avg> <max>\n` and `ro_compat chunk-profile <release>\n` only for a non-default profile (D-09, D-10).

**Line kind** (lines 86-93, `to_text` 123-133): add `Line::Chunks(u32, u32, u32)` beside `Line::Hash(String)`, an accessor like `Header::hash()` (lines 105-111), and a `to_text` arm like `Line::Hash(name) => s.push_str(&format!("hash {name}\n"))`.

**Parse rule** (lines 307-313), copy for `chunks`:
```rust
("hash", _) if words.len() == 2 => {
    if lines.iter().any(|l| matches!(l, Line::Hash(_))) {
        return Err(bad("two hash lines"));
    }
    Line::Hash(words[1].to_string())
}
```
Add `("chunks", _) if words.len() == 4` with the same "two chunks lines" check and decimal parse; the rule "line without flag, flag without line" is a post-loop check (use `msg_bad_line` as at line 305). The match order matters: `chunks` must come before the `Line::Other` arm (line 324).

**Decision** (`decide`, lines 371-397): flags in `KNOWN_FLAGS` are skipped, so a build that knows `chunk-profile` opens it read-write; an old build sees an unknown ro_compat flag and reads only. No change to `decide` is needed beyond `KNOWN_FLAGS`.

**First header write:** `check_with(store, want, hasher)` (412-429) and `create_with(store, hasher)` (512-518, `write_tmp(store, &marker(hasher))`). Add the profile next to `hasher` (or a small `NewStore { hasher, profile }` struct). `check` (407) calls `check_with(.., DEFAULT_HASHER)`.

**Kani:** the byte-level core is `valid_name_bytes`, `valid_release_bytes`, `first_line_number`, `Class::from_word_bytes` (lines 331-364). The `chunks` parse works on `str` and is outside the proofs. If a new byte-level helper is added (for example a decimal parser), add a harness in `mod proofs` and run `scripts/safety/kani.sh` (D-05, D-23).

**Tests:** copy the style at lines 581-640 (`decide_text`, `HASH` const, `h.hash()` assertions) and the rewrite test at 794-806 (it uses a line `chunk-profile 16 64 256` as an unknown flag today: with `chunk-profile` now known, that test must change its sample line to a different unknown flag name).

### `crates/slicefs-cli/src/content.rs` (service, request-response)

**Analog:** same file. Header to option (lines 49-58, 119-124, 151-157):
```rust
fn hasher_of(store: &Path, h: Option<&Header>) -> std::io::Result<&'static dyn ObjectHasher> {
    let Some(name) = h.and_then(Header::hash) else { return Ok(DEFAULT_HASHER) };
    ...
}
pub fn open(store: &Path) -> std::io::Result<Content> {
    let opened = header::check(store, Want::Write)?;
    let hasher = hasher_of(store, opened.header.as_ref())?;
    let opts = Options { hasher, gc_tmp_dir: Some(store.join(GC_TMP_DIR)), ..Options::default() };
```
Add `profile_of(h: Option<&Header>) -> ChunkProfile` (absent line gives `DEFAULT`) and `profile` in both `Options` literals (`open`, `open_read_only`). `store_io` and `store_io_read_only` (137-146) call these, so they follow.

**Setting at seed or mount** (`check_write`, lines 88-109): the model for the new check. Parse the name first so a bad value changes nothing, call `header::check_with(.., new profile)` (a new store gets it in its first write), then compare with the recorded profile. Unlike `--hash`, a different profile returns an error naming both profiles and never rewrites the header. Parse function shape: `parse_hash` (lines 61-65) returns `Result<String, String>` for clap; the new `parse_chunk_profile` parses `MIN/AVG/MAX` in KiB and returns `ChunkProfile` (validated).

**ro_compat refusal:** copy `msg_ro_hash` (`header.rs:214-217`) for a `--chunk-profile` message.

### `crates/slicefs-cli/src/cli.rs`, `seed.rs`, `mount.rs`

**Analog:** the `--hash` option. `cli.rs` lines 93-96:
```rust
/// Hash of the IDs of new objects ...
#[arg(long, value_name = "NAME", value_parser = crate::content::parse_hash)]
hash: Option<String>,
```
Add `chunk_profile: Option<ChunkProfile>` with `value_parser = crate::content::parse_chunk_profile` in both Seed and Mount (`hash` appears at lines 96 and 151). Parse tests: lines 293-310 (`Cli::try_parse_from([...])`, accept and refuse). Threading: `run_seed(store_path, source_dir, hash)` (`seed.rs:36`) calls `content::check_write(store_path, hash)?` first; `mount.rs` has `hash: Option<&str>` at lines 63, 612, a `hash: Option<String>` config field at 337 and `hash.map(str::to_string)` at 678, then `select_hash(store_path, hash)?` at line 111 (under the mount lock, before `store_io`). Add the profile at each of those places, and fix each caller of `run_seed` and the mount config builder.

### `crates/slicefs-cli/src/stats.rs`

**Analog:** same file (read it before planning; it prints the human lines and the `--json` object). Read the profile with `header::read` + `parse` as `content::write_hasher` does (lines 43-46) and print `4/8/32 KiB` for no line (D-19).

### Benchmark: `scripts/chunk-profiles/bench.sh`, `README.md`

**Analog:** `scripts/pack-measure/seed-measure.sh` and `README.md`. Copy the header block (usage, env list, "stdout: CSV only; progress goes to stderr", rows also to `$WORK/results-*.csv`, median row), `set -euo pipefail`, `usage()`, the `WORK=$(mktemp -d)` default, and `[[ "$N" =~ ^[1-9][0-9]*$ ]]` argument checks. Differences: no mount, and it must also run on Linux (the analog needs macOS `stat -f` and `/usr/bin/time -l`, so use portable calls or a branch on `uname`). Inputs from `scripts/macos-mount-test/fetch-boost.sh`.

### `crates/chunk-store/examples/chunk_profile_bench.rs`

**No analog:** `chunk-store` has no `examples/` directory. Use the library API directly (`ChunkStore::open_with`, `FileWriter`, `FileReader`, `splice`) and the open pattern of `crates/chunk-store/tests/file_tests.rs`. Alternative: put it in `slicefs-cli` as a bin only if it needs the header; it does not (the profile goes in through `Options`).

### Docs

`docs/design/store-format.md`: add the `chunks` row to the grammar table, the `chunk-profile` row to the flag list, and an example header, in the style of the existing rows. `docs/design/chunk-store.md`: chunking section. `docs/research/chunk-profiles.md`: structure like the other `docs/research/*.md` reports.

## Shared Patterns

### A setting line always comes with a flag
**Source:** `docs/design/store-format.md` and 21.1 CONTEXT. **Apply to:** `header.rs` parse, marker and tests. A `chunks` line with no flag, or the flag with no line, is a bad header.

### Setting given at seed or mount, recorded before the store opens
**Source:** `crates/slicefs-cli/src/content.rs` `check_write` (88-109) and `select_hash` (71-79). **Apply to:** `--chunk-profile` in `seed.rs` and `mount.rs`.

### Options carry store-level behavior
**Source:** `Options.hasher` (`lib.rs:135`, `191`) and `hasher_of` (`content.rs:49`). **Apply to:** profile in `Options`, read at all four open points.

### Safety gates
**Source:** `scripts/safety/kani.sh`, `scripts/safety/unsafe-count.sh`. **Apply to:** any plan that edits `header.rs` byte-level helpers or adds `unsafe`.

## No Analog Found

| File | Role | Data Flow | Reason |
|------|------|-----------|--------|
| `crates/chunk-store/examples/chunk_profile_bench.rs` | utility | batch | No `examples/` in the repo crates; build on the library API and test helpers |

## Notes for the planner

- D-11 check: the reader zero path (`file.rs:307-311`) matches only `len == zeros.len()` with a zero ID, so an old build with a 32 KiB zero buffer reads a larger-profile chunk from the store. Chunk lengths come from the manifest entries. `known_zero` is a write-side function (used by splice). This agrees with `ro_compat`. Confirm when reading the code in the plan.
- The existing header test at `header.rs:794-806` uses the name `chunk-profile` as a sample unknown flag. It must change when the name becomes known.
- A per-store zero chunk costs one `max` buffer per store (up to 16 MiB at the crate maximum). A lazy init on first use keeps open cheap.

## Metadata

**Analog search scope:** `crates/chunk-store`, `crates/slicefs-cli`, `crates/metadata`, `scripts/pack-measure`, `docs/design`, `docs/research`
**Files scanned:** about 14
**Pattern extraction date:** 2026-11-25
