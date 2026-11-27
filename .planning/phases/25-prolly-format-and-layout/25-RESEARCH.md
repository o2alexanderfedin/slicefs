# Phase 25: Prolly format and layout - Research

**Researched:** 2026-11-26
**Domain:** On-disk format of prolly-tree metadata maps in a Rust content-addressed file system (store header, root record, node format, deterministic boundary rule, layout benchmark)
**Confidence:** HIGH for the code facts, the header behaviour and the fixed-point design; MEDIUM for the layout prediction (the bench decides it)

<user_constraints>
## User Constraints (from CONTEXT.md)

### Locked Decisions

**Design target**
- **D-01:** Design for 1,000,000,000 inodes (files and directories) or more in one store. The goal is a global, decentralized file system. Hard rules for Phases 25 and 26: no operation loads a whole map into RAM (not mount, `stats`, GC or scrub). Memory stays bounded at any store size. Inode and byte counts come from the counts in the internal entries, or from the root record, never from a full walk. Write this target in `docs/design/store-format.md`. — **Reversibility:** costly — later code that loads a whole map breaks the rule at scale.

**Map layout (Q-2)**
- **D-02:** Q-2 is decided by a test at the start of Phase 25, not by a guess. Wire both layouts into `benchmarks/prolly-bench`: (a) one prolly map for each metadata map, (b) one combined map with keys such as `(ino, kind, …)`. Measure commit time, CPU, nodes read and written, and bytes per commit on Boost, on 1M and on 10M synthetic inodes. Use mixed commits (one change, K=100, K=1,000, insert/delete heavy).
- **D-03:** Claude picks the layout from the test, writes the numbers and the reason in `docs/design/store-format.md` (with the effect on the root record and on the GC mark), and reports to the owner. The owner delegated this choice ("Test both, then decide"). — **Reversibility:** one-way after the format freeze (#156); before the freeze it is costly (a new format and a copy migration via #155).
- **D-04:** `refcounts` is a prolly map too, like all the other metadata maps. Then a one-change commit is O(change). #165 (refcount removal) can drop the map later.
- **D-05:** Extrapolate the test to 1B entries by tree height and node count, and write the method and the result in the doc. A 100M or 1B run is not part of this phase.

**One inode map (PMAP-02)**
- **D-06:** `inode_map` and `inode_data` both map an inode number to an `ObjectKey`. Only one stays. Claude picks which one. `next_ino` moves to the root record (if the kept map does not carry it).

**Boundary rule (PMAP-04)**
- **D-07:** Replace the float test (`exp_neg`, `cut_probability` in `crates/prolly/src/rule.rs`) with a fixed-point table and integer math. The rule carries a version byte, kept in the node and/or the root record (Claude decides where). A golden test with fixed input gives the same cut points and root ID on Linux and macOS.
- **D-08:** Retune the constants in the Q-2 test. Run the Dolt values (L=4096, K=4, `NODE_MIN` 512 B, `NODE_MAX` 16 KiB) against at least one larger L (for example 8192, a mean node of about 7.4 KB, near the 8 KiB chunk average). Weigh node count at 1B entries: each node is one chunk-index object. Pick by the bench, and write the chosen constants and the reason in `docs/design/store-format.md`. — **Reversibility:** one-way after the freeze — the constants decide every node ID, so a change rewrites every map.
- **D-09:** The prolly node format (magic, level, count, entries) becomes part of the store format and is documented in `docs/design/store-format.md`. This reverses Phase 24 D-03/D-14/D-15, where the node bytes were private to the crate.

**Boundary salt**
- **D-10:** Add an optional per-store boundary salt. The default is no salt, so node IDs are equal across stores (for global dedup). `slicefs seed` gets an option (for example `--boundary-salt random`) that writes a salt setting line into the header. By the format rule ("a new setting line always comes with a flag"), the line comes with its own flag. Claude picks the class: a build that does not know the salt can read the trees but must not write them, so ro_compat fits. The salt is part of the boundary hash input. Record grinding as an accepted risk for unsalted stores (the harm has a limit: no node above `NODE_MAX`, the tree never breaks). — **Reversibility:** one-way per store — a salt change changes every node ID.
- **D-11:** A later change of the salt (a rebuild of all maps with a new salt, similar to `rehash`) is wanted by the owner. It is a new command, so it is deferred (see Deferred Ideas). Phase 25 sets the salt at seed only.

**Format mark (PMAP-05)**
- **D-12:** Store format 8. The owner chose this over an incompat flag on format 7. The new build refuses format 7 stores with a clear message that points to `docs/user/upgrade.md` (#155). Release `v2.3.0` refuses format 8 with its existing "other format number" message. A test covers both directions. — **Reversibility:** one-way — a published format number.
- **D-13:** Update the format policy in `docs/design/store-format.md`. Today it says the number changes only when an older SliceFS cannot parse the header. Add that the number also changes for a full change of the metadata layout (format 8, prolly maps), and record that the owner decided this. Reword PMAP-05 in `.planning/REQUIREMENTS.md` and success criterion 4 of Phase 25 in `.planning/ROADMAP.md` from "new incompat flag" to "format 8" (done in the commit of this context).

### Claude's Discretion
- The resolution of the fixed-point table and the integer math (D-07). It must match the float rule closely (a test measures the error), be documented, and be pinned by a golden test.
- Which of `inode_map` / `inode_data` stays (D-06).
- The final layout after the test (D-03), and the final constants (D-08). Report both.
- Where the rule version byte lives (D-07), and the class of the salt flag (D-10).

### Deferred Ideas (OUT OF SCOPE)
- **Salt rebuild command:** rebuild all prolly maps of a store with a new boundary salt (similar to `rehash`). The owner wants this. It needs a new phase or an issue.
- **100M / 1B measured runs:** a real large-scale run of the prolly store. Separate task, hours to days.
- **Global namespace identity:** inode numbers and `(dir ino, name)` keys are local to one store, so cross-store sharing of metadata nodes works only for identical subtrees. A global identity design is its own topic.
- **Snapshot diff by equal subtrees:** a later gain of prolly roots (from `prolly-trees.md`).

**Owner constraints given with the task (not in CONTEXT.md, same authority):** no whole metadata map in RAM (1B+ inode design target); no store backward compatibility; tests that mount run only in Docker (never mount on the macOS host); `cargo test --workspace` must pass; do not push, no `gh` writes.
</user_constraints>

<phase_requirements>
## Phase Requirements

| ID | Description | Research Support |
|----|-------------|------------------|
| PMAP-01 | Map layout decided and written down (one prolly map per metadata map, or one combined namespace map, Q-2 of #19) | "Layout decision (Q-2)": key grammar for both layouts, effect on root record and GC mark, bench shape, a pre-registered decision rule, 1B extrapolation method |
| PMAP-02 | `inode_map` / `inode_data` duplication removed; root record holds only one | "PMAP-02 inventory": keep `inode_data`, drop `INODE_MAP`; every touch point listed; criterion 2 is met on the flat maps in Phase 25 |
| PMAP-04 | Fixed-point boundary rule with a version byte; rule, constants and node format in `docs/design/store-format.md` | "Fixed-point boundary rule": table design, integer math, measured error, version byte in the node header, salt as keyed BLAKE3, golden-test plan |
| PMAP-05 | Store format 8; old release refuses; new build refuses format 7 with a pointer to the upgrade doc | "Header and format 8": exact `header.rs` edits, M2 fall-through trap, v2.3.0 behaviour verified from the tag, test list |
</phase_requirements>

## Summary

Phase 25 is mostly a format, doc and plumbing phase; the only large new piece of measurement code is the layout benchmark. Four findings shape the plan. First, success criterion 2 can be met in Phase 25 on the existing flat maps: `inode_map` and `inode_data` are always written together with the same value, `next_ino` is already a field of the root record, so dropping the `INODE_MAP` field (root record 230 to 197 bytes) and shrinking `InodeMap` to a number allocator is enough. The cost is wide, not deep: the removal touches `store.rs`, `rehash`, `lock_stats`, a lock-stats test, a GC-mark test and the bench's private copy of the root record layout. Second, release `v2.3.0` already refuses a format 8 header with its M3 "newer SliceFS" message, because `header.rs` is byte-identical on `develop` and at the tag; no change to the old release is possible or needed. Third, a deterministic integer rule is straightforward: a 1,024-step table of `exp(-f)` plus a 23-entry integer table, linear interpolation, and `u128` arithmetic. My scratch experiment gave a maximum absolute error of about 1.2e-7 on the cut probability and zero differing cut decisions in 1.2 million random trials. The table does not depend on L, so one table serves L=4096 and L=8192. Fourth, Q-2 cannot be answered from reading code; the bench must be extended well beyond Phase 24, because `batches` and `scale` only cover `inode_data` today.

The interim state at the end of Phase 25 is "format 8, flat maps, five map keys". `store-format.md` must describe the prolly target (node format, rule, constants, layout) and mark clearly which parts are live only after Phase 26. No release comes between the two phases (v2.4.0 ships at the end of the milestone; tags only come from `release.yml` on `main`), so the interim store state never reaches a user.

**Primary recommendation:** Split into five plans on three waves. Wave 1, in parallel: (25-01) prolly rule, version byte, Config and goldens; (25-02) drop `inode_map` everywhere; (25-03) header format 8, M2 fix, boundary-salt line and flag, `seed --boundary-salt`. Wave 2: (25-04) extend the bench (five-map batches, layout (a)/(b), synthetic 1M/10M, L sweep), then run it by hand (25-05). Wave 3: (25-06) pin the final constants and goldens, write every doc. Keep the root record field order stable so Phase 26 only changes what a map key points at.

## Project Constraints (from CLAUDE.md)

No `./CLAUDE.md`, `.claude/CLAUDE.md`, `.claude/skills/` or `.agents/skills/` exists in the repo [VERIFIED: `ls` of those paths, this session]. The user's global instructions and memory give these directives, which the plan must respect:

- Avoid doing things with little or no value (global CLAUDE.md).
- No store backward compatibility: format changes need not stay readable by old releases (memory: no-store-backward-compat).
- No whole file or map in RAM; per-file daemon memory bounded (memory: no-whole-file-in-ram, global-decentralized-goal: full BLAKE3-256 IDs, design for 1B+ files).
- Tests that mount run in Docker only; short waits; a Boost mount run only by hand or on `main`/`ci/**` (memory: test-time-budget).
- Parallel or background agents get their own git worktree, never the shared main tree (memory: use-worktrees-for-agents).
- A huge `target/` directory slows each test binary start; `cargo clean` before suspecting a hang (memory: huge-target-dir-slow-tests). `target/` is 7.4 GB now [VERIFIED: `du -sh target`].
- Never mention licensing in public GitHub text (memory: no-license-in-issues). Do not push; no `gh` writes (task constraint).
- Tags only via `release.yml` on `main`; the v2.3 milestone ships as release `v2.4.0` [CITED: `.planning/ROADMAP.md` line 9].
- `cargo` is not on the default PATH: every command needs `export PATH="$HOME/.cargo/bin:$PATH"` first [VERIFIED: `command -v cargo` returned nothing; with the export, `cargo 1.95.0-nightly`].

## Architectural Responsibility Map

The "tiers" of this system are layers of the store, not web tiers.

| Capability | Primary Tier | Secondary Tier | Rationale |
|------------|-------------|----------------|-----------|
| Format number, flag lines, salt line, refusal messages | `slicefs-cli` `header.rs` | `docs/design/store-format.md` | The header is parsed before anything else is read; M2/M3/M9 live here |
| Boundary rule, node format, rule version byte | `crates/prolly` | `docs/design/store-format.md` | Pure function of keys, sizes, salt; needs no store, only `ChunkStore::put/read` |
| Boundary salt value (source of truth) | Store header (`FORMAT`) | `slicefs seed` writes it | Per-store setting; nodes cannot carry it (a reader never needs it, only a writer) |
| Root record layout, `next_ino`, counts | `crates/metadata` `store.rs` | bench's private copy | One fixed-width record; `inode_count` and `logical_bytes` already live here (D-01) |
| Inode number allocation | `crates/metadata` (allocator + lock) | `chunk-store` `lock_stats` `Site` | Today the allocator is inside `InodeMap`; it must survive the map |
| Layout decision data | `benchmarks/prolly-bench` | `.planning/phases/25-*/runs/` | Measured by a by-hand tool; result goes to the design doc |
| Persisted decision, constants, extrapolation | `docs/design/store-format.md` | `docs/research/prolly-trees.md`, `docs/design/metadata-maps.md` | One normative doc; the research doc keeps the history |
| Upgrade pointer for format 7 | `header.rs` message | `docs/user/upgrade.md` (Phase 27) | The doc does not exist yet (see Open Questions) |

## Standard Stack

### Core

No new external package is needed. Everything uses crates already in the workspace.

| Library | Version | Purpose | Why Standard |
|---------|---------|---------|--------------|
| `blake3` | 1.8.7 in the local registry, `blake3 = "1"` in the workspace `Cargo.toml` | Boundary hash; keyed mode for the salt | Already a dependency of `crates/prolly`; `Hasher::new_keyed(&[u8; 32])` exists [VERIFIED: `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/blake3-1.8.7/src/lib.rs:1097` `pub fn new_keyed(key: &[u8; KEY_LEN]) -> Self`] |
| `chunk-store` | workspace path crate | Node objects (`ChunkStore::put`/`read`) | Phase 24 D-14, kept |
| Rust `u128` integer math | std | `(after^4 - before^4) << 32` needs 88 bits | No dependency; deterministic on every platform |
| `/dev/urandom` (std `File`) | OS | Source of `--boundary-salt random` | The workspace has no `rand`/`getrandom` direct dependency [VERIFIED: `grep` of the root and crate `Cargo.toml` files for `getrandom`, `rand`, `urandom` found nothing; `grep` of `crates/**/*.rs` found no use]. Reading 32 bytes from `/dev/urandom` avoids a new package and its legitimacy gate |

### Supporting

| Library | Version | Purpose | When to Use |
|---------|---------|---------|-------------|
| `proptest` | workspace dev-dependency of `prolly` | Monotonicity and agreement properties of the threshold function | Rule tests |
| Python 3 `decimal` | system `/usr/bin/python3` | Offline generator of the table literals (committed script) | Only to regenerate the table; never at build or run time |

### Alternatives Considered

| Instead of | Could Use | Tradeoff |
|------------|-----------|----------|
| Table of `exp(-x)` with interpolation | Integer Taylor series with fixed term count | No table, but the owner asked for a "fixed-point table"; a table is easier to pin and document |
| Survival-table `S(s)=exp(-(s/L)^4)` per byte | Conditional probability from `S(before)/S(after)` | 16 K entries, depends on L, underflows in the tail; the delta form is exact in u64/u128 and L-independent |
| `getrandom` / `rand` crate | `/dev/urandom` | A crate adds a legitimacy audit and a dependency for 32 bytes; the project is Unix-only for the CLI |
| Salt as extra bytes after the level byte | BLAKE3 keyed mode | Keyed mode is the designed way to domain-separate by a secret; no concatenation ambiguity |

**Installation:** none.

**Version verification:** no package is added. `blake3` exists in the registry at 1.8.7 [VERIFIED: registry directory listing].

## Package Legitimacy Audit

No external package is installed or added in this phase, so the gate has nothing to check.

| Package | Registry | Age | Downloads | Source Repo | Verdict | Disposition |
|---------|----------|-----|-----------|-------------|---------|-------------|
| (none added) | — | — | — | — | — | — |

**Packages removed due to [SLOP] verdict:** none
**Packages flagged as suspicious [SUS]:** none

## Architecture Patterns

### System Architecture Diagram

```
 seed --boundary-salt random ──► header.rs ──► FORMAT file
                                   │             line 1: slicefs-store 8 (...)
                                   │             hash blake3-256
                                   │             boundary-salt <64 hex>        (optional)
                                   │             ro_compat boundary-salt v2.4  (with the line)
                                   ▼
 mount/seed/gc/... ──► header::check ──► KNOWN_FLAGS decide (M5 / read-only / go on)
                          │ format 7 ──► refuse M2(7) "copy with docs/user/upgrade.md"
                          │ format 9+ ─► refuse M3 "newer SliceFS"
                          ▼
 commit (Phase 25, flat) ──► intern 5 flat maps ──► root record (197 B, 5 map keys)
 commit (Phase 26, prolly) ─► prolly::apply per map (a) or once (b) ──► root record (same fields,
                                                                         keys now point at SPT1 roots)
                                      │
   key bytes ──► boundary_hash(level, key, salt?) ─┐
   node bytes so far ──► Splitter(before, after) ──┼─► threshold_u32(before, after)  [integer table]
   hash32 < threshold ? cut : keep growing ────────┘            │
                                                                ▼
                                   node = SPT1 | rule_version | level | count | entries
                                   ChunkStore::put(node) ──► ObjectId (33 B) = child pointer
```

### Recommended Project Structure

```
crates/prolly/src/
├── rule.rs          # boundary_hash(level, key, salt), threshold table, Splitter (no f64)
├── rule_table.rs    # generated literals: EXP_INT[23], EXP_FRAC[1025] (header says: generated, do not edit)
├── node.rs          # NODE_HEADER = 10: magic, rule_version, level, count
├── lib.rs           # constants, Config::new(l, salt), RULE_VERSION
└── tests.rs         # golden thresholds, golden cut points, golden root, error test vs f64
scripts/prolly-table/gen_table.py      # Decimal generator; the table literals are copied from its output
benchmarks/prolly-bench/src/
├── layout.rs        # new: layouts a / b, synthetic multi-map generator, mixed commits
└── replay.rs        # batches generalized to five maps
.planning/phases/25-prolly-format-and-layout/runs/   # CSVs of the layout and L runs
docs/design/store-format.md                          # normative: layout, node format, rule, constants, root record
```

### Pattern 1: Integer cut threshold (replaces `exp_neg` and `cut_probability`)

**What:** After an entry takes the node from `before` to `after` bytes, a cut happens iff `(hash32 as u64) < threshold(before, after)`, where `threshold` is `round(2^32 * (1 - exp(-(after^4 - before^4) / L^4)))`, an integer in `[0, 2^32]`.

**Why this form:** the delta form is exact in integers, depends on L only through one division, and a threshold of exactly `2^32` ("always cut") is representable only in `u64`, so compare as `u64`.

**Example (design sketch, not final code; constants are provisional):**
```rust
// delta in 2^-32 units. a^4 - b^4 < 2^56, shifted by 32 -> < 2^88: u128.
// For L = 2^k with k >= 8 this division is a shift by 4k - 32.
let num = ((after as u128).pow(4) - (before as u128).pow(4)) << 32;
let delta = num / (l as u128).pow(4);            // fixed point, 32 fractional bits
let e = exp_neg_fp(delta);                       // u64 in [0, 2^32], exp(-delta) * 2^32
let threshold = (1u64 << 32) - e;                // cut iff hash32 < threshold

fn exp_neg_fp(x: u128) -> u64 {
    let int = x >> 32;
    if int >= 23 { return 0; }                   // exp(-23) * 2^32 rounds below 1
    let f = (x & 0xFFFF_FFFF) as u64;
    let j = (f >> 22) as usize;                  // 1024 steps: top 10 bits
    let r = f & ((1 << 22) - 1);
    let (lo, hi) = (EXP_FRAC[j], EXP_FRAC[j + 1]);
    let frac = lo - (((lo - hi) * r) >> 22);     // linear interpolation, hi < lo
    ((EXP_INT[int as usize] * frac) >> 32)
}
```
`EXP_FRAC[j] = round(2^32 * exp(-j/1024))` for `j = 0..=1024` and `EXP_INT[i] = round(2^32 * exp(-i))` for `i = 0..=22` are source literals made by a committed generator (see Don't Hand-Roll).

### Pattern 2: Version byte in the node header

**What:** `NODE_HEADER` becomes 10 bytes: magic `SPT1` (4), `rule_version` `u8` (1), level `u8` (1), entry count `u32` LE (4). The empty leaf carries the byte too.

**Why the node, not the root record:** only writers cut (`Builder`, `apply`); readers (`get`, `iter`, `walk`) never evaluate the rule [VERIFIED: `crates/prolly/src/tree.rs` uses `READ_CFG` for every read and only `Builder`/`apply` call `Splitter::push`]. A writer reads the root node first, so a version byte in the node reaches exactly the code that needs it, and in a decentralized setting a foreign node explains itself. The root record has no version field today and its length is the only check (`check_root_len`), so a root-record version would be a second source of truth. Recommendation: node header only. `decode` returns `Corrupt`/a new `Error::RuleVersion` for an unknown byte; `apply` additionally requires the root's byte to equal `RULE_VERSION` before it cuts anything. A read of a node with another version byte may still be allowed (reading never depends on the rule) but Phase 26 should decide; the safe default is refuse.

### Pattern 3: Salt as keyed BLAKE3

**What:** unsalted: `Hasher::new()`, then `update([level+1])`, then `update(key)`, first 4 bytes LE (unchanged from Phase 24). Salted: `Hasher::new_keyed(&salt)` with the same two updates. The default (no salt) therefore keeps the Phase 24 boundary hash values, and node IDs are equal across unsalted stores.

**Where the salt lives:** the store header only. `Config` gets `salt: Option<[u8; 32]>` (stays `Copy`), used by writers only. A build that does not know the salt flag must not write: `ro_compat` (D-10).

### Pattern 4: Root record in Phase 25 (flat) and Phase 26 (prolly)

Root record fields after PMAP-02 (the new offsets are computed here from the old layout by removing the first field; they are not in the code yet):

| Field | Offset | Bytes |
|-------|--------|-------|
| `ROOT_DIR_INO` | 0 | 8 |
| `NEXT_INO` | 8 | 8 |
| `INODE_DATA` | 16 | 33 |
| `DIR_DATA` | 49 | 33 |
| `MANIFEST_DATA` | 82 | 33 |
| `XATTR_DATA` | 115 | 33 |
| `REFCOUNTS` | 148 | 33 |
| `LOGICAL_BYTES` | 181 | 8 |
| `INODE_COUNT` | 189 | 8 |
| `SIZE` | 197 | |

Today's layout, quoted verbatim [VERIFIED: `crates/metadata/src/store.rs:38-52`]:

```rust
pub(crate) mod root_rec {
    use super::KEY_BYTES as D;
    pub const INODE_MAP: usize = 0;
    pub const ROOT_DIR_INO: usize = INODE_MAP + D;
    pub const NEXT_INO: usize = ROOT_DIR_INO + 8;
    pub const INODE_DATA: usize = NEXT_INO + 8;
    pub const DIR_DATA: usize = INODE_DATA + D;
    pub const MANIFEST_DATA: usize = DIR_DATA + D;
    pub const XATTR_DATA: usize = MANIFEST_DATA + D;
    pub const REFCOUNTS: usize = XATTR_DATA + D;
    pub const LOGICAL_BYTES: usize = REFCOUNTS + D;
    pub const INODE_COUNT: usize = LOGICAL_BYTES + 8;
    /// Total size in bytes (230 with 33-byte keys).
    pub const SIZE: usize = INODE_COUNT + 8;
}
```

Key insight for Phase 26: a prolly root is an `ObjectId` of `ID_LEN` = `1 + DIGEST_LEN` = 33 bytes, the same size as an `ObjectKey` (`KEY_BYTES` = 33) [VERIFIED: `crates/slicefs-traits/src/key.rs:10`, `crates/chunk-store/src/lib.rs:72`]. Under layout (a) the root record layout above is therefore already final: Phase 26 changes only what each 33-byte field points to (a flat map node today, an `SPT1` root later). The two are self-identifying: a flat map node starts with a `u64` count, a prolly node with `SPT1`, so reading the wrong kind gives a clean `Corrupt`, never a silent misread. Under layout (b) the record shrinks (one namespace root, plus refcounts if kept apart) and `SIZE` changes (73 or 106 bytes), so the layout decision is exactly what fixes the final root record.

### Anti-Patterns to Avoid

- **Computing the table with floats at build or run time.** `f64::exp` is not guaranteed bit-identical across platforms. The table is a committed literal, the generator is a committed script, and a test checks the literals against `f64::exp` within one unit (a check, not a source).
- **Putting the rule version in two places** (node and root record). One source of truth: the node header.
- **Documenting only the final prolly state in `store-format.md`.** Say plainly what is live at the end of Phase 25 (flat maps, 197-byte root, format 8) and what Phase 26 turns on.
- **Letting `Config::DEFAULT` double as the read limit while L varies.** See Pitfall 6.
- **Claiming D-01 is met in Phase 25.** The flat maps are still whole in RAM until Phase 26; Phase 25 only writes the rule into the doc and does not add a new whole-map load.

## Don't Hand-Roll

| Problem | Don't Build | Use Instead | Why |
|---------|-------------|-------------|-----|
| Salted hash | Own concatenation scheme | `blake3::Hasher::new_keyed` | Designed for secret-keyed hashing; no ambiguity between salt and key bytes |
| Random salt | Own PRNG, or time/pid mixing | 32 bytes from `/dev/urandom` | OS entropy; no new crate |
| Hex of the salt | Custom codec | A tiny local `hex` (the bench already has `hex()` in `main.rs`) with strict length and lowercase check in the parser | Avoid ambiguity in the header grammar (exactly 64 lowercase hex) |
| Header line handling | A second parser | The existing `Line` enum and `parse` in `header.rs`, following the `chunks`/`chunk-profile` pairing | Same rules, same M9 error shape, same rewrite behaviour |
| Table values | Hand-typed numbers | `scripts/prolly-table/gen_table.py` using `decimal` (60 digits), output pasted into `rule_table.rs`, plus a test comparing to `f64::exp` | Reproducible, reviewable |
| Cross-platform check of the rule | A special CI job | The existing `ci.yml` jobs: Linux in Docker (`run.sh test`) and macOS (`cargo test --workspace`) both run the prolly unit tests [VERIFIED: `.github/workflows/ci.yml` lines 17-18, 50-51, 64-65] | Integer-only code is deterministic by construction; the golden test pins it on both |

**Key insight:** the correct design is cheap, because the boundary rule is a pure function. What costs is the blast radius: header, root record, GC, rehash, tests and docs all mention the old numbers.

## Runtime State Inventory

A format bump to 8 is migration-class: stores made before it are refused by design (no backward compatibility), so the question is which stored state is now unusable and what stays.

| Category | Items Found | Action Required |
|----------|-------------|------------------|
| Stored data | Every format 7 store (flat maps, 230-byte root records) is refused by the new build. No data migration is written in this phase; Phase 27 (#155) is the copy path. Stores in Docker volumes, the measure dir `~/slicefs-measure/` (Boost bench stores) and `.planning/phases/24-*/runs` data (CSV only, no stores) | Re-seed bench stores after the change; no code migration. Prolly Phase 24 nodes (`SPT1`, 9-byte header) never reached a product store, so no prolly data exists to migrate |
| Live service config | None. SliceFS has no external service config; the `FORMAT` file is inside the store. Verified: no deployments exist (memory: no-store-backward-compat) | None |
| OS-registered state | None found: no launchd, systemd or scheduled task embeds the format number (grep of `scripts/` for `slicefs-store` found nothing) | None |
| Secrets/env vars | None. The new salt is a header value, not a secret store entry and not an env var | None |
| Build artifacts / installed packages | Docker volumes `slicefs-target*` hold old build output (rebuilt by cargo; no format dependence). Any `slicefs` binary installed from `v2.3.0` refuses a format 8 store (by design) | None; do not reuse a format 7 store seeded before the change in any test fixture |

**Nothing found in:** Live service config, OS-registered state, Secrets/env vars (verified by grep of `crates`, `scripts`, `docs`, `.github`).

**Code-side runtime state that does carry the old layout:** the bench `benchmarks/prolly-bench/src/main.rs` copies the root record layout (`ROOT_REC_SIZE = 230`, `RR_INODE_MAP = 0`, `RR_NEXT_INO = 41`, `RR_INODE_DATA = 49`, ... `RR_INODE_COUNT = 222`) and indexes `maps[1]` as `inode_data`; the whole/batches modes break on a 197-byte record until it is updated [VERIFIED: `benchmarks/prolly-bench/src/main.rs:41-60` and `RootRec` at ~325-345, read via `sed`].

## Findings by Question

### 1. How the root record is serialized and loaded today, and every use of `inode_map` vs `inode_data`

- Serialization is in `finish_inner` (`crates/metadata/src/store.rs`, ~1123-1148): intern `inode_map` (`intern_inode_map`), take `next_ino` from it, intern the other five maps, then write fields in `root_rec` order, assert `root_bytes.len() == root_rec::SIZE`, `put_node`. Both maps get the same entries at the same moments: `p.inode_data.insert(*ino, d); p.inode_map.insert(*ino, d);` (~1112-1113), at the settle step (~1160-1161) and in `delete_inode` (~1436-1437) [read via `sed`, this session].
- Loading is `load_from_root` (~1815-1880): reads the record, `check_root_len`, loads `inode_map` via `load_inode_map` and calls `set_next_ino(next_ino)`, loads the other five maps, sanity-checks that `inode_data` holds the root dir ino.
- `begin_commit` (~1065-1071) clones every map including `inode_map` and `inode_data`: removing `inode_map` also removes one full map clone and one full serialization per commit (about a third of the serialized bytes, the Phase 24 estimate [CITED: `docs/research/prolly-trees.md` "Method" and "Recommendation"]).
- Readers of the `inode_map` *content*: only `insert_inode` (`map.get(ino).is_some()` in the `AlreadyExists` test at ~858, which also checks `live.map.contains_key` and `self.inode_data.lock().contains_key(&ino)`) and one test (`store.inode_map.lock().get(x).is_none()`, ~3139). `inode_data` is the one every other path reads: `load_meta` (~782), the GC mark `for_each_referenced` (~1617-1620 takes the `INODE_MAP` node, then walks `INODE_DATA` values), `referenced_estimate` (~1642), rehash. So the decision for D-06 is: **keep `inode_data`**, because the GC mark and rehash already walk it and the duplicate adds no information.
- Other users of the `InodeMap` *type*: the allocator (`reserve_ino` ~643 `self.inode_map.lock().allocate_ino()`, `advance_next_ino` ~650-654, `insert_inode` ~853-863). The lock-order comment at the top of `store.rs` ("`inode_map`, then one `inode_live` shard, then `inode_data`") and the `Site::InodeMap` lock-stats site (`crates/chunk-store/src/lock_stats.rs`, label `"inode_map"`, listed in `crates/slicefs-cli/tests/lock_stats_report.rs:90`) hang off the same lock.
- Other touch points: `crates/metadata/src/rehash/mod.rs` (`Blob::InodeMap`, `Kind::InodeMap`, `use crate::inode_map::RECORD`, field list at ~516), `crates/metadata/src/rehash/map.rs` (`InodeMap = 5` in `Kind`), `store.rs` tests (`the_windowed_walk_gives_the_keys_of_the_whole_nodes` ~3274 uses `at(root_rec::INODE_MAP)`; `referenced_estimate` has the literal `7` nodes, which becomes `6`), `docs/design/metadata-maps.md`, `docs/design/chunk-store.md` ~284 ("the inode map, the refcount table and the root record").

**Criterion 2, answered plainly:** yes, it is met in Phase 25 on the flat maps. A format 8 store written by the new build has a 197-byte root record with five map keys; it mounts; the existing metadata and mount tests pass once the offsets and counts above are updated. `next_ino` is already a root record field (D-06's "move" affects RAM only: `InodeMap::next_ino` becomes a plain `Mutex<u64>` or similar allocator, and `load_from_root` sets it from `u64_at(root_rec::NEXT_INO)`).

### 2. How a `v2.3.0` binary reacts to a format 8 header

It refuses, with message M3, for every command, with no change needed. At the tag `v2.3.0`:

```
24:pub const FORMAT_NUMBER: u32 = 7;
244:fn msg_newer(store: &Path, n: u32) -> String {
371:        n if n > FORMAT_NUMBER => return Err(refuse(msg_newer(store, n))),
```
[VERIFIED: `git show v2.3.0:crates/slicefs-cli/src/header.rs | grep -n ...`, this session]. `git diff v2.3.0 --stat -- crates/slicefs-cli/src/header.rs` printed nothing, so the current file is identical [VERIFIED]. The text (from `msg_newer`, Read this session) is `{store} was made by a newer SliceFS (store format {n}). This SliceFS reads store format {known} only.` The parse stops at the first line, before any flag line is read, so a salt line or any other format 8 layout cannot confuse the old build.

**Testing "release v2.3.0 refuses it":** the old rule cannot be run from the new tree. Two layers: (1) in-tree regression tests that format 9 gives M3 and that format 8 is accepted (the existing tests use `slicefs-store 8 (x)` as the "newer" example and must move to 9, see Pitfall 3); (2) a by-hand log of the real old binary. Building `v2.3.0` in the container needs a source tree: the Docker image `rust:1.100.0-bookworm` has no `git` and the repo is mounted read-only at `/src` [VERIFIED: `scripts/docker-linux-test/Dockerfile`, `run.sh` `-v "$REPO":/src:ro`], so `git archive v2.3.0 | tar -x -C <dir>` on the host and a second `docker run -v <dir>:/src:ro` with its own `SLICEFS_TARGET_VOL` is the practical path. A `stats` call needs no mount and only runs the header check, so the by-hand log can use `slicefs stats <format-8-store>` from a host build of the tag in a separate worktree, and a mount attempt inside Docker only. Record the log in the phase directory. Phase 27's migration test will build the old binary anyway (MIGR-02), so a full automated old-binary test belongs there.

### 3. Fixed-point cut-probability table (design and measured error)

Design (see Pattern 1): fixed-point delta in `u128`, integer part split from fraction, `EXP_INT[0..=22]` and `EXP_FRAC[0..=1024]` in `u64` scaled by 2^32, linear interpolation, product shifted down, `threshold = 2^32 - e`, compare `u64`. Behaviour at the edges: `delta >= 23` gives `e = 0`, threshold `2^32`, "always cut" (the real probability exceeds 1 - 2^-32 there, and the forced cut at `NODE_MAX - MAX_ENTRY` bounds the node anyway); `delta` below 2^-32 gives threshold 0, "never cut".

Measured with a scratch Python script `fp.py` (my own scratch output, not committed; reproduce by writing the table with `decimal`): maximum absolute error of the cut probability against the float formula over 200,000 random `(before, after)` pairs with `after <= 16384`:

| Frac step | Entries | Max abs error |
|-----------|---------|---------------|
| 2^-6 | 65 | 3.0e-05 (2^-15) |
| 2^-8 | 257 | 1.9e-06 (2^-19) |
| 2^-10 | 1,025 | 1.2e-07 (2^-23) |

The error does not depend on L (the same maximum for 4096 and 8192) because only `delta` enters the table. Decision-level agreement with random 32-bit hashes at step 2^-10: 0 differing cut decisions in 400,000 trials for each of L=4096 (`after` 512..14336), L=8192 (`after` 512..30720) and L=256 (the `SMALL`-like range 32..768) [scratch output]. Recommendation: step 2^-10 (1,025 + 23 literals, about 8 KB of source); the size is irrelevant to runtime.

What the tests must pin (the planner should write these, they are the criterion-3 evidence):
1. A golden table: BLAKE3 of the little-endian bytes of both tables, written in the test and in `store-format.md`.
2. A golden threshold table for fixed `(L, before, after)` triples, as integers (replaces the `to_bits` table in `golden_root`).
3. A golden list of cut points: the offsets where the splitter cuts a fixed key sequence (for example `u64_entries(5_000, 7)`) and the resulting root ID (`golden_root` pattern, new values).
4. An error test: for a grid and for random pairs, `|threshold/2^32 - (1 - exp(-delta))| <= 2^-22` (using `f64` only inside the test as the reference), and for the real decision metric D-07 needs, the share of cut decisions that differ from the float rule on a built tree (expected 0 at this resolution). Include `Config::SMALL` (L=256, min 32, max 1,024), which the Phase 24 sweep did not cover.
5. Properties: threshold nondecreasing in `after`, nonincreasing in `before`, equal to `2^32` for large delta, `0` for tiny delta.

The existing goldens (`golden_root`: root `1ee1cf56...`, `(depth, leaves) = (3, 58)`, and the `to_bits` array [VERIFIED: `crates/prolly/src/tests.rs:42-72`]) all change with the new rule, the header byte, and any new constant; they are re-pinned in the last plan after the constants are final.

### 4. Layout decision (Q-2): what the bench has to do

**What exists:** `benchmarks/prolly-bench` has modes `gen`, `seed`, `whole`, `batches`, `flat`, `prolly`, `scale`. `batches` extracts only the `inode_data` change of each real commit (`replay.rs` `batches`, `root_record(...).inode_data()`), `prolly` replays that one map with `pkey(ino) = ino.to_be_bytes()`, and `scale` builds one synthetic map of 8-byte keys with 33-byte values (`replay.rs` ~641-729) [read this session]. `Config::DEFAULT` is the only config the bench can use, because `Config` has private fields and no constructor (`crates/prolly/src/lib.rs:144-198`).

**What must be added** (plan 25-04):

1. `Config::new(l, node_max, salt)` (or named presets), so the bench can sweep L; and a read limit that is at least as large as any writable config (Pitfall 6).
2. `batches` for all five maps. The entries of all five maps are 41 bytes (`MAP_ENTRY = 8 + 33` for the four per-inode maps; `refcounts` is a 33-byte key and an 8-byte count, also 41), so `MapStream` can decode either shape; output one `base-<map>.bin` and one batch file per map and commit.
3. A `layout` mode that replays those batches into layout (a) (five trees, five `apply` calls and one sync per commit) or layout (b) (one tree, one `apply` per commit, one sync) in a scratch store and prints, per commit, the Phase 24 columns (`ms`, CPU, `nodes_read`, `nodes_written`, `bytes_written`, new objects and stored bytes, peak RSS) plus the tree shape per tree.
4. A synthetic multi-map generator and edit generator for 1M and 10M inodes. A real 10M-file seed is impractical: Boost 1_84_0 gives 84,104 inodes, and `whole` runs the real commit path with all maps in RAM. Synthetic data: N inodes; each inode gets an `inode` entry; about 90% get a `manifest` entry (files); about 1 in 10 gets a `dir` entry; about 5% get an `xattr` entry; `refcounts` gets about 2 entries per file. Values are 33-byte random `ObjectKey`-shaped blobs with tag `0x1e` (as `synth_value` does). Build from sorted streams with `Builder::push` (no whole map in RAM, which also respects the memory rule). `refcounts` keys must be sorted: generate key `i` as `0x1e || BE(i) || filler` and edit at random `i`, so edits land at random leaves as real content keys do. Synthetic commits: (1) one change (inode mtime plus one manifest and one refcount change); (2) K=100 and (3) K=1,000 mixed value edits; (4) insert/delete heavy (K inserts and K deletes in the middle; the `insdel` pattern of Phase 24, with new inodes at the right edge for creates). Commit loop of 10 commits per row, like Phase 24.
5. A sweep over L for the L decision (D-08): a `--l` and `--node-max` argument on the scale and layout modes.
6. Bench README and `bench.sh` updates; the `SMOKE=1` path and `tests/smoke.rs` extended (the smoke test asserts the exact commit counts of the current modes and must keep passing; add the new modes with small N).

**Key grammar for the two layouts** (the decision data in the planner's hands; the note on `(ino, kind)` versus `(kind, ino)` matters because the two docs disagree):

| | Layout (a): per-map | Layout (b): combined |
|---|---|---|
| Trees | 5: `inode`, `dir`, `manifest`, `xattr`, `refcounts` | 1 (or 2 with refcounts kept apart) |
| Per-inode key | `ino` as 8 bytes big endian | `0x01 ‖ ino (8 BE) ‖ kind (1)`, kind: 0 inode, 1 dir, 2 manifest, 3 xattr (10 bytes) |
| Refcount key | the 33-byte content key | inside: `0x02 ‖ content key (33)` (34 bytes); apart: as (a) |
| Value | 33-byte `ObjectKey` (refcounts: 8-byte count) | same |
| Root record | 5 map roots, 197 bytes, the table above | 1 namespace root (+1 refcounts root), 73 or 106 bytes |
| GC mark | 5 tree walks, one value type per tree | 1 walk, dispatch on the kind byte |
| Counts | root of `inode` map gives the inode count; others by their own roots | per-kind counts need a range descent (the internal entries carry subtree counts, so it is O(depth) but needs a `count_range` call that `prolly` does not have); `inode_count` and `logical_bytes` are already root record fields |

Reason for the ino-first order in (b): `(kind, ino)` is just (a) with a shared top; it keeps five separate leaf ranges and gains only one root. `(ino, kind)` puts the inode, manifest and xattr records of one file in the same leaf, so a content change (inode mtime and size plus manifest) rewrites one path instead of two. That adjacency is the only structural reason for (b) to beat (a) [ASSUMED: analysis, to be measured]. Refcounts have a different key space (a content hash, random order) and their own lifetime (#165 removes them), which is why a (b) variant with refcounts apart (the "(c)" arm) is cheap to include: it is the same code with a flag.

**Open scope question to settle in the plan:** the Q-2 text in `docs/research/prolly-trees.md` writes the combined key as `(kind, inode, name)`, i.e. directory entries by name, while D-02 writes `(ino, kind, …)`. Directory entries by name would replace the one flat list per directory (`DirList`) with one prolly entry per name. That changes the directory code (a Phase 26 matter) and is also the real scale fix for a directory with millions of names. Recommendation: Phase 25 benchmarks the same value shape in both layouts (ino keys, one 33-byte value per map entry, as the current maps have) and writes the key grammar with room for a name suffix (`... ‖ kind ‖ name`, max 8+255+2 = 265 bytes, under `MAX_KEY` = 1,024); it does not decide directory-entry explosion. Tag [ASSUMED]; the owner or the planner confirms.

**Pre-registered decision rule** (write it into the plan before the run so the choice is not made after seeing the data; thresholds are a recommendation [ASSUMED]): choose (b) (with refcounts in whichever form wins) if, averaged over the Boost rows and the 1M and 10M rows for K = 1 and K = 100, (b) reads plus writes at least 20% fewer nodes per commit than (a), its CPU per commit is not worse, its bytes per commit at K = 1,000 and for the insert/delete-heavy pattern are not more than 10% worse, and its extrapolated node count at 1B is not larger. Otherwise choose (a): it is simpler, keeps one value type per tree, and drops `refcounts` (#165) as one root. A tie goes to (a). The result and the numbers go to the owner (D-03).

**1B extrapolation method (D-05).** From a measured tree: entries per leaf = (mean leaf bytes - 9) / mean entry bytes; fanout = (mean internal bytes - 9) / internal entry bytes (`2 + klen + 33 + 8`); level counts: `leaves = N / entries_per_leaf`, each level above is the previous divided by the fanout until one node remains; depth = number of levels; nodes = their sum. With the Phase 24 Boost shape (leaf mean 3,729 B, internal mean 3,111 B [CITED: `docs/research/prolly-trees.md` "Tree shape"]) and 45-byte leaf entries (4 + 8 + 33) and 51-byte internal entries (2 + 8 + 33 + 8), one 1B-entry map at L=4096 has about 82.7 entries per leaf and a fanout of about 60.8: about 12.1M leaves, 199K, 3.3K, 54 and 1 nodes above them, depth 5, about 12.3M nodes (this is my arithmetic on those inputs, [ASSUMED] until the bench confirms the shape at 10M). At L=8192 with the mean node about doubled, about 6.1M leaves, depth 5, about 6.2M nodes [ASSUMED]. Each node is one chunk-store object, and the on-disk index keeps the full 33-byte ID of each object [CITED: `docs/design/chunk-store.md` ~459], so the node count is a real cost; this is the reason D-08 asks to weigh L.

### 5. Constants (D-08)

Candidates for the bench: (L=4096, `NODE_MAX` 16 KiB) the Dolt values; (L=8192, 16 KiB); (L=8192, 32 KiB). For L=8192 the probability of reaching the forced cut at `NODE_MAX - MAX_ENTRY` = 14,336 is `exp(-(14336/8192)^4)` = `exp(-9.4)`, about 8e-5 per node, so a 16 KiB `NODE_MAX` is workable for L=8192 but adds forced cuts that break the "boundaries depend on keys only" property a little more often; 32 KiB removes that and equals the FastCDC maximum chunk (32 KiB). Measure forced-cut share as a bench column. Keep `MAX_ENTRY` = 2,048 and `MAX_KEY` = 1,024 unless the doc's key grammar needs room (265-byte keys fit). Because the forced-cut condition and `NODE_MAX` appear in every decode check, `NODE_MAX` is a format constant; record it with L.

Power-of-two L (4096, 8192) turns the division into a shift, `delta = (after^4 - before^4) >> (4k - 32)` for `L = 2^k`, `k >= 8`; recommend the format document says L must be a power of two for rule version 1 (this also keeps `Config::SMALL` (L=256, k=8, shift 0) valid). [ASSUMED: simplification, optional]

### 6. Header, format 8 and the boundary-salt flag (PMAP-05, D-10)

Exact edits in `crates/slicefs-cli/src/header.rs` (current code Read this session):

- `FORMAT_NUMBER` 7 to 8 (line 24); `FORMAT` first line `"slicefs-store 8 (...)\n"` (line 26); doc comments at lines 3 and 67.
- **M2 fall-through trap.** The older-format match is:
  ```rust
  n if n < FORMAT_NUMBER => {
      let what = match n {
          0..=4 => "metadata in vt0/",
          5 => "224-bit object IDs",
          _ => "store format 6, no feature flags",
      };
      return Err(refuse(msg_older(store, what)));
  }
  ```
  [VERIFIED: `header.rs:362-373`]. With `FORMAT_NUMBER = 8`, a format 7 store falls into `_` and prints the format 6 text. Add an explicit `6` arm and a `7` arm (and ideally make `_` unreachable-by-design). The text for 7 differs from `msg_older`: `{store} was made by SliceFS release v2.3.0 or earlier (store format 7, flat metadata maps). This SliceFS reads store format 8 only. Copy the store with the steps in docs/user/upgrade.md.` The current `msg_older` ends "copy the files out with the older version, then into a new store" and does not name the upgrade doc. The phrase "release v2.3.0 or earlier" is accurate for format 7 (it is what `v2.3.0` writes; earlier releases wrote lower numbers), but check against `git show v2.2.0:crates/slicefs-cli/src/header.rs` before writing it.
- M3 text uses `FORMAT_NUMBER` dynamically (`msg_newer`), so it becomes "reads store format 8 only" with no edit.
- `KNOWN_FLAGS` (line 46) gets `boundary-salt`; add `BOUNDARY_SALT_FLAG`, `BOUNDARY_SALT_RELEASE = "v2.4"` constants like `CHUNK_PROFILE_*`. The release word is `v2.4` because release `v2.4.0` is the first to know it [CITED: ROADMAP line 9]; tests must use the constant, not the text (existing convention).
- New `Line::BoundarySalt([u8; 32])` (or keep it a `Line::Other` plus validator; follow `Chunks`), grammar `boundary-salt <64 lowercase hex>`, exactly two words, at most one. Pairing rules exactly like `chunks`/`chunk-profile`: a salt line needs the flag, the flag needs the line, the flag is ro_compat. New M9 reasons: `bad boundary-salt line`, `two boundary-salt lines`, `boundary-salt line with no boundary-salt flag`, `boundary-salt flag with no boundary-salt line`, `the feature boundary-salt must be ro_compat`. `to_text` and the rewrite path keep the line in order (the `Chunks` precedent). The Kani proofs cover only `valid_name_bytes`, `valid_release_bytes` and `Class::from_word_bytes` [VERIFIED: `header.rs:1131-1180` region read]; leave those functions untouched.
- Class: `ro_compat`. A build that does not know the flag can read prolly trees (reading does not use the salt) but a write with the wrong salt would cut with another rule and silently fork the tree shapes, so it must be read-only. Same reasoning as `chunk-profile`.
- `seed`: new `--boundary-salt random` (also accept an explicit 64-hex value for tests and reproducible stores; `random` reads `/dev/urandom`). Follow the `--chunk-profile` threading: `cli.rs` option, `seed::run_seed_with` parameter, `content::check_write_with`, `header::marker_with` / `check_with` for the first header write. The salt is fixed when the store is made; `seed --boundary-salt X` on an existing store that has no or another salt is refused with a new M17 (parallel to M16 `msg_profile_differs`), because changing it is the deferred rebuild command (D-11). `mount` takes no salt option; it reads the header.
- In Phase 25 the salt reaches no product code (maps are flat); the plan only needs the header plumbing, a getter (`Header::boundary_salt()`), and `prolly::Config` support, tested in the prolly crate with a salted golden root.
- `stats`: the existing Feature flags row lists the flag; adding a salt row is optional and not required by any success criterion (low value, skip unless the planner wants parity with `Chunk profile`).

Hard-coded 7 and 8 that must move [VERIFIED by grep this session]: `header.rs` tests at 819 (`"slicefs-store 7\n"`, `format, 7`), 822-823 and 914 (`slicefs-store 8 (x)` as the "newer" example, must become 9), 916; `crates/slicefs-cli/src/stats.rs` 642 (`"format":7`), 673, 722 (`Store format     : 7`); `crates/slicefs-cli/tests/store_header_tests.rs` 199 comment (and the `a_format_6_store_is_refused` neighbour: add `a_format_7_store_is_refused`); docs: `docs/design/store-format.md` (examples, parse order step 1 "If the number is not 7", flag list "Store format 7 has two flags", M2/M3 rows, Stats example, JSON example, Format policy), `docs/design/chunk-store.md:175` ("format 7"), `crates/slicefs-cli/src/content.rs` module doc if it names 7. The header of a default new store becomes `slicefs-store 8 (...)` followed by `hash blake3-256`.

### 7. Where `docs/user/upgrade.md` stands

`docs/user/` does not exist (`ls docs` shows `design`, `research`, `superpowers`) [VERIFIED]. MIGR-03 (Phase 27) writes it. The M2 text for format 7 must point to it now (success criterion 4). Options for the planner: (1) create a minimal stub in Phase 25 (path, one paragraph: "format 7 stores are not opened by this release; the copy path is described here when Phase 27 lands", plus the commands to see the format number: `slicefs stats`), and let Phase 27 fill it; (2) point at the path and accept a dangling pointer until Phase 27 (no release comes between, so no user sees it). Recommendation: (1), a 10-line stub, because the message must not point at a missing file in any build someone might run from `develop`. [ASSUMED]

## Common Pitfalls

### Pitfall 1: M2 prints the format 6 text for format 7
**What goes wrong:** the `_ =>` arm of the older-format match catches 7 once `FORMAT_NUMBER` is 8.
**Why it happens:** the match was written when 7 was the current format.
**How to avoid:** explicit arms 6 and 7; a test that a format 7 header produces the new text and the upgrade-doc path, and one for 6 that still produces the old text.
**Warning signs:** a test for 7 that only asserts `refused`, not the text.

### Pitfall 2: The upgrade doc does not exist
See Findings section 7. Decide stub or dangling pointer explicitly in the plan.

### Pitfall 3: Tests that use 8 as "newer" and 7 as "current"
`header.rs` tests 819-823 and 914-916, `stats.rs` 642, 673, 722. Change them together with `FORMAT_NUMBER`; a grep for `slicefs-store 7`, `"format":7`, `Store format     : 7`, `store format 8` before and after is the check.

### Pitfall 4: `inode_map` removal reaches past `store.rs`
Do all of these in one plan (25-02) or the workspace does not compile or tests fail: `PreparedCommit.inode_map` field and its clone in `begin_commit`; the allocator lock and the lock-order comment; `Site::InodeMap` in `chunk-store/src/lock_stats.rs` (decide: rename to an allocator label and update `crates/slicefs-cli/tests/lock_stats_report.rs:90`, or keep the label and the misleading name; renaming is three edits); rehash `Blob::InodeMap`, `Kind::InodeMap = 5` and `RECORD` import (the `Kind` byte is only in rehash temp files, not in the store format, so deleting the variant is safe; check `rehash` tests that count kinds); `for_each_referenced` (drop the `INODE_MAP` node from the array) and `referenced_estimate` (`let mut keys = 7u64` becomes 6); the test `the_windowed_walk_gives_the_keys_of_the_whole_nodes`; `root_rec` offsets everywhere; `inode_map.rs` (`serialize_inode_map`, `deserialize_inode_map`, `intern_inode_map`, `load_inode_map` and their tests go; the allocator tests remain); `metadata/src/lib.rs` module list; the bench's private root record copy (constants and `maps: [ObjectKey; 6]`, `maps[1]`).

### Pitfall 5: `insert_inode` relies on the lock for atomicity
The allocator lock is held across "take a number and cache the inode" so that no number is given twice, and for an explicit `at` it checks existence and raises `next_ino` under that lock. Keep one lock with the same position in the order (before `inode_live`, before `inode_data`); do not replace it with an atomic unless the explicit-`at` path is re-argued. Replay of the write log calls `advance_next_ino`, which also needs the guarantee. Tests: the existing concurrency and replay tests must pass unchanged.

### Pitfall 6: `Config` has no constructor and `READ_CFG = Config::DEFAULT` assumes DEFAULT is the largest
Readers check every node against `READ_CFG` (`tree.rs:306`, "the largest config (the crate constants), so they read a tree of any config"). If the bench sweeps `NODE_MAX` = 32 KiB, a tree built with it fails to read under the 16 KiB `READ_CFG` (`decode` rejects `bytes.len() > cfg.node_max()`). Make the read limits a separate constant set that is at least every writable config, or take them from the tree's config. Also `Config` derives `Eq` and `Copy`; keep both when adding `salt: Option<[u8; 32]>`.

### Pitfall 7: Salt inside the hash or beside it
Do not concatenate the salt into the key bytes (it would also change the key used for `last_key` ordering if someone reuses the buffer). Use keyed BLAKE3 on the boundary hash only; keys stored in nodes are unchanged.

### Pitfall 8: Re-pinning goldens before the constants are final
The constants (L, `NODE_MAX`) decide every node ID. If the golden root is pinned in plan 25-01 and L changes after the bench, the goldens change again. Plan 25-01 introduces the rule with provisional constants and a one-command regenerate of the goldens; plan 25-06 re-pins after the decision, and the doc quotes the final root and table hash.

### Pitfall 9: The interim format 8 store
A store written at the end of Phase 25 is "format 8 with flat maps". Under layout (a) the Phase 26 loader sees the same 197-byte record and reads a flat map node as a prolly root: it fails with `Corrupt("wrong magic")` (clean). Under (b) the length check fails. Either way a dev store from between the phases is unusable after Phase 26; acceptable (no compatibility), but say it in the Phase 25 doc and do not merge Phase 25 to `main` alone. [ASSUMED: sequence of merges]

### Pitfall 10: Bench stores made before the change
`~/slicefs-measure/` Boost stores and any `runs/` stores are format 7; the new bench modes refuse them (`root_record` length check). Re-seed with `prolly-bench seed` after 25-02.

### Pitfall 11: `cargo` not on the PATH, and a huge `target/`
Every validation command carries `export PATH="$HOME/.cargo/bin:$PATH"`. If a test binary starts slowly, check the file count of `target/debug/deps` before suspecting a hang (memory note).

## Code Examples

### Existing float rule being replaced
```rust
// Source: crates/prolly/src/rule.rs:63-66, 106 (Read this session)
pub fn cut_probability(cfg: &Config, before: usize, after: usize) -> f64 {
    let delta = s4(after).saturating_sub(s4(before)) as f64 / s4(cfg.weibull_l()) as f64;
    1.0 - exp_neg(delta)
}
// in Splitter::push:
(boundary_hash(level, key) as f64 / 4_294_967_295.0) < cut_probability(cfg, before, self.size)
```

### Boundary hash with an optional salt
```rust
// Source: blake3 1.8.7 Hasher::new_keyed (verified at src/lib.rs:1097); design sketch
pub fn boundary_hash(salt: Option<&[u8; 32]>, level: u8, key: &[u8]) -> u32 {
    let mut h = match salt {
        Some(k) => blake3::Hasher::new_keyed(k),
        None => blake3::Hasher::new(),
    };
    h.update(&[level.wrapping_add(1)]);
    h.update(key);
    let d = h.finalize();
    let b = d.as_bytes();
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}
```

### Header: refusing format 7 (design sketch)
```rust
// Source: header.rs parse order step 1; new arms for 6 and 7
n if n < FORMAT_NUMBER => {
    return Err(refuse(match n {
        0..=4 => msg_older(store, "metadata in vt0/"),
        5 => msg_older(store, "224-bit object IDs"),
        6 => msg_older(store, "store format 6, no feature flags"),
        _ => msg_flat_maps(store, n),   // 7: names docs/user/upgrade.md
    }));
}
```

### New root record offsets as a test
```rust
// Pin the interim layout so the bench copy and the docs cannot drift
assert_eq!(root_rec::SIZE, 197);
assert_eq!((root_rec::NEXT_INO, root_rec::INODE_DATA, root_rec::REFCOUNTS), (8, 16, 148));
```

## State of the Art

| Old Approach | Current Approach | When Changed | Impact |
|--------------|------------------|--------------|--------|
| Float Weibull test with a hand-made `exp` (basic ops only) | Integer threshold from a pinned table | Phase 25 | The rule becomes a documentable, language-independent part of the format |
| Node bytes private to the crate (Phase 24 D-14) | Node bytes are part of the store format | Phase 25 (D-09) | `docs/design/store-format.md` documents magic, version, level, count, entries; any change needs a new rule version or format number |
| Format number changes only when the header cannot be parsed | Also changes for a full metadata layout change | Phase 25 (D-13) | Policy text in the doc must say it and record the owner decision |
| `inode_map` and `inode_data` both stored | One map | Phase 25 | Root record 230 to 197 bytes; about a third fewer serialized bytes per commit |

**Deprecated/outdated:**
- `exp_neg`, `cut_probability`, `WEIBULL_K` as a runtime-looking constant: K=4 is now baked into the `x*x*x*x` shape; keep `WEIBULL_K` only as documentation or remove it.
- The Phase 24 statement "the node bytes are private" in `crates/prolly/src/lib.rs`, `node.rs`, `docs/research/prolly-trees.md` "Node storage", and `docs/design/metadata-maps.md` "later": update to point at `store-format.md`.

## Assumptions Log

| # | Claim | Section | Risk if Wrong |
|---|-------|---------|---------------|
| A1 | `(ino, kind)` adjacency is the only structural reason for (b) to win; (b) with refcounts apart is the likely best combined variant | Findings 4 | The bench decides; if wrong, the decision rule still gives the right answer from data |
| A2 | The 20% node-count threshold and the 10% bytes tolerance of the decision rule | Findings 4 | The planner/owner may prefer other numbers; fix them in the plan before running |
| A3 | Directory entries by name are out of scope for the layout decision in Phase 25 | Findings 4 | If the owner meant `(kind, inode, name)` literally for (b), the bench needs a dirent-by-name arm and Phase 26 grows |
| A4 | 1B extrapolation numbers (12.3M nodes at L=4096, 6.2M at L=8192) | Findings 4 | Only arithmetic on the Boost shape; the 10M bench run checks the shape |
| A5 | Power-of-two L is acceptable as a rule-version-1 requirement | Findings 5 | Slightly less freedom in future retuning; a new rule version removes it |
| A6 | A 10-line `docs/user/upgrade.md` stub in Phase 25 is better than a dangling pointer | Findings 7 | Phase 27 may prefer to own the file entirely; cheap to drop |
| A7 | `ro_compat` is the right class and `v2.4` the right release word for `boundary-salt` | Findings 6 | The owner delegated the class (D-10); the release word follows the roadmap |
| A8 | Phase 25 is not merged to `main` or released alone | Pitfall 9 | If it were, users would get format 8 flat stores that Phase 26 cannot read |
| A9 | `NODE_MAX` 32 KiB with L=8192 avoids forced cuts and fits the chunk store limit | Findings 5 | The bench measures forced-cut share; the chunk store object limit was not read (only that `ChunkStore::put` has an "object too large for one record" error, `chunk-store/src/lib.rs:383`) |
| A10 | The release text "v2.3.0 or earlier" in the format 7 message is accurate | Findings 6 | Check `git show v2.2.0:...header.rs` before writing it |

## Open Questions

1. **Does layout (b) include directory entries by name?**
   - What we know: D-02 says `(ino, kind, …)`; the Phase 24 Q-2 text says `(kind, inode, name)`.
   - What's unclear: whether Phase 25 should leave room only, or benchmark a by-name arm.
   - Recommendation: leave room in the key grammar, benchmark the same value shape, and name directory-entry explosion as a Phase 26 decision (A3).

2. **Upgrade doc stub or dangling pointer** (Findings 7). Recommendation: stub (A6).

3. **Does `READ_CFG` become per-tree or a fixed maximum?** (Pitfall 6.) Recommendation: a fixed format maximum (`NODE_MAX` of the format), because the format fixes the constants anyway.

4. **Does the empty tree's root carry the rule version, and may a reader accept a node with another version byte?** Recommendation: yes it carries it; readers refuse an unknown version in Phase 25 (the conservative choice); relax later by a deliberate rule-version change.

5. **Keep `ROOT_DIR_INO` in the root record?** It is "always 1" in `finish_inner`. Removing it saves 8 bytes and one field, but it is outside Phase 25's scope. Recommendation: keep it (a smaller diff; Phase 26 may drop it with the rest of the record).

6. **Is a root record magic or version byte worth adding now?** The record is unversioned and length-checked. The node header already versions the rule; the format number versions the layout. Recommendation: no (low value).

## Environment Availability

| Dependency | Required By | Available | Version | Fallback |
|------------|------------|-----------|---------|----------|
| `cargo`/`rustc` | all builds and tests | ✓ (not on default PATH) | cargo 1.95.0-nightly (2026-01-30), rustc 1.95.0-nightly (2026-02-10) with `export PATH="$HOME/.cargo/bin:$PATH"` | — |
| Docker (OrbStack) | Linux golden check, mount tests, Linux bench run | ✓ | Docker 29.8.2, context `orbstack`; volumes `slicefs-target`, `slicefs-cargo` exist; no containers running | — |
| Docker image `rust:1.100.0-bookworm` (`slicefs-fusetest`) | `scripts/docker-linux-test/run.sh` | ✓ (built by the script; no `git` inside) | Rust 1.100.0 | build from host-prepared source |
| Python 3 | table generator, median rows of `bench.sh` | ✓ | `/usr/bin/python3` (system) | — |
| Boost trees | bench Boost rows | ✓ | `~/slicefs-measure/data/x/boost_1_84_0` and `boost_1_85_0` | `scripts/macos-mount-test/fetch-boost.sh DIR 1_84_0 1_85_0` |
| git tag `v2.3.0` | old-binary check | ✓ | `582a41d` (merge of release/v2.3.0) | — |
| Disk | 10M-inode synthetic runs, Docker volumes | ✓ | 649 GiB free on `/System/Volumes/Data` | — |
| FUSE-T / macOS mount | not used | — | tests that mount run only in Docker | — |

**Missing dependencies with no fallback:** none.
**Missing dependencies with fallback:** none. The 10M runs need a disk-backed store (the context says no tmpfs); the Docker volume `-v name:/work` is disk-backed, and a host bind mount works on macOS.

## Validation Architecture

### Test Framework
| Property | Value |
|----------|-------|
| Framework | Rust built-in test harness; `proptest` (workspace dev-dependency of `prolly`); Kani proofs in `header.rs` (unchanged functions) |
| Config file | none (workspace `Cargo.toml`) |
| Quick run command | `export PATH="$HOME/.cargo/bin:$PATH"; timeout 900 cargo test -p prolly </dev/null` |
| Full suite command | `export PATH="$HOME/.cargo/bin:$PATH"; timeout 2400 cargo test --workspace </dev/null`; Linux and mount tests: `scripts/docker-linux-test/run.sh test` (and `all` for the FUSE smoke) |

### Phase Requirements → Test Map
| Req ID | Behavior | Test Type | Automated Command | File Exists? |
|--------|----------|-----------|-------------------|-------------|
| PMAP-04 | threshold table pinned (hash of literals) and threshold goldens | unit | `cargo test -p prolly -- rule` | ❌ Wave 0 (new tests in `crates/prolly/src/tests.rs`) |
| PMAP-04 | same cut points and root on Linux and macOS | unit, run by CI on both | `cargo test -p prolly -- golden`; Linux: `scripts/docker-linux-test/run.sh prolly-bench-smoke` | ❌ Wave 0 (new goldens) |
| PMAP-04 | fixed-point error vs float, decision agreement incl. `Config::SMALL` | unit/property | `cargo test -p prolly -- fixed_point` | ❌ Wave 0 |
| PMAP-04 | unknown rule version refused by `decode` and `apply` | unit | `cargo test -p prolly -- rule_version` | ❌ Wave 0 |
| PMAP-04 | salted vs unsalted differ; unsalted equals Phase 24 boundary hash | unit | `cargo test -p prolly -- salt` | ❌ Wave 0 |
| PMAP-02 | root record is 197 bytes, offsets pinned | unit | `cargo test -p metadata -- root_rec` | ❌ Wave 0 |
| PMAP-02 | commit/load round trip, `next_ino` restored, inode numbers not reused | unit | `cargo test -p metadata` (existing tests incl. `next_ino` at `store.rs` ~2431-2437) | ✅ update |
| PMAP-02 | GC mark and windowed walk without the `INODE_MAP` node | unit | `cargo test -p metadata -- windowed_walk referenced` | ✅ update (~3255) |
| PMAP-02 | rehash walks the 5-key record | integration | `cargo test -p metadata rehash`, `cargo test -p slicefs-cli --test hash_change_tests` | ✅ update |
| PMAP-02 | lock-stats labels | integration | `cargo test -p slicefs-cli --test lock_stats_report` (feature gated; check how it is run) | ✅ update `:90` |
| PMAP-05 | new store header is format 8 | unit | `cargo test -p slicefs-cli --lib header` | ✅ update |
| PMAP-05 | format 7 refused with the upgrade-doc text; 6 keeps its text; 9 gives M3 | unit + integration | `cargo test -p slicefs-cli --lib header`, `--test store_header_tests` | ❌ Wave 0 for the 7 case |
| PMAP-05 | salt line and flag pairing, M9 reasons, rewrite keeps the line, seed option, M17 | unit + integration | `cargo test -p slicefs-cli` | ❌ Wave 0 |
| PMAP-05 | `v2.3.0` refuses format 8 | by hand, log recorded (see Findings 2) | `slicefs stats <format-8-store>` from a build of the tag | manual |
| PMAP-05 | a store written by the new build mounts, existing mount tests pass | Docker | `scripts/docker-linux-test/run.sh test` and `all` | ✅ |
| PMAP-01 | layout decided and written, numbers cited | doc check | `grep` of `docs/design/store-format.md` for the layout section and the CSV file names | ❌ Wave 3 |
| PMAP-01 | bench modes run and agree with the real final maps | smoke | `cargo test -p prolly-bench`; Docker: `scripts/docker-linux-test/run.sh prolly-bench-smoke` | ✅ update |

### Sampling Rate
- **Per task commit:** `cargo test -p prolly` (rule plan), `cargo test -p metadata` (inode map plan), `cargo test -p slicefs-cli --lib header` (header plan), `cargo test -p prolly-bench` (bench plan).
- **Per wave merge:** `cargo test --workspace` on the host (no mounts), plus `scripts/docker-linux-test/run.sh test`.
- **Phase gate:** full suite green on the host and in Docker, the `prolly-bench-smoke` Docker case, the by-hand `v2.3.0` log recorded, `scripts/safety/unsafe-count.sh` unchanged (no new `unsafe`).

### Wave 0 Gaps
- [ ] `crates/prolly/src/tests.rs`: new goldens, error and agreement tests, version and salt tests; remove `use crate::rule::exp_neg` (line 1) and the float `to_bits` table.
- [ ] `scripts/prolly-table/gen_table.py` and a note on how to regenerate.
- [ ] `crates/slicefs-cli/tests/store_header_tests.rs`: `a_format_7_store_is_refused`, a salt round-trip and refusal tests.
- [ ] `crates/metadata`: a root-record size/offset test.
- [ ] `benchmarks/prolly-bench/tests/smoke.rs`: extend with the five-map batches and the layout mode on a small tree.

## Security Domain

`security_enforcement` is absent from `.planning/config.json`, so it is treated as enabled [VERIFIED: `grep security_enforcement .planning/config.json` found nothing].

### Applicable ASVS Categories

| ASVS Category | Applies | Standard Control |
|---------------|---------|-----------------|
| V2 Authentication | no | — |
| V3 Session Management | no | — |
| V4 Access Control | no | — |
| V5 Input Validation | yes | Strict header grammar for the salt line (exactly 64 lowercase hex, two words, one line, flag pairing, M9 on error); `node::decode` bounds-checks every length and never panics on a crafted node; unknown rule version refused |
| V6 Cryptography | yes | BLAKE3 keyed mode for the salt; 32 bytes from `/dev/urandom`; no hand-rolled hash or PRNG. The boundary hash is not a security boundary, only a distribution; the store hash (BLAKE3-256 or SHA-256) still names every node |

### Known Threat Patterns for this stack

| Pattern | STRIDE | Standard Mitigation |
|---------|--------|---------------------|
| Boundary grinding: a user picks names and inode keys to force many or few cuts | Tampering / Denial of service | Size limits hold: no node above `NODE_MAX` (forced cut), at most one small node per level below `NODE_MIN`, the tree never breaks. Optional per-store salt stops precomputation across stores; it does not stop a user who can read `FORMAT` (it is plain text), so record it as an accepted risk (D-10) |
| Crafted node in a shared or damaged store (decentralized goal) | Tampering | `decode` checks magic, version, sizes, key order, counts; each child is checked against its parent entry; a bad node gives `Error::Corrupt`, never a panic |
| Salt change or loss makes a writer cut with another rule | Tampering | The flag is `ro_compat`; the line is in the header with the flag, validated as a pair; M17 refuses a salt option on an existing store |
| Old binary writes a format 8 store | Tampering | v2.3.0 refuses format 8 at the first line (M3); verified from the tag source |
| Predictable salt (weak randomness) | Information disclosure | `/dev/urandom`, 256 bits; a failed read of fewer than 32 bytes is an error, not a fallback |

## Sources

### Primary (HIGH confidence)
- `crates/prolly/src/{rule,lib,node,tree,apply,tests}.rs` — read in this session (rule, constants, node header, `READ_CFG`, goldens)
- `crates/metadata/src/store.rs` (root_rec Read; commit, load, GC mark, `insert_inode` through `sed`/`grep`), `inode_map.rs` (Read), `rehash/{mod,map}.rs` (Read lines 150-195; fields via `sed`)
- `crates/slicefs-cli/src/header.rs` (Read lines 1-440 and test lines), `git show v2.3.0:crates/slicefs-cli/src/header.rs` (lines 24, 244, 371), `git diff v2.3.0 -- header.rs` empty
- `docs/design/store-format.md` (Read in full), `docs/design/metadata-maps.md`, `docs/research/prolly-trees.md` (sections Read), `benchmarks/prolly-bench/src/{main,replay}.rs` and `tests/smoke.rs`, `scripts/prolly-bench/{bench.sh,README.md}`, `scripts/docker-linux-test/{run.sh,Dockerfile}`, `.github/workflows/ci.yml`
- blake3 1.8.7 source in the local cargo registry (`new_keyed`)
- `.planning/{ROADMAP,REQUIREMENTS,STATE}.md`, `25-CONTEXT.md`

### Secondary (MEDIUM confidence)
- Phase 24 numbers quoted from `docs/research/prolly-trees.md` (Tree shape, Scale, Recommendation); used as inputs for the extrapolation

### Tertiary (LOW confidence / own experiment)
- Scratch script `fp.py` (Python `decimal` table, random sweeps) at `/private/tmp/claude-501/-Users-alexanderfedin-projects-slicefs/443903fe-0300-459b-abda-ef306b861a4f/scratchpad/fp.py`: maximum cut-probability error 3.0e-5 (65 steps), 1.9e-6 (257), 1.2e-7 (1,025); 0 differing decisions in 400,000 trials each for L=4096, 8192, 256. It is not committed; the plan must regenerate and pin the table with a committed generator.

No web search was needed: the phase is internal to this repository and uses no new package.

## Metadata

**Confidence breakdown:**
- Standard stack: HIGH — no new package; blake3 keyed mode confirmed in source
- Architecture (root record, header, rule, node header): HIGH — read from code, v2.3.0 behaviour read from the tag
- Layout winner: MEDIUM-LOW — only a prediction; the bench decides, with a pre-registered rule
- Pitfalls: HIGH — each comes from a read of the code or tests

**Suggested plan split** (for the planner; detail in the Summary):

| Plan | Wave | Content | Depends on |
|------|------|---------|------------|
| 25-01 | 1 | `crates/prolly`: integer rule and table, generator script, rule version byte, 10-byte node header, `Config::new` (L, `NODE_MAX`, salt), separate read limits, tests; provisional constants and a regenerate command for goldens | — |
| 25-02 | 1 | PMAP-02: drop `INODE_MAP`, shrink `InodeMap` to the allocator, root record 197, GC mark, rehash, lock-stats site and test, bench root-record constants | — |
| 25-03 | 1 | PMAP-05: format 8, M2 arms for 6 and 7, `boundary-salt` line and flag, `seed --boundary-salt`, M17, all hard-coded 7/8 tests, upgrade-doc stub | — |
| 25-04 | 2 | Bench: five-map batches, layout (a)/(b)/(b-apart), synthetic 1M/10M multi-map and mixed commits, L sweep, smoke and README | 25-01, 25-02 |
| 25-05 | 2 | By-hand runs (macOS and Docker/Linux; Boost, 1M, 10M; L sweep), CSVs in `runs/`, decision by the pre-registered rule, report to the owner | 25-04 |
| 25-06 | 3 | Pin the final constants and goldens; write `store-format.md` (D-01 target, layout, root record now and after Phase 26, node format, rule, table hash, constants, salt, policy D-13, messages, stats), update `prolly-trees.md`, `metadata-maps.md`, `chunk-store.md:175`, crate docs; reword REQUIREMENTS/ROADMAP only if needed | 25-03, 25-05 |

**Research date:** 2026-11-26
**Valid until:** 2026-12-26 (the code is under active change in this milestone; recheck line references after plan 25-02 lands)
