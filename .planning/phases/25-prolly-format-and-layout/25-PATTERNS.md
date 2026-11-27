# Phase 25: Prolly format and layout - Pattern Map

**Mapped:** 2026-11-26
**Files analyzed:** 17 (new or modified groups)
**Analogs found:** 15 / 17 (all analog paths are git-tracked; checked with `git ls-files` on the crates and bench dirs)

Line numbers are from `develop` at d405ee6. Row labels follow the plan split of 25-RESEARCH.md (25-01 .. 25-06).

## File Classification

| New/Modified File | Role | Data Flow | Closest Analog | Match |
|---|---|---|---|---|
| `crates/prolly/src/rule.rs` (modify: integer threshold, salted `boundary_hash`) | utility | transform | itself (`rule.rs` 1-120, float rule being replaced) | exact |
| `crates/prolly/src/rule_table.rs` (new, generated literals) | config | transform | none | no analog |
| `crates/prolly/src/lib.rs` (modify: `Config::new`, salt, `RULE_VERSION`, `READ_CFG`) | config | transform | itself (`Config` 24-100) | exact |
| `crates/prolly/src/node.rs` (modify: `NODE_HEADER` 10, version byte) | model | file-I/O | itself (header doc in lib.rs 1-30) | exact |
| `crates/prolly/src/tests.rs` (modify: re-pin goldens, error test) | test | transform | `golden_root` (tests.rs 42-72) | exact |
| `scripts/prolly-table/gen_table.py` (new) | utility | batch | none in repo (scripts are shell) | no analog |
| `crates/slicefs-cli/src/header.rs` (modify: format 8, M2 arms, salt line and flag) | model/config | request-response | `chunks` line plus `chunk-profile` flag in the same file | exact |
| `crates/slicefs-cli/src/cli.rs` (modify: `seed --boundary-salt`) | controller | request-response | `--chunk-profile` on `Seed` (cli.rs 102, 163, 322) | exact |
| `crates/slicefs-cli/src/seed.rs` (modify: `run_seed_with` takes salt) | service | request-response | `run_seed_with(.., profile)` (seed.rs 42-70) | exact |
| `crates/slicefs-cli/src/content.rs` (modify: `check_write_with` takes salt) | service | request-response | `check_write_with(.., profile)` | exact |
| `crates/slicefs-cli/src/stats.rs` and `tests/store_header_tests.rs` (modify: 7 to 8) | test | request-response | `a_format_6_store_is_refused` | exact |
| `crates/metadata/src/store.rs` (modify: drop `INODE_MAP`, `root_rec` offsets) | model | CRUD | itself (`root_rec` 38-52) | exact |
| `crates/metadata/src/inode_map.rs` (modify: allocator only) | service | CRUD | itself (`InodeMap` 15-70) | exact |
| `crates/metadata/src/rehash/{mod,map}.rs`, `crates/chunk-store/src/lock_stats.rs`, `crates/slicefs-cli/tests/lock_stats_report.rs` (modify) | utility | batch | self | exact |
| `benchmarks/prolly-bench/src/layout.rs` (new) and `replay.rs`, `main.rs` (modify) | utility | batch | `replay.rs` `batches`/`prolly`/`scale` modes | role-match |
| `benchmarks/prolly-bench/tests/smoke.rs` (modify) | test | batch | itself (`ok(&[..])` helper, lines 1-26) | exact |
| `docs/design/store-format.md`, `docs/user/upgrade.md` (stub, new) | doc | n/a | `store-format.md` (existing sections) | role-match |

## Pattern Assignments

### `crates/prolly/src/rule.rs` (utility, transform)

**Analog:** itself. The float rule is replaced in place; keep the `Splitter` shape so build and apply cannot diverge.

**Hash to extend** (lines 15-23). Unsalted keeps today's bytes; salted switches to keyed BLAKE3 (`Hasher::new_keyed(&[u8; 32])`, blake3 1.8.7):
```rust
pub fn boundary_hash(level: u8, key: &[u8]) -> u32 {
    let mut h = blake3::Hasher::new();
    h.update(&[level.wrapping_add(1)]);
    h.update(key);
    let d = h.finalize();
    let b = d.as_bytes();
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}
```
New signature adds `salt: Option<&[u8; 32]>` (or reads it from `Config`).

**Code to delete:** `exp_neg` (lines 25-48), `s4`, `cut_probability` (63-66). Keep `s4`-style exact `u64`/`u128` fourth-power math.

**Decision site to rewrite** (inside `Splitter::push`, lines ~100-106). Keep the ordering of the checks (internal node never ends after first entry, forced cut at `force_at()`, no cut below `node_min()`), replace only the last line:
```rust
if self.size > cfg.force_at() { return true; }
if self.size < cfg.node_min() { return false; }
(boundary_hash(level, key) as f64 / 4_294_967_295.0) < cut_probability(cfg, before, self.size)
```
with `(boundary_hash(..) as u64) < threshold(cfg, before, self.size)` (u64 compare; threshold in `[0, 2^32]`). Integer sketch is in 25-RESEARCH.md Pattern 1.

---

### `crates/prolly/src/lib.rs` and `node.rs` (config/model)

**Analog:** itself. `Config` (lib.rs 24-100) has private fields, `Copy + Eq`, a `DEFAULT` const, a `SMALL` const and accessors, and no constructor.

**Pattern to extend:**
```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config { node_min: usize, node_max: usize, max_entry: usize, max_key: usize, weibull_l: usize }
impl Config {
    pub const DEFAULT: Config = Config { node_min: NODE_MIN, node_max: NODE_MAX, max_entry: MAX_ENTRY, max_key: MAX_KEY, weibull_l: WEIBULL_L as usize };
    pub const SMALL: Config = Config { node_min: 32, node_max: 1_024, max_entry: 256, max_key: 128, weibull_l: 256 };
    pub fn force_at(&self) -> usize { self.node_max - self.max_entry }
}
```
Add `salt: Option<[u8; 32]>` (still `Copy + Eq`), a `Config::new(l, node_max, salt)`, and `RULE_VERSION: u8`. Pitfall 6: readers use `READ_CFG = Config::DEFAULT` (`tree.rs:306`); make it the largest writable config before the bench sweeps `NODE_MAX` = 32 KiB.

**Node header:** doc at lib.rs 3-10 says 9 bytes (`SPT1`, level `u8`, count `u32` LE). New: `SPT1`, `rule_version u8`, level `u8`, count `u32` = 10 bytes. Update the module doc, which also says "node bytes are private", because D-09 reverses that.

---

### `crates/prolly/src/tests.rs` (test, transform)

**Analog:** `golden_root` (lines 42-72): builds `u64_entries(5_000, 7)` on `mem_store()` with `Config::DEFAULT`, pins the root hex and `(depth, leaves)`, then pins a table of `(cfg, before, after)` probabilities and `boundary_hash` values.
```rust
let (root, st) = build(&s, Config::DEFAULT, u64_entries(5_000, 7)).unwrap();
assert_eq!(root.to_hex(), "1ee1cf56...");
assert_eq!((st.depth, st.leaves), (3, 58));
let bits: Vec<u64> = table.map(|(c, b, a)| cut_probability(&c, b, a).to_bits()).collect();
assert_eq!((boundary_hash(0, b""), boundary_hash(1, b"key")), (527_629_384, 94_351_618));
```
Replace the `to_bits` table with integer thresholds. Keep the f64 `exp` reference check only inside tests (existing lines 68-71 show the style). Add: table BLAKE3 golden, cut-point list, error test vs `f64`, monotonicity (proptest is already a dev-dependency), a salted golden root. Re-pin only in 25-06 (Pitfall 8).

---

### `crates/slicefs-cli/src/header.rs` (model, request-response)

**Analog:** the `chunks` line plus `chunk-profile` flag (a setting line that always comes with a ro_compat flag). Copy it for `boundary-salt`.

**Constants** (lines 32-46):
```rust
pub const CHUNK_PROFILE_FLAG: &str = "chunk-profile";
pub const CHUNK_PROFILE_RELEASE: &str = "v2.3";
pub const KNOWN_FLAGS: &[&str] = &[CHUNK_PROFILE_FLAG, HASH_SHA256_FLAG];
```
Add `BOUNDARY_SALT_FLAG`, `BOUNDARY_SALT_RELEASE = "v2.4"`; add to `KNOWN_FLAGS`. Tests use the constants, never the text.

**Writing the header** (`marker_with`, lines 70-80): a non-default setting appends the line then the flag line:
```rust
if profile != ChunkProfile::DEFAULT {
    s.push_str(&format!("chunks {} {} {}\n", profile.min(), profile.avg(), profile.max()));
    s.push_str(&format!("ro_compat {CHUNK_PROFILE_FLAG} {CHUNK_PROFILE_RELEASE}\n"));
}
```
Salt: `boundary-salt <64 lowercase hex>` then `ro_compat boundary-salt v2.4`, only when a salt is set (default header unchanged).

**Parse and pairing** (lines 137, 163, 349-420): `Line::Chunks(ChunkProfile)` variant, a `parse_chunks` word validator (lines 437-441, digits only, rejects anything else, so a bad line is M9 never a panic), a getter `chunk_profile()`, and the pair check:
```rust
let bad_at = |j: usize, reason: &str| refuse(msg_bad_line(store, j + 2, reason, pieces[j + 1]));
match (chunks_at, flag_at) {
    (Some(j), None) => return Err(bad_at(j, "chunks line with no chunk-profile flag")),
    (None, Some(j)) => return Err(bad_at(j, "chunk-profile flag with no chunks line")),
    (Some(_), Some(j)) if !matches!(&lines[j], Line::Flag(f) if f.class == Class::RoCompat) => {
        return Err(bad_at(j, "the feature chunk-profile must be ro_compat"));
    }
    _ => {}
}
```
Add `Line::BoundarySalt([u8; 32])`, strict 64 lowercase hex, at most one ("two boundary-salt lines"), and `Header::boundary_salt()`. Do not touch the Kani-proved functions (`valid_name_bytes`, `valid_release_bytes`, `first_line_number`, `Class::from_word_bytes`).

**Format number and M2 trap** (lines 362-373). `FORMAT_NUMBER` 7 to 8 (line 24, plus the `FORMAT` first line at 26). The `_` arm would print the format 6 text for format 7:
```rust
n if n < FORMAT_NUMBER => {
    let what = match n {
        0..=4 => "metadata in vt0/",
        5 => "224-bit object IDs",
        _ => "store format 6, no feature flags",
    };
    return Err(refuse(msg_older(store, what)));
}
n if n > FORMAT_NUMBER => return Err(refuse(msg_newer(store, n))),
```
Add explicit `6` and `7` arms; the 7 arm gets its own message naming `docs/user/upgrade.md`. Message functions follow `msg_profile_differs` (lines 297-300): `let store = store.display(); format!(...)`. Add M17 `msg_salt_differs` the same way for `seed --boundary-salt` on a store with another salt. M3 (`msg_newer`) needs no edit; v2.3.0 already refuses format 8 with it (header.rs identical at the tag).

**Test migration** (Pitfall 3): header.rs tests near 819-823, 914-916 (`slicefs-store 8 (x)` as "newer" becomes 9), `stats.rs` 642/673/722, `tests/store_header_tests.rs` ~199 (add `a_format_7_store_is_refused` next to `a_format_6_store_is_refused`).

---

### `cli.rs`, `seed.rs`, `content.rs` (seed option threading)

**Analog:** `--chunk-profile`.

cli.rs lines 102-103 (mount) and 163-164 (seed):
```rust
#[arg(long, value_name = "MIN/AVG/MAX", value_parser = crate::content::parse_chunk_profile)]
chunk_profile: Option<ChunkProfile>,
```
Add `boundary_salt: Option<BoundarySalt>` with a value parser accepting `random` (32 bytes from `/dev/urandom`, no new crate) or 64 hex. Seed only; mount reads the header. Update destructuring at cli.rs 322 and the tests at 322-330 and 358-380 (`Cli::try_parse_from([...])` style).

seed.rs `run_seed_with` (lines 42-70) passes `profile` to `content::check_write_with(store_path, hash, profile)?` after the exclusive `lock_offline` and the source check, before any store write. Thread the salt the same way down to `header::marker_with` / `check_with` for the first header write.

---

### `crates/metadata/src/store.rs`, `inode_map.rs` (PMAP-02)

**Analog:** itself. `root_rec` (store.rs 38-52) is a chain of offsets; delete the first field and the rest shift:
```rust
pub const INODE_MAP: usize = 0;
pub const ROOT_DIR_INO: usize = INODE_MAP + D;   // becomes 0
pub const NEXT_INO: usize = ROOT_DIR_INO + 8;
pub const INODE_DATA: usize = NEXT_INO + 8;
...
pub const SIZE: usize = INODE_COUNT + 8;          // 230 -> 197
```
Keep `inode_data`. Touch points (research section 1 and Pitfall 4, all in one plan): `finish_inner` (~1123-1148), `load_from_root` (~1815-1880), `begin_commit` clone (~1065), `insert_inode` (~858), `reserve_ino`/`advance_next_ino` (~643-654), `for_each_referenced` (~1617), `referenced_estimate` (7 becomes 6), tests at ~3139 and ~3274, the lock-order comment. In `inode_map.rs` the allocator part (`InodeMap::allocate_ino`, `next_ino`, `set_next_ino`, lines 15-70) stays behind the same lock and position in lock order (Pitfall 5); `serialize_/deserialize_/intern_/load_inode_map` and `RECORD` users go.

Rehash: `Blob::InodeMap`, `Kind::InodeMap = 5` (`rehash/map.rs`), `use crate::inode_map::RECORD` (`rehash/mod.rs`). `chunk-store/src/lock_stats.rs` `Site::InodeMap` and `slicefs-cli/tests/lock_stats_report.rs:90`.

---

### `benchmarks/prolly-bench/src/layout.rs` (new), `replay.rs`, `main.rs` (bench, batch)

**Analog:** `replay.rs` modes `batches` (extracts only `inode_data` changes from real commits via `root_record(..).inode_data()`), `prolly` (replays with `pkey(ino) = ino.to_be_bytes()`), `scale` (synthetic 8-byte keys, 33-byte values via `synth_value`, built from sorted streams with `Builder::push`, no whole map in RAM; ~lines 641-729). Copy `scale` for the 1M/10M synthetic generator and the `prolly` replay loop (10 commits per row, Phase 24 columns) for layouts (a)/(b). `main.rs` 41-60 holds a private copy of the root record (`ROOT_REC_SIZE = 230`, `RR_*`, `maps: [ObjectKey; 6]`, `maps[1]` as inode_data): update for 197 bytes after plan 25-02.

**Test analog:** `tests/smoke.rs` helpers:
```rust
const BIN: &str = env!("CARGO_BIN_EXE_prolly-bench");
fn ok(args: &[&str]) -> String { let (code, out) = run(args); assert_eq!(code, 0, ...); out }
```
Add the new modes with small N; existing asserts on commit counts must still pass. Update `scripts/prolly-bench/bench.sh` and the bench README.

---

### `docs/design/store-format.md` (doc)

**Analog:** its existing sections: header grammar and parse order, flag list, M-message table, Stats and JSON examples, Format policy. Edits listed in 25-RESEARCH.md section 6 ("Hard-coded 7 and 8"). Add: D-01 scale target, layout decision with numbers, node format (D-09), rule and constants, root record table, salt line, policy sentence (D-13). State plainly what is live at the end of Phase 25 (flat maps, 197-byte root) versus Phase 26. Also `docs/design/chunk-store.md:175`, `docs/design/metadata-maps.md`.

## Shared Patterns

### Setting line with a paired flag
**Source:** `crates/slicefs-cli/src/header.rs` 349-420 (`chunks` / `chunk-profile`).
**Apply to:** the boundary-salt line and flag (grammar, pairing, class, M9 reasons).

### Refuse messages
**Source:** `header.rs` `msg_*` functions (e.g. 297-300): take `store: &Path`, shadow with `store.display()`, return `String`; callers wrap with `refuse(...)`.
**Apply to:** the format 7 refusal, M17 salt mismatch.

### Option threading through seed
**Source:** `--chunk-profile` in `cli.rs` to `run_seed_with` to `content::check_write_with` to `header::marker_with`.
**Apply to:** `--boundary-salt`.

### Golden-test pinning
**Source:** `crates/prolly/src/tests.rs::golden_root`.
**Apply to:** rule table hash, thresholds, cut points, root IDs (salted and unsalted). Pin last.

### Validation commands
`export PATH="$HOME/.cargo/bin:$PATH"` before every cargo call. Mount tests only in Docker (`scripts/docker-linux-test/run.sh`).

## No Analog Found

| File | Role | Data Flow | Reason |
|---|---|---|---|
| `crates/prolly/src/rule_table.rs` | config | transform | Generated literal tables; use RESEARCH.md Pattern 1 |
| `scripts/prolly-table/gen_table.py` | utility | batch | No Python tooling in `scripts/` today; use `decimal`, 60 digits |
| `docs/user/upgrade.md` | doc | n/a | `docs/user/` does not exist; research recommends a short stub |

## Metadata

**Analog search scope:** `crates/prolly`, `crates/slicefs-cli`, `crates/metadata`, `benchmarks/prolly-bench`
**Files read:** rule.rs, lib.rs (prolly), tests.rs (prolly), header.rs (excerpts), cli.rs, seed.rs, inode_map.rs (excerpts), plus 25-CONTEXT.md and 25-RESEARCH.md (lines 1-468; the remainder was not paged in)
**Pattern extraction date:** 2026-11-26
