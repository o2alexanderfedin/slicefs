# Phase 24: Prolly evaluation - Research

**Researched:** 2026-11-26
**Domain:** Rust storage library (prolly tree on `chunk-store`), in-process benchmark on the metadata commit path, written decision
**Confidence:** HIGH for the boundary rule, the baseline commit path and the bench method; MEDIUM for the byte and time estimates (the bench will measure them)

<user_constraints>
## User Constraints (from CONTEXT.md)

### Locked Decisions
Copied from `24-CONTEXT.md` (`## Implementation Decisions`). D-01..D-08 are locked by the owner; D-09..D-29 are the recommended defaults taken in `--auto` mode and are binding for the planner unless this research says otherwise (marked "Change" below).

- **D-01:** Prolly work is a prototype plus a written decision. No migration, no store format change, no change to the default store behaviour in this phase. The prototype is not wired into the root record, `DictMetadataStore`, `slicefs-cli` or any mount path.
- **D-02:** The prototype must not hold a whole map or a whole file in RAM. Memory stays bounded: O(tree depth x max node size + the current batch of edits), never O(map entries). (Owner rule "no whole file in RAM", #98.)
- **D-03:** No store backward compatibility is needed, but this phase must not change the on-disk format anyway. No change to `docs/design/store-format.md` grammar, the FORMAT header, `KNOWN_FLAGS`, the root record (`root_rec`) or the segment records.
- **D-04:** FastCDC default stays 4/8/32 KiB. BLAKE3-256 stays the default hash (full 256-bit IDs, no bit saving). The 1 TiB cap stays.
- **D-05:** Executors run in git worktrees, one per plan (local branch `exec/24-NN`, never pushed). No file is in two plans of the same wave. The workspace `Cargo.toml` (members list) and `Cargo.lock` count as files: only one plan per wave may change them.
- **D-06:** Tests stay short (test-time budget). The benchmark runs by hand or in Linux Docker (`scripts/docker-linux-test/run.sh`), never in the normal test suite. `cargo test` may only build the benchmark binary and run a tiny smoke unit test, if any.
- **D-07:** No local macOS mounts. The benchmark needs no mount: it runs in process on the chunk-store and metadata APIs (as the Phase 22 benchmark does).
- **D-08:** Reversible choices are decided and recorded here. One-way doors are listed for the owner and not decided.
- **D-09:** The prototype is a generic sorted map of byte keys to byte values (u64 keys big-endian). The benchmark uses it for the `inode_data` map (`BTreeMap<u64, ObjectKey>`, one entry of 8 + 33 bytes per inode). The researcher measures the entry counts of all six maps; the planner may switch the target map and records why.
- **D-10:** The unit tests of the crate also use variable-length keys and values (for example `(parent u64, name)` keys).
- **D-11:** A node ends after an entry when a hash of that entry's **key** (never its value bytes) falls under a threshold that grows with the current node byte size (Dolt's rule; hard minimum and maximum node size). The same rule cuts every level; an internal entry is (last key of the child, child ObjectId, entry count of the subtree). Starting point: target about 4 KiB, min about 512 B, max 16 KiB.
- **D-12:** The hash is a fast keyed or plain hash of the key bytes, stable across runs and platforms (no `std` `RandomState`). The researcher picks it and states the cost per key.
- **D-13:** "Boundaries depend only on the keys": with fixed-length values the boundaries are a function of the key set alone; with variable-length values they depend on the key set and the entry byte lengths, never on value content bytes. Tests: (a) same set, different insert orders or bulk versus batches, same root ObjectId; (b) same-length value changes move no boundary; (c) no node above max or (except the last of a level) below min.
- **D-14:** Each node is one chunk-store object (`ChunkStore::put` / `get` / `read`); not cut again by FastCDC. The node byte format is private to the prototype crate and is not a store format.
- **D-15:** The root of a map is one ObjectId. No root record, segment record or header line is written.
- **D-16:** API: (1) build from a sorted iterator, one in-progress node per level (streaming, bounded); (2) `get(key)`; (3) `apply(batch)` with a sorted list of puts and deletes, rewriting only touched leaves and their paths; (4) an ordered iterator that reads one path of nodes at a time. A diff of two roots is deferred.
- **D-17:** No node cache that grows with the map. A small fixed-size cache is allowed if the benchmark shows it matters.
- **D-18:** Prototype size about 1,000 to 1,500 lines with tests. The researcher checks the maturity of existing Rust prolly-tree crates; the default is our own small implementation with no new heavy dependency.
- **D-19:** `crates/prolly` depends on `chunk-store` only (plus a hash crate if D-12 needs one). `metadata` and `slicefs-cli` do not depend on it. The planner checks this in a verify step.
- **D-20:** `benchmarks/prolly-bench` follows `benchmarks/index-bench` and the Phase 22 `chunk_profile_bench`: release binary, CSV to stdout, a work dir, machine info without path, host or user. `scripts/prolly-bench/bench.sh` follows `scripts/chunk-profiles/bench.sh` (usage, env vars, `SMOKE=1`, several runs with a median row, peak RSS by `/usr/bin/time`). A Docker smoke target in `scripts/docker-linux-test/run.sh` is optional.
- **D-21:** Setup: seed Boost 1_84_0 into a new store in process, then a fixed seeded edit script of small commits (K changed files for K in {1, 10, 100, 1000}), then the 1_85_0 tree as one large commit. Deterministic, recorded in the report.
- **D-22:** Two comparisons per commit: (a) whole commit (current `DictMetadataStore::commit` versus the same commit with the chosen map's full serialization replaced by the prolly `apply`); (b) map only (`intern_u64_digest_map` versus `apply` of the same batch). A bench-local copy of the map serializer is allowed; no change to `metadata`.
- **D-23:** Metrics per commit and mode: wall time, CPU user and system (`getrusage`), peak RSS (one process per mode, `/usr/bin/time`), bytes serialized/hashed, new objects and new stored bytes (delta of `ChunkStore::stats()`), and after the script the on-disk size (packs apparent and allocated, index included). Several runs (default 3), median reported.
- **D-24:** The baseline is measured fairly: `store_io` already cuts each map node with FastCDC under a content-cut manifest tree. The report separates "bytes serialized and hashed per commit" from "new bytes stored per commit", and shows the RAM and CPU cost of the full in-RAM maps (`begin_commit` clones every map).
- **D-25:** `docs/research/prolly-trees.md`: method, command lines, machine, inputs, tables, the boundary rule, the Rust crate survey, the result, and the recommendation. States that file manifests are already content-cut trees (`crates/chunk-store/src/file.rs`) and that metadata map nodes are already FastCDC-cut values (`crates/metadata/src/store_io.rs`).
- **D-26:** Design-doc part: a short "Metadata maps: prolly-tree decision" section (new `docs/design/metadata-maps.md` or a section in an existing doc); must not touch the format grammar in `store-format.md`.
- **D-27:** Executors run no `gh` write command. The plan writes the comment text to `.planning/phases/24-prolly-evaluation/24-ISSUE-19-COMMENT.md`; the orchestrator or owner posts it. The last sentence of D-27 in CONTEXT (a wording ban) applies to every new file.
- **D-28:** The recommendation is the deliverable; adopting prolly maps is an owner decision (one-way door). The doc lists what adoption would change (root record, all six maps, GC mark, rehash, snapshots, the format number) and its cost.
- **D-29:** One plan runs the benchmark by hand with a time limit, with the Boost data from `scripts/macos-mount-test/fetch-boost.sh`, and commits the results. If the Boost data cannot be fetched, run `SMOKE=1` and mark the full run as a human-needed item; do not invent numbers.

### Claude's Discretion
Crate, type, function and constant names; the node byte layout; the exact hash; the size-aware CDF and its parameters (within D-11/D-12, from research); the edit-script details (within D-21); whether a small bounded node cache is used (D-17); the design-doc file (D-26); the plan split.

### Deferred Ideas (OUT OF SCOPE)
- Diff of two roots that skips equal subtrees.
- Prolly trees for file manifests with subtree lengths (manifests are already content-cut trees with byte lengths).
- Wiring a prolly map into the root record, GC mark, rehash and snapshots (only after an owner decision Q-1..Q-3).
- A combined namespace map (Q-2).

### Owner questions (not decided here)
Q-1 adopt or not and in which milestone; Q-2 one prolly map per metadata map or one combined namespace map; Q-3 whether the boundary rule and its constants become part of the frozen store format.
</user_constraints>

<phase_requirements>
## Phase Requirements

| ID | Description | Research Support |
|----|-------------|------------------|
| PROLLY-01 | Prototype for one metadata map, separate crate, not wired into the root record; boundaries depend only on the keys with a size-aware rule; nodes are chunk-store objects | Exact Dolt rule and constants (section "Boundary rule"); node and `ChunkStore` limits (D-14 facts); streaming build and apply algorithm; test list in "Validation Architecture" |
| PROLLY-02 | Benchmark on Boost trees compares the current commit path with the prototype: time, CPU, peak memory, bytes written per commit, on-disk size after several small commits; runs outside the test suite | Baseline quantified from code; the three-mode bench design (`whole`, `flat`, `prolly`) that needs no change to `metadata`; seed and reopen APIs; Phase 22 driver pattern |
| PROLLY-03 | Written decision (adopt, later, drop) in the design docs and on #19; states file manifests are already content-cut trees | Facts for the decision (what the baseline already stores, what is O(map size), scaling); the manifest-tree facts of `file.rs`; crate survey |
</phase_requirements>

## Summary

The prototype is a small own implementation: a sorted byte-key map whose nodes are single `ChunkStore` objects, cut by Dolt's size-aware "key splitter" rule. The rule is now pinned from the Dolt source (pinned commit below): a node ends after an entry when `hash32(salt(level), key) / 2^32 < (F(size) - F(size - entry)) / (1 - F(size - entry))` with the Weibull CDF `F(s) = 1 - exp(-(s/4096)^4)`, no cut while the node is under 512 B, a forced cut over 16 KiB, a separate salt for each tree level, and the hash taken over the key only. The size counted is the encoded entry size, so with fixed-width values the boundaries are a function of the key set alone, and with variable-width values they also depend on the entry lengths (D-13). A simulation of the rule on 84,104 fixed-width entries gives 970 leaves of mean 3.7 KB (min 0.7 KB, max 6.8 KB) and about 1.04 rewritten leaves per single insert or delete; a same-width value change rewrites exactly one node per level (about 3 nodes, about 8 KB).

The existing Rust prolly crates do not fit: both of the maintained ones hash and name their own nodes (`ChunkStore::put` names an object by the hash of its bytes, so a caller-chosen node key cannot be stored) and one defines its shape from "contents". The default of D-18 (write our own) is confirmed.

The baseline is already cheap on the disk side and expensive on the CPU and RAM side: each commit serializes four O(N) maps in full (about 10.3 MB for the 84,104 inodes of Boost 1_84_0), runs FastCDC and BLAKE3 over all of it, and `begin_commit` clones five `BTreeMap`s. Only the changed chunks and one manifest leaf per map are stored again. The benchmark must therefore show serialized bytes and CPU separately from new stored bytes (D-24). One finding changes the D-22 method: the `PreparedCommit` fields are private and `intern_u64_digest_map` is private, so the whole commit can only be timed as `begin_commit` plus `finish_commit`, and the single-map cost has to be measured by a bench-local copy of the serializer in its own scratch store (the `flat` mode below). Also `inode_map` and `inode_data` hold identical entries (see "Baseline"), which a stats delta in the real store would hide.

**Primary recommendation:** Build `crates/prolly` with Dolt's exact rule (min 512 B, max 16 KiB, L 4096, K 4, per-level salt, hash over the key only) using BLAKE3 for the key hash and a hand-written deterministic `1 - exp(-x)` (std `exp_m1` is documented as non-deterministic across platforms); measure with three processes per run (`whole` on the real `DictMetadataStore`, `flat` and `prolly` on scratch stores that replay the exact `inode_data` changes of the real commits).

## Architectural Responsibility Map

This phase has no client/server tiers. The responsibilities are:

| Capability | Primary Owner | Secondary | Rationale |
|------------|---------------|-----------|-----------|
| Node format, boundary rule, build, get, apply, iterate | `crates/prolly` (library) | `chunk-store` for object storage | D-19: depends on `chunk-store` only; nothing in the product depends on it |
| Seeding, reopening, edit script, timing, replay | `benchmarks/prolly-bench` (binary) | `slicefs-cli` lib (`seed`, `content`), `metadata` | D-01: copy logic into the bench instead of changing `metadata` |
| Run orchestration, medians, machine info | `scripts/prolly-bench/bench.sh` | `scripts/docker-linux-test/run.sh` (optional smoke) | Phase 22 pattern |
| Decision text | `docs/research/prolly-trees.md`, `docs/design/metadata-maps.md`, `24-ISSUE-19-COMMENT.md` | — | D-25..D-27 |

## Standard Stack

### Core
| Library | Version | Purpose | Why Standard |
|---------|---------|---------|--------------|
| `chunk-store` (workspace path dep) | 0.1.0 | Node storage: `ChunkStore::put/read/stats/sync`, `ObjectId`, `ID_LEN` | D-14; already the object store of files and metadata |
| `blake3` (workspace dep `1`) | workspace pin | Key hash for the boundary rule (first 4 bytes of `blake3::hash(level_byte ++ key)`) | No new package; stable by specification across platforms [ASSUMED: cost 50-100 ns per short key; the bench prints it] |

### Supporting
| Library | Version | Purpose | When to Use |
|---------|---------|---------|-------------|
| `tempfile` (workspace) | workspace pin | Test and bench work dirs | Dev-dependency |
| `proptest` (workspace) | workspace pin | Property test "apply equals bulk build" with a small config | Dev-dependency; keep cases at 32-64 |
| `libc` (workspace) | workspace pin | `getrusage` in the bench | Bench only; needs an `unsafe` entry (see Pitfall 7) |

### Alternatives Considered
| Instead of | Could Use | Tradeoff |
|------------|-----------|----------|
| Own implementation | `prolly-map` 0.7.2 or `prollytree` 0.4.1 | See the crate survey: they name their nodes by their own hash and cannot store nodes as chunk-store objects without a side table; reject |
| BLAKE3 for the key hash | an xxh3 crate (Dolt's choice) | A new dependency for no gain; any such crate name would be `[ASSUMED]` and needs the legitimacy gate |

**Installation:** no new external package. Both new crates use only workspace dependencies.

**Version verification:**
```bash
gsd_run query package-legitimacy check --ecosystem crates blake3 libc tempfile proptest
```
[VERIFIED: gsd-tools package-legitimacy this session] all four returned `OK` (already used in the workspace: `Cargo.toml` `[workspace.dependencies]`).

### Rust prolly crate survey (issue #19 asks for it)

| Crate | Version | Last release | Node storage | Fit |
|-------|---------|--------------|--------------|-----|
| `prolly-map` (lib name `prolly`) | 0.7.2 | 2026-08-22 (first release 2026-07-11, about 10k downloads) | `Store` trait: `get(&[u8])`, `put(&[u8], &[u8])`, `delete`, `batch`; keys are the crate's own CID bytes, node ID is SHA-256 of the node bytes [CITED: docs.rs/prolly-map/0.7.2/prolly/trait.Store.html, docs.rs/crate/prolly-map/0.7.2/source/docs/architecture.md] | Reject: the ID is chosen by the crate (SHA-256), but `ChunkStore::put(data)` returns the ID it computes itself (BLAKE3 tag), so a CID to ObjectId side table would be needed, and that table is mutable state outside the content-addressed store |
| `prollytree` | 0.4.1 | 2026-08-27 (first release 2024-07-07, about 3.7k downloads) | `NodeStorage<const N: usize>` with `get_node_by_hash(&ValueDigest<N>) -> Option<Arc<ProllyNode<N>>>` and `insert_node(&mut self, hash, node: ProllyNode<N>)`, `Send + Sync + Clone` [CITED: docs.rs/prollytree/0.4.1/prollytree/storage/trait.NodeStorage.html] | Reject: it hands over an in-memory `ProllyNode` with the crate's own digest, not bytes to be named by the store; its README says the shape "is a deterministic function of its contents" [CITED: docs.rs/crate/prollytree/0.4.1/source/README.md], which is not the key-only rule of D-11 |
| `prolly` | 0.1.1 | 2022-04-08 | not examined | Reject: no release since 2022 |

[VERIFIED: crates.io API this session] for versions, dates and download counts. [VERIFIED: gsd-tools package-legitimacy this session] flags all three as `SUS` (low downloads), so the planner would need a `checkpoint:human-verify` before any install; none is recommended, so none is installed. The docs pages do not state the boundary rule, streaming build or bounded memory of either crate; that is silence, not a finding, so the rejection rests only on the node-identity mismatch above and on D-18's size argument. **Confirmed: write our own small one.**

## Package Legitimacy Audit

| Package | Registry | Age | Downloads | Source Repo | Verdict | Disposition |
|---------|----------|-----|-----------|-------------|---------|-------------|
| blake3 | crates.io | since 2019 | about 5.3M/wk | github.com/BLAKE3-team/BLAKE3 | OK | Approved (already a workspace dependency) |
| libc | crates.io | since 2015 | about 40.7M/wk | github.com/rust-lang/libc | OK | Approved (already a workspace dependency) |
| tempfile | crates.io | since 2015 | about 21.4M/wk | github.com/Stebalien/tempfile | OK | Approved (already a workspace dependency) |
| proptest | crates.io | since 2017 | about 6.6M/wk | github.com/proptest-rs/proptest | OK | Approved (already a workspace dependency) |
| prolly-map | crates.io | 2026-07 | 242/wk | github.com/crabbuild/prolly | SUS | Not used; surveyed only |
| prollytree | crates.io | 2024-07 | 2/wk | github.com/zhangfengcdt/prollytree | SUS | Not used; surveyed only |
| prolly | crates.io | 2022-04 | 1/wk | github.com/cfkaran2/prolly | SUS | Not used; surveyed only |

**Packages removed due to SLOP verdict:** none.
**Packages flagged SUS:** the three surveyed prolly crates; none is recommended for install.

## Boundary rule (the research flag)

### Dolt's rule, pinned

Source: `dolthub/dolt`, `go/store/prolly/tree/node_splitter.go` and `chunker.go` at commit `d5a833f8df56e35c8fffccb48cfa5d790726aa57` (main on 2026-11-26), read as raw files, not through a summary. Verbatim lines that carry the facts:

- `minChunkSize = 1 << 9` and `maxChunkSize = 1 << 14` (node_splitter.go:35-36): 512 B and 16,384 B.
- `var defaultSplitterFactory splitterFactory = newKeySplitter` (line 42): the key splitter is the default (the rolling-hash splitter in the same file is not used).
- `salt: saltFromLevel(level + 1),` (line 165) and `saltFromLevel` takes the first 8 bytes (little endian) of SHA-512 of the single byte `level` (lines 250-252): a different salt for each tree level.
- `thisSize := uint32(len(key) + len(value))`, `ks.size += thisSize` (Append): the size counted is the key bytes plus the value bytes of the entry, with no framing.
- `if ks.size < minChunkSize { return nil }` (line 175): no cut under the minimum. `if ks.size > maxChunkSize { ks.crossedBoundary = true` (line 178): a forced cut after the entry that pushes the size over the maximum; so a Dolt node can exceed 16 KiB by one entry.
- `h := xxHash32(key, ks.salt)` and `func xxHash32(b []byte, salt uint64) uint32 { return uint32(xxh3.HashSeed(b, salt)) }` (lines 184, 244-246): the hash is xxh3 with the level salt as seed, truncated to 32 bits, over the **key bytes only**.
- `targetSize float64 = 4096`, `K = 4.`, `L = targetSize` (lines 199-206), with the comment `// TODO: seems like this should be targetSize / math.Gamma(1 + 1/K).`
- `weibullCheck(size, thisSize, hash)`: `start = -expm1(-((size-thisSize)/L)^4)`, `end = -expm1(-(size/L)^4)`, `p = hash / maxUint32`, `d = 1 - start`, if `d <= 0` cut, else cut iff `p < (end - start) / d`. In words: the probability to cut at this entry, given no cut before it, is the part of the remaining Weibull mass that the entry covers.
- `chunker.go`: the splitter at every level gets `Append(key, value)`; for an internal level `appendToParent` passes `novel.lastKey` as the key and the child address bytes as the value, and the subtree count goes to the builder but not to the splitter (line 378: `tc.parent.append(ctx, novel.lastKey, novel.addr[:], novel.treeCount)`); an internal node with one item is never split (the `!degenerate` test at line 360); re-synchronisation after an edit is `for !(split && tc.cur.atNodeEnd())` (line 197): the chunker re-appends old items until its new boundary lands on an old node end; `getCanonicalRoot` (line 527) descends through single-child nodes so the root is canonical.

[VERIFIED: dolthub/dolt raw source at the commit above, files node_splitter.go lines 35-42, 165, 175-206, 227-252 and chunker.go lines 64, 197, 352-378, 466, 527]

Derived numbers [VERIFIED: Python model of the rule run in this session]:

- Weibull K=4, L=4096 has mean `L * Gamma(1.25)`, about 3,713 B. The model on 84,104 entries of 43 B (fixed width): 970 leaves, mean 3,728 B, min 731 B, max 6,794 B, 5th percentile 1,935 B, 95th percentile 5,418 B. The 12 KiB mark where a forced cut would apply has survival `exp(-81)`: it never fires in practice, and the 512 B minimum binds with probability about 0.02 %. So the "target 4 KiB" of D-11 gives nodes of about 3.7 KB on average, which is what the planner should expect and write in the doc (not 4 KiB).
- One inserted or deleted fixed-width entry: new leaves per edit, mean 1.04 (insert) and 1.07 (delete), 300 trials each, worst case 5. A changed value of the same width: exactly one leaf (nothing moves).
- Tree shape for 84,104 entries with 41 to 43 B leaf entries and about 52 B internal entries: about 970 leaves, about 14 level-1 nodes, one root: depth 3. One same-width change writes 3 nodes, about 3.7 KB + 3.7 KB + 0.7 KB, about 8 KB raw. The 33-byte digests are incompressible, so stored bytes are about 80 % of that.

### Recommended rule for SliceFS (constants and one comment each)

| Constant | Value | Comment to write next to it |
|----------|-------|------------------------------|
| `NODE_MIN` | 512 B | no cut below this node size (Dolt `minChunkSize`) |
| `NODE_MAX` | 16,384 B | hard bound of a node, header included (Dolt `maxChunkSize`) |
| `WEIBULL_L` | 4096 | scale of the CDF; mean node about 0.906 x L (Dolt `L`) |
| `WEIBULL_K` | 4 | shape; written as `x*x*x*x`, never a `pow` call |
| `MAX_ENTRY` | 2,048 B | limit of one encoded entry, so a node can always hold at least 4 entries and `NODE_MAX` is a real bound |
| `FORCE_AT` | `NODE_MAX - MAX_ENTRY` | cut after any entry that leaves the size above this (see Change 1) |
| salt | one byte `level + 1` prepended to the key | domain separation of the levels (see Pitfall 2) |

**Change 1 (to Dolt, for D-13c):** Dolt cuts after the entry that crosses 16 KiB, so a node can be larger than max. D-13(c) says no node is above max. Use "cut after an entry when `size > NODE_MAX - MAX_ENTRY`", with a hard error `EntryTooLarge` for an entry above `MAX_ENTRY`. Then `size_before <= NODE_MAX - MAX_ENTRY` and `entry <= MAX_ENTRY` give `size <= NODE_MAX` always, and the decision still uses only the bytes since the last cut and the current entry (no look-ahead), so `apply` can restart at the start of any touched node. (A "cut before the entry that would overflow" rule needs the next entry and breaks that restart.) Keep a minimum of two entries per internal node as Dolt does (never cut after the first entry of a level >= 1).

**Change 2 (size counted):** count the encoded entry bytes (length fields included) plus the 9-byte node header as the starting size, so `NODE_MAX` bounds the real object size. With fixed-width values every entry has the same size, so the cut state after `n` entries is a function of the keys alone.

**Key hash (D-12):** `u32` = first 4 bytes little endian of `blake3::hash(&[level + 1] ++ key)`; compare `hash as f64 / 4294967295.0` to the cut probability, as Dolt does. Only compute it when `size >= NODE_MIN` (Dolt does the same; about 12 of the roughly 86 entries of a leaf skip it). The cost per key is a single BLAKE3 compression of at most one 64-byte block for keys up to 63 bytes [ASSUMED: about 50 to 100 ns]; for 84,104 keys that is about 5 to 8 ms of a bulk build and about 20 microseconds for an `apply` of one change. The bench prints `boundary_hash_ns_per_key` once so the report states a measured number.

**Deterministic CDF (important, see Pitfall 1):** Rust documents `f64::exp_m1` as "non-deterministic ... varies by platform, Rust version" [CITED: doc.rust-lang.org/std/primitive.f64.html#method.exp_m1], and node IDs depend on every cut. Write the cut test as `p < 1 - exp(-delta)` with `delta = (end^4 - start^4) / L^4` where `end^4 - start^4` is exact in `u64` (`end <= 2^14`, so `end^4 <= 2^56`) and the single division is an IEEE operation; compute `exp(-x)` with a short loop of `+ - * /` only (range reduction by halving `x` k times, a Taylor series of about 12 terms, then square k times; for `x >= 40` the answer is "cut"). IEEE basic operations are bit-exact on all platforms, and Rust does not fuse multiply-add. Absolute error of about 1e-15 is far below the 2^-32 hash resolution. Add a golden test: a fixed key set gives a fixed root ObjectId hex and a fixed table of cut decisions. If prolly maps are ever adopted, the exact rule becomes part of the frozen format (owner question Q-3), and the planner of that phase should consider replacing the float by a fixed-point table.

**Why "boundaries depend only on the keys" holds (D-13):** the cut decision after entry `i` of a node is a function of `hash(level, key_i)` and of the byte size of the entries since the last cut. For fixed-width values that size is `header + n * width`, so it depends on the number of entries since the last cut, so the whole sequence of cuts is a function of the ordered key sequence. A put that keeps the width moves no boundary. With variable-width values the size also contains value lengths: a value that grows can move that node's cut and the next few, until a cut falls on the same key in the old and the new sequence (then every later node is identical, because every node starts from size zero). The hash never reads value bytes, so content changes of equal length move nothing. An insert or delete shifts the later sizes of its own node only until the first shared cut (about 1.04 extra leaves on average in the model).

**Internal levels:** the same splitter runs at every level on the entry (last key of the child, child ObjectId, subtree count). The child ID and the count have fixed width, so an internal boundary depends only on the last keys of the children and their byte lengths: a changed child ID with the same last key moves nothing at the level above.

### Algorithms (all stream; none holds a map)

**Bulk build from a sorted iterator:** `levels: Vec<Builder>`; a `Builder` holds the entries of the node in progress (at most `NODE_MAX` bytes) and the splitter state `(size, count)`. `push(level, entry)`: add it; if the splitter says cut, encode the node, `ChunkStore::put` it, then `push(level + 1, (last_key, id, subtree_count))`. `finish`: for `level = 0..`, if the builder is non-empty: if it is the only node ever emitted at this level and no higher builder has entries, its node is the root; else emit it and push it up. A cut that falls after the very last entry of the stream creates no empty trailing node. Memory: `depth x NODE_MAX` plus one encoded node. A test counter (`high_water_bytes`, maximum of the sum of all builder bytes) is the evidence for D-02.

**Canonical root (Pitfall 3):** an empty map has one fixed root (an empty leaf node object, or `None`; pick one and test it); a root that is an internal node with a single child is replaced by that child, repeatedly, exactly as Dolt's `getCanonicalRoot`. Without this, bulk build and `apply` give different roots for the same set.

**`apply(batch)` (sorted puts and deletes):** one function for every level. `rewrite_level(level, edits) -> edits_for_level+1`, where an edit is "put (key, value)" or "delete key", sorted by key. Walk the old nodes of the level with a path cursor (a stack of `(node, index)`, one node per level in RAM): for each touched node, merge its entries with the edits, re-run the splitter from the start of that node, and keep feeding entries from the following old nodes until a new cut lands on an old node end (or both streams end): from there the rest of the level is identical. The result for this level is a list of replaced node ranges; convert it to edits for the level above, also keyed (delete the old last key of each replaced node, put the new `(last_key -> child id, count)`). Because an internal entry is itself a keyed entry, the same routine runs at every level. After the top level: if the level now has more than one node, build one more level above with the bulk builder; if exactly one node, collapse single-child roots. Read each old node once (a sorted sweep), not once per edit. Memory: O(touched nodes x node size), i.e. O(changes x depth), as D-02 allows. Return an `ApplyStats { nodes_read, nodes_written, bytes_written }`; the tests and the bench use it.

**Correctness guard:** bulk build and `apply` must call one shared `Splitter` type; the property test "random map, random batches: `apply` root equals bulk-build root of the final set" is the proof that they agree.

**`get(key)`:** descend; at each internal node pick the first entry whose last key is `>=` the key (binary search over decoded entries). **Cursor:** a stack of `(node, index)`; `next()` steps the index and refills from the parent; at most `depth` nodes in RAM.

### Node format (private to the crate, not a store format)

Same shape as the manifest node in `file.rs`: 4-byte magic, level `u8`, entry count `u32` LE (9-byte header); leaf entry `klen u16, vlen u16, key, value`; internal entry `klen u16, key, child ObjectId (33 B), subtree count u64`. Decode must return an error (never panic) on a wrong magic, wrong count, unsorted keys or a child at the wrong level (`file.rs` has a test of this kind, `a_malformed_tree_is_an_error_not_a_panic`, [VERIFIED: file.rs:759]).

## Chunk store facts for one object per node (D-14)

- `ChunkStore::put(&self, data: &[u8]) -> Result<ObjectId>` returns the ID computed by the store hasher; identical bytes are stored once. Object size limit: [VERIFIED: crates/chunk-store/src/lib.rs:1653-1654] `if data.len() > u32::MAX as usize - REC_HEADER {` / `return Err(Error::TooLarge(data.len()));` with `const REC_HEADER: usize = ID_LEN + 9;` (line 374), so about 4 GiB; a 16 KiB node is far below it.
- Objects under `const MIN_COMPRESS: usize = 64;` (line 410) are not compressed; zstd level 3 is the default (`zstd_level: 3`). Digest-heavy nodes compress little (the 33-byte digests are random), keys compress well.
- `read(&ObjectId) -> Result<Vec<u8>>` verifies the hash on each read (about 4 microseconds for 3.7 KB). `get` returns `Option`.
- `ChunkStore::stats()` returns `Stats { objects, raw_bytes, stored_bytes, packs }` with the doc "Distinct objects in the store." (lib.rs:329-338); the counters change in `add_new` only (lines 3651-3656), so a put of bytes already in the store adds 0. So a `stats()` delta around an operation is "new objects, new raw bytes, new stored bytes". `stored_bytes` is the payload only; each object also has a record header of `REC_HEADER` = 42 bytes (and a CRC), so report `stored_bytes + 42 * objects` as the record bytes, plus the real file sizes after the run.
- Tests can use `ChunkStore::open_with(Arc::new(MemIo::new()), Options::default())`: `chunk_store::mem::MemIo` is `pub mod mem;` (lib.rs:44) and `file.rs` tests open stores this way (file.rs:772). No disk, fast.
- The store hasher: `ChunkStore::hasher()` (lib.rs:2992); default `Blake3_256` tag `0x1e`, `ObjectId` is 33 bytes (`ID_LEN`).

## Baseline: the current commit path, quantified from code

### What a commit does [VERIFIED: crates/metadata/src/store.rs:1049-1092, 1105-1149]

`begin_commit` clones, under the commit lock: `inode_map` (`InodeMap`), `inode_data`, `dir_data`, `manifest_data`, `xattr_data` (each `BTreeMap<u64, ObjectKey>`) and `refcounts` (`BTreeMap<ObjectKey, u64>`). `finish_inner` then writes the dirty inodes and dirty directory lists, and interns all of these in full:

```rust
let inode_map_digest = intern_inode_map(&mut w, &p.inode_map);
let inode_data_digest = intern_u64_digest_map(&mut w, &p.inode_data);
let dir_data_digest = intern_u64_digest_map(&mut w, &p.dir_data);
let manifest_data_digest = intern_u64_digest_map(&mut w, &p.manifest_data);
let xattr_data_digest = intern_u64_digest_map(&mut w, &p.xattr_data);
let refcount_data_digest = intern_key_u64_map(&mut w, &p.refcounts);
```

then a 230-byte root record, `cs.sync()`, `pre_root_sync`, and the log root. Each `put_node` is `store_io::put_value`: a `FileWriter` that FastCDC-cuts the bytes (4/8/32 KiB), `put`s each chunk (dedup check by index lookup), and builds the manifest tree (`NODE_MIN` 64 entries to `LEAF_ENTRIES` 4096, cut when a chunk ID matches `NODE_CUT_ONE_IN` 512; file.rs:32-47). [VERIFIED: file.rs:32-40 verbatim: `pub const LEAF_ENTRIES: usize = 4096;`, `pub const NODE_MIN: usize = 64;`, `pub const NODE_CUT_ONE_IN: u32 = 512;`]

### Findings that change the method

1. **`inode_map` and `inode_data` hold the same entries.** In `finish_inner` the two lines `p.inode_data.insert(*ino, d);` and `p.inode_map.insert(*ino, d);` (store.rs:1112-1113) write the same `(ino, digest)` pair, and `insert_inode`/`delete_inode` update both. Their serializations differ only by the 8-byte count prefix of `inode_data` (`serialize_inode_map` writes bare 41-byte records, inode_map.rs:~104-110; `intern_u64_digest_map` writes `[count u64][41-byte entries]`, store.rs:1636-1644). FastCDC cuts by content, so most chunks of the two maps are the same objects and dedup against each other. Two consequences: (a) a `stats()` delta around "the `inode_data` part of a real commit" reads close to 0 because `inode_map` was interned just before; so the single-map baseline must be measured in a scratch store that holds only that map (the `flat` mode); (b) a design note for the report: one of the two maps is redundant before any prolly question.
2. **Private fields and functions:** the `PreparedCommit` fields (store.rs:276-300) have no `pub`, and `intern_u64_digest_map` (store.rs:1636) is a private `fn`. `begin_commit` and `finish_commit` are `pub`. So from outside `metadata` one can time `begin_commit` (the map clones) and `finish_commit` (interning plus sync plus root) separately, but not the interning of one map. The single-map number comes from a bench-local copy of `intern_u64_digest_map` (about 8 lines; `store_io::put_value` and `store_io::get_value` are `pub`). This is the "copy in the bench crate" of D-22 and needs no change to `metadata`.
3. **`inode_data` has no public accessor.** The public route is the root record: `store_io::get_value(&cs, &root_key)` returns the 230 bytes; the layout is [VERIFIED: store.rs:38-52, `root_rec`] `INODE_MAP = 0`, `ROOT_DIR_INO = INODE_MAP + D`, `NEXT_INO = ROOT_DIR_INO + 8`, `INODE_DATA = NEXT_INO + 8`, `DIR_DATA = INODE_DATA + D`, `MANIFEST_DATA = DIR_DATA + D`, `XATTR_DATA = MANIFEST_DATA + D`, `REFCOUNTS = XATTR_DATA + D`, `LOGICAL_BYTES = REFCOUNTS + D`, `INODE_COUNT = LOGICAL_BYTES + 8`, `SIZE = INODE_COUNT + 8` with `/// Total size in bytes (230 with 33-byte keys).` With `D` = 33 the offsets are 0, 33, 41, 49, 82, 115, 148, 181, 214, 222 and 230. Copy them into the bench with a start-up self check: root length 230, and the entry count read from the `inode_data` node equals the `u64` at offset 222. (`root_rec` is `pub(crate)`, so it cannot be imported.) The map node is `[count u64 LE]` plus entries `[ino u64 LE][ObjectKey 33 B]` (`MAP_ENTRY = 8 + KEY_BYTES`, store.rs:55).
4. **`run_seed_with` cannot add a second version.** It makes a new `DictMetadataStore::new` each call (seed.rs), and `walk_dir`, `seed_file`, `file_inode_meta` and `dir_inode_meta` are private. The 1_85_0 step therefore needs a bench-local tree sync (about 100 to 150 lines, see Benchmark design); the fallback is seeding 1_85_0 into a second fresh store and comparing a bulk build against a flat intern of that map (useful as its own row either way).
5. **Inode numbers only grow** (`InodeMap::allocate_ino`, next starts at 2): creates append at the right edge of `inode_data`, while content or metadata changes on existing files are scattered. The edit script must say which it uses (D-21): the recommended script has both patterns (`scatter` and `cluster`).
6. **Seeding leaves `refcounts` and `xattr_data` empty.** `increment_refcount` is called only from `crates/slicefs-cli/src/filesystem.rs` (lines 2522, 2525 and 2272 for decrement), not from `seed.rs`, and seed sets no xattrs. So on a seeded Boost store those two maps are 8-byte nodes.

### Entry counts and bytes per commit (Boost 1_84_0, seeded)

The seed makes one inode per regular file and per directory (the root directory is inode 1) and one `manifest_data` entry per regular file. Counts of the local Boost trees [VERIFIED: `find` on the Phase 22 data this session; no symlinks in either tree]:

| Version | Files | Directories (root included) | Inodes (`inode_map`, `inode_data`) | `dir_data` | `manifest_data` | `xattr_data`, `refcounts` |
|---------|-------|-----------------------------|-----------------------------------|------------|-----------------|---------------------------|
| 1_84_0 | 76,801 | 7,303 | 84,104 | 7,303 | 76,801 | 0, 0 |
| 1_85_0 | 77,655 | 7,425 | 85,080 | 7,425 | 77,655 | 0, 0 |

These counts come from the file system and from reading `seed.rs`; they are **not** read back from a seeded store. The bench must print the real entry counts at start (from the node lengths; `FileReader::open(cs, id)?.len()` gives a node's byte length without reading it) and the report quotes those. Correction to CONTEXT: the "about 156,000 inodes, 6.4 MB per map" figure is two versions together; one store holds one version, so each big map is 84,104 entries, 3.45 MB.

Serialized and hashed per commit at 84,104 inodes (41-byte entries, calculated):

| Map | Bytes |
|-----|-------|
| `inode_map` | 84,104 x 41 = 3,448,264 |
| `inode_data` | 8 + 3,448,264 = 3,448,272 |
| `dir_data` | 8 + 7,303 x 41 = 299,431 |
| `manifest_data` | 8 + 76,801 x 41 = 3,148,849 |
| `xattr_data`, `refcounts` | 8 each |
| root record | 230 |
| **Total** | **about 10.34 MB, about 1,260 FastCDC chunks (8 KiB average), each one `put`** |

`docs/design/chunk-store.md` states FastCDC 8 KiB plus BLAKE3 "does 530 to 910 MB/s" [CITED: docs/design/chunk-store.md, "Why"], so the hashing and cutting alone is about 11 to 20 ms of CPU per commit at this size, linear in the number of inodes (about 1.2 GB per commit at 10 million inodes). `inode_data` alone is a third of it. A prototype for one map therefore removes about a third of the serialization; the report must say so and must not present the single-map number as the whole-commit saving. The five cloned maps in `begin_commit` are about 84k-entry `BTreeMap`s [ASSUMED: about 4 MB each with node overhead; the bench measures the RSS step at the first commit].

New stored bytes per commit (the current path), by reasoning; **the bench measures this** [ASSUMED]: for K changed inodes a flat 41-byte-entry map changes at most K FastCDC chunks (8 KiB average, about 200 entries each) plus the manifest leaf node of that map (about 420 chunk entries x 41 B = about 17 KB, stored again whole because one entry changed). For K = 1 that is about 25 KB per changed map (the changed chunk is shared by `inode_map` and `inode_data`, but their manifest leaves differ), so about 40 to 65 KB per commit for a metadata-only change and more when `manifest_data` also changes. For K = 1000 scattered changes, almost every chunk of the map is touched (1000 changes over about 420 chunks), so the whole map is stored again, about 3.4 MB per map. A prolly map under the same scatter touches about `min(K, leaves)` leaves of about 3.7 KB and the nodes above them. This is why the script must have both `scatter` and `cluster` patterns and why K up to 1000 matters: the two designs cross over where K approaches the number of leaves (about 970).

Directory lists are separate per-directory flat nodes keyed by `name_key` (a hash; `directory.rs:54-68`), written only when dirty. The largest Boost directory has 1,155 entries (about 75 KB), so a directory commit is not O(inodes) on Boost; it is a reason not to pick the directory map as the single target.

### Why `inode_data` stays the target (D-09)

It is the largest map with `inode_map` (84,104 entries), every inode change touches it, its values are fixed width (exact D-13 statement), and its keys are small (8 bytes). `manifest_data` (76,801 entries) is nearly as large and changes only on content edits; `dir_data` has 7,303 entries. Keep `inode_data`; add `manifest_data` as a second target only if the bench harness makes it nearly free (it has the same entry format).

## Benchmark design

### Three modes, three processes per run (D-22, D-23, no change to `metadata`)

| Mode | What it runs | Store | Gives |
|------|--------------|-------|-------|
| `whole` | The real `DictMetadataStore` on a copy of the seeded store: edit script, then `begin_commit` and `finish_commit` timed separately | the seeded store copy | whole-commit wall and CPU, `begin_commit` time and RSS step, `stats()` delta, on-disk growth, the root of each commit (appended to `roots.txt`) |
| `flat` | Bench-local copy of `intern_u64_digest_map` over an in-RAM `BTreeMap<u64, [u8; 33]>` of `inode_data`; replays the real changes | a fresh scratch store with only this map | the "current design, one map" cost: serialized bytes, CPU, RSS with the map in RAM, new objects and bytes, on-disk growth |
| `prolly` | `crates/prolly` bulk build, then `apply` per batch; `cs.sync()` once per commit to match the sync of a real commit | a fresh scratch store with only this map | the prototype cost: nodes read and written, bytes, CPU, RSS (no `DictMetadataStore` open in this process), on-disk growth |

Untimed helper `batches --store S --roots roots.txt --out DIR`: for consecutive roots it reads the two `inode_data` nodes by streaming (`FileReader` in 1 MiB pieces and a merge-diff, bounded memory) and writes one sorted batch file per commit (`put ino value` or `del ino`) plus `base.bin` (the initial sorted entries). `flat` and `prolly` replay these files. This ties both map-only modes to the exact `inode_data` changes of the real commits (real digests, real creates and deletes), and keeps the `prolly` process free of the `DictMetadataStore` maps, so its peak RSS is honest (the RSS of `whole` and `flat` contains their maps; that is the cost being compared).

Whole-commit comparison (D-22a) is reported as three numbers: measured `whole`; the arithmetic estimate `whole - flat_intern(inode_data) + prolly_apply` labelled "estimated"; and the components. Peak RSS is per process (`/usr/bin/time`, as Phase 22) plus `ru_maxrss` read after load and after each commit (a high-water mark, so only increases are meaningful: the step at the first commit is the `begin_commit` clone cost).

### Edit script (deterministic, seeded; D-21)

- Setup, untimed: seed `boost_1_84_0` with `slicefs_cli::seed::run_seed_with(store, tree, None, None)` (default hash and profile); copy the seeded store for each run (`cp -R`, a clone on APFS) so seeding (minutes on the full tree) happens once per `WORK`. Reopen: `metadata::segment::load_store_from_segments(&store.join("segments"))` gives `(Option<ObjectKey>, snapshots)` (the root), `slicefs_cli::content::store_io(store)` (pub, header check included) gives the `StoreIo`, `DictMetadataStore::load_from_root(Arc::new(Mutex::new(io)), &root)`. This is exactly what `crates/slicefs-cli/src/snapshot.rs:63-72` does. Without a WAL, `commit()` works (begin_commit sees `wal = None`). Call `set_pre_root_sync` as `seed.rs` does so the content sync is in the timed commit.
- A once-made "bench" directory (`create_directory(1, "bench", ..)`) holds created files, so deletes can name their parent and name.
- Public mutation API on `DictMetadataStore` (trait `MetadataStore` plus pub inherent methods) [VERIFIED: store.rs:1403-1560, 821, 1348-1375]: `create_inode`, `create_directory`, `link(parent, name, ino)`, `unlink(parent, name)`, `delete_inode`, `get_inode`, `update_inode`, `modify_inode(ino, |m| ..)`, `set_manifest`, `prepare_manifest` + `set_manifest_digest`, `commit()` (= `begin_commit` + `finish_commit`). Edit kinds per commit of K edits, in equal shares: (1) metadata change: `modify_inode` mtime or mode of an existing file inode; (2) content change: new content via `slicefs_cli::content::put_reader` (returns the content `ObjectKey` and size), then `set_manifest` and `modify_inode` size and mtime; (3) create: `create_inode` + `link(bench_dir, name, ino)` + `set_manifest`; (4) delete: `unlink` + `delete_inode` of a file this script created earlier. Existing inodes are picked as random numbers in `[2, next_ino)` and resampled until `get_inode(ino).mode` is a regular file.
- Patterns: `scatter` (K random inodes) and `cluster` (K consecutive inode numbers from a random start). Reason: scattered K = 1000 touches nearly every FastCDC chunk of a flat map (see above) but also about 630 of 970 prolly leaves; clustered changes show the structural-sharing case.
- Commits: 10 per (K, pattern) for K in {1, 10, 100, 1000}, then the version step. Record per commit: kind counts, entries changed in `inode_data` (from the batch diff), seconds, CPU, RSS, `stats()` delta, bytes serialized (sum of the six map node lengths).
- **Version step 1_84_0 to 1_85_0 as one commit:** the bench copies about 100 to 150 lines: walk `boost_1_85_0` in sorted order next to `list_directory` of the store directory; a file with the same size and mtime is left alone; a changed file gets new content (`put_reader`), `modify_inode`, `set_manifest`; a new file or directory gets `create_inode`/`create_directory`, `link`, `set_manifest`; a name only in the store is `unlink`ed and its inodes `delete_inode`d (recursively for directories). These private helpers of `seed.rs` (`file_inode_meta`, `dir_inode_meta`) are copied, not exported (D-01; the exception "a tiny `pub` addition" is not needed). The bench prints how many inodes changed. The fallback is described in Finding 4 above.
- Optional `scale` rows (no Boost needed): generate N = 100k, 1M, 4M sorted synthetic entries; time `flat` intern versus `prolly` build and one-entry `apply`, with RSS. This shows O(N) against O(depth) and is the data for the "metadata does not need to fit in RAM" claim of #19. Keep N at most 4M so the in-RAM `flat` map stays under about 1 GB.

### Metrics, timing and hooks (no `unsafe` surprises)

- Wall: `std::time::Instant` around `begin_commit` and `finish_commit` separately.
- CPU and peak RSS in process need `libc::getrusage`, which is `unsafe`. Copy the three helpers `cpu_secs`, `max_rss_bytes` (and `current_rss_bytes` if wanted) from `benchmarks/index-bench/src/main.rs:156-176` ([VERIFIED: lines 157-173] `fn cpu_secs() -> f64 { let mut ru: libc::rusage = unsafe { std::mem::zeroed() }; unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) }; ...`; `ru_maxrss` is bytes on macOS and KiB on Linux).
- **Gate consequence (Pitfall 7):** `scripts/safety/unsafe-count.sh` counts every `unsafe` token (comments too) in `crates` and `benchmarks` and compares it with the `- count: <path> <n>` lines of `docs/design/unsafe-review.md` (it runs inside `scripts/safety/clippy.sh`, a CI gate). A new `benchmarks/prolly-bench/src/main.rs` with `unsafe` needs a review entry and a `- count: benchmarks/prolly-bench/src/main.rs N` line there, in the same plan; `crates/prolly` must contain **no** `unsafe` token at all, not even in a comment.
- `ChunkStore::stats()` deltas before and after each commit give new objects, raw bytes and stored bytes (dedup'd puts add 0, see above). After the script, sum `metadata().len()` and `blocks() * 512` over the files of the store dir (copy `files_of` and the loop of `chunk_profile_bench.rs:384-391`; the on-disk size includes dead nodes: no GC runs, so say "before GC" in the report).
- Per-commit CSV columns (suggested): `mode,run,pattern,k,commit,entries_changed,serialized_bytes,begin_ms,finish_ms,total_ms,cpu_user_s,cpu_sys_s,maxrss_bytes,new_objects,new_raw_bytes,new_stored_bytes,new_record_bytes,nodes_read,nodes_written`; then per run `apparent_bytes,alloc_bytes,files`; then `run=median` rows with `median_low` per column (copy the Python block of `scripts/chunk-profiles/bench.sh`, which needs `python3`: Python 3.9.6 is present locally).
- Use the Phase 22 driver pattern: `scripts/prolly-bench/bench.sh <data-dir> [runs]`, env `WORK`, `BIN`, `RUNS` (3), `SMOKE`, `BOOST_VERS`, plus `KS` ("1 10 100 1000"), `COMMITS` (10), `PATTERNS` ("scatter cluster"); the script refuses a missing data dir, refuses `SMOKE=1` with a data dir argument, makes its store dirs with `mktemp -d` under `WORK`, uses `/usr/bin/time -l` on macOS and `-v` on GNU time (`NA` otherwise), warms the page cache by reading the trees once, writes `results.csv`, `batches.csv`, `machine.txt` (no path, host or user). [VERIFIED: scripts/chunk-profiles/bench.sh read in full this session.]

### SMOKE data generation (Phase 22 pattern)

`scripts/chunk-profiles/bench.sh` builds its smoke data with its own binary: `"$BIN" gen tree --dir "$D/x/boost_$1" --files 200 --seed 1` and `... --seed 1 --change-every 10` for the second version; `gen_tree` makes N files in 16 subdirectories `dNN/fNNNNNN.bin` with random sizes up to `--max-bytes` (default 262,144) and a per-file generator, so a change in one file moves nothing else (`chunk_profile_bench.rs:202-250`). For this phase copy `gen tree` (about 40 lines, xorshift RNG) into `prolly-bench` as `gen tree`, with `--max-bytes 4096` and about 2,000 files so the smoke runs in seconds and the tree has two levels at the default constants (2,000 inodes x 43 B = 86 KB = about 23 leaves). SMOKE checks plumbing only; add one test with the small config in `crates/prolly` for depth. The smoke refuses a data dir, uses `BOOST_VERS="0_0_1 0_0_2"` and `x/boost_<v>` layout like Phase 22. The optional Docker target follows `chunk-profiles-smoke` in `scripts/docker-linux-test/run.sh:73-85` (no FUSE, no `RUN`, `cargo build -q --release --locked -j 1 -p prolly-bench`, then `SMOKE=1 RUNS=1 WORK=/tmp/pp BIN=/target/release/prolly-bench timeout 600 bash /src/scripts/prolly-bench/bench.sh`); the docker image has no GNU `time`, so peak RSS reads `NA` there (as in Phase 22).

### Dependencies of the bench crate

`benchmarks/prolly-bench` depends on `prolly`, `chunk-store`, `metadata`, `slicefs-traits` and `slicefs-cli` (the library target; `slicefs-cli` has `[lib]` behavior through `src/lib.rs`, `pub mod seed;` and `pub mod content;`, [VERIFIED: crates/slicefs-cli/src/lib.rs]) and `libc`. This pulls the vendored `fuser` into the bench build; the workspace already builds it on macOS and Linux CI. D-19 still holds: `metadata` and `slicefs-cli` do not depend on `prolly`.

## Architecture Patterns

### System architecture (data flow of the benchmark)

```
Boost tree 1_84_0 --run_seed_with--> seeded store S0
                                        |  cp (clone) per run
                                        v
 edit script (seeded RNG) --> [whole] DictMetadataStore: begin_commit | finish_commit   --> results.csv (whole rows)
                                 |  root_k appended to roots.txt
                                 v
                    [batches] stream-diff inode_data(root_k-1, root_k)  (untimed)
                       |  base.bin + batch_k files (sorted put/del)
          +------------+-------------+
          v                          v
   [flat] in-RAM map +          [prolly] bulk build once,
   intern copy -> scratch       apply(batch_k) -> scratch
   store F                      store P, cs.sync()
          |                          |
          +--------> results.csv rows (time, CPU, RSS, new bytes, nodes) ; on-disk size per store
```

### Recommended project structure
```
crates/prolly/
├── Cargo.toml          # depends on chunk-store, blake3; dev: tempfile, proptest
└── src/
    ├── lib.rs          # docs: the rule, constants with one comment each, the node format; exports
    ├── cdf.rs          # deterministic cut_probability(size_before, size_after) and exp
    ├── splitter.rs     # Splitter { size, count }: the one rule used by build and apply
    ├── node.rs         # encode/decode, entry limits, errors (no panic on bad bytes)
    ├── build.rs        # streaming bulk builder, BuildStats { high_water_bytes, .. }
    ├── apply.rs        # path cursor, rewrite_level, canonical root, ApplyStats
    ├── cursor.rs       # ordered iterator, get
    └── tests.rs        # (or tests/) all tests with MemIo
benchmarks/prolly-bench/
├── Cargo.toml
└── src/main.rs         # subcommands: gen, seed, whole, batches, flat, prolly, scale
scripts/prolly-bench/{bench.sh,README.md}
docs/research/prolly-trees.md
docs/design/metadata-maps.md
```

### Pattern: one splitter, two callers
The build and `apply` code must call one `Splitter::push(level, key, entry_len) -> Cut` so they cannot diverge; keep its state to `(size, count)`.

### Anti-patterns to avoid
- **Reading all keys to decide boundaries.** Any "collect then chunk" step breaks D-02. The splitter sees one entry at a time.
- **A cut rule that needs the next entry.** It blocks restarting at a node start (see Change 1).
- **Using `metadata::store_io::put_value` for nodes.** It FastCDC-cuts a 4 KB node into one chunk plus a manifest object; D-14 says one object per node, so call `ChunkStore::put` directly.
- **Timing the whole commit and the map by subtraction of two different runs.** Use the three-mode design; label every estimate.

## Don't Hand-Roll

| Problem | Don't Build | Use Instead | Why |
|---------|-------------|-------------|-----|
| Object storage, dedup, compression, packing | a node file store | `ChunkStore::put/read/stats/sync` | D-14; stats deltas give the bytes metric |
| Key hash | a custom hash | `blake3::hash` on `level_byte ++ key` | stable by specification, already a dependency |
| Seeding Boost | a copy of the walker for setup | `slicefs_cli::seed::run_seed_with` | exact production path |
| Reopening the store | manual parsing of the store | `load_store_from_segments` + `content::store_io` + `DictMetadataStore::load_from_root` | what `snapshot.rs` does |
| CPU and RSS helpers | new `unsafe` code | copy the index-bench helpers (their review entry is the template) | one pattern, one review |
| Median rows, machine info, SMOKE gen | new tooling | copy `scripts/chunk-profiles/bench.sh` and `gen tree` | proven; same CSV rules |
| Boost download | curl lines | `scripts/macos-mount-test/fetch-boost.sh DIR 1_84_0 1_85_0` | Phase 22 data |

**Key insight:** everything with a trap (the float CDF, the root canonicalisation, the `unsafe` count gate, the duplicated map) is a trap of the details, not of the idea; copy proven helpers and spend the effort on the splitter and the apply resync.

## Common Pitfalls

### Pitfall 1: Platform-dependent cut decisions (float `exp`)
**What goes wrong:** the same key set gives different root IDs on macOS and Linux. **Why:** `f64::exp_m1` and `exp` are documented non-deterministic [CITED: doc.rust-lang.org/std/primitive.f64.html#method.exp_m1]; a one-ulp difference flips a cut when `p` is within an ulp of the threshold (rare, but each flip changes every ID above it). **Avoid:** the exact-integer `delta` and the basic-ops-only `exp` of "Deterministic CDF"; a golden-vector test run on both platforms (the Docker smoke target covers Linux). **Warning sign:** the golden root hex differs between machines.

### Pitfall 2: Reusing one salt for every level
**What goes wrong:** the leaf-ending keys are exactly those whose hash was low, so an internal level that hashes the same key with the same salt gets biased hashes, and nodes at level 1 cut after the minimum every time (degenerate wide-then-narrow trees). **Avoid:** the per-level salt (Dolt: `saltFromLevel(level + 1)`), tested by building 200k synthetic keys and checking the level-1 mean node size is within a factor of 2 of the leaf mean.

### Pitfall 3: Non-canonical roots
**What goes wrong:** bulk build and `apply` give different roots for one set: a root with a single child after deletes, an empty trailing node when a cut falls on the last entry, or an empty map with two representations. **Avoid:** the canonical rules above and the property test (apply equals bulk) including "delete down to 0, 1, 2 keys".

### Pitfall 4: `apply` that does not resync
**What goes wrong:** re-chunking after an edit runs to the end of the level (O(N)), or stops too early and the tree differs from the bulk build. **Avoid:** stop only when a new cut lands on an old node end, or both streams end; test with an insert at the very first key and at the last key, and with a delete that removes a whole node.

### Pitfall 5: Entries larger than a node
**What goes wrong:** a 255-byte name plus value is fine, but a key or value of tens of KB makes a node larger than `NODE_MAX` or leaves an internal node with one entry. **Avoid:** `MAX_ENTRY` and `Error::EntryTooLarge`; the rule that an internal node never cuts after its first entry.

### Pitfall 6: Hidden dedup hides the baseline cost
**What goes wrong:** the `inode_data` stats delta in the real store is near 0 because `inode_map` has the same chunks (Finding 1), which makes the baseline look free. **Avoid:** the scratch-store `flat` mode; report the whole-commit delta separately and say why.

### Pitfall 7: The unsafe-count gate and the clippy gate
**What goes wrong:** CI job `clippy` (`scripts/safety/clippy.sh`: `cargo clippy --locked --workspace --all-targets -- -D warnings -D clippy::allow_attributes_without_reason`, then every `allow`/`expect` needs `reason = "..."` on the same line, then `unsafe-count.sh`) fails on a new `unsafe` with no review entry, on an `#[allow(...)]` with no reason, or on `Cargo.lock` out of date (`--locked`). **Avoid:** zero `unsafe` in `crates/prolly`; bench `unsafe` counted in `docs/design/unsafe-review.md` (same plan as the bench); a reason on every allow; commit the updated `Cargo.lock` in the plan that adds a member.

### Pitfall 8: Measuring RSS in a process that holds unrelated state
**What goes wrong:** the prolly process is charged for the seeded maps. **Avoid:** one process per mode and the batch-file replay; `prolly` never opens `DictMetadataStore`.

### Pitfall 9: A big target dir makes every test binary slow
**What goes wrong:** a start of a test binary takes seconds when `target/debug/deps` has hundreds of thousands of files (noted in project memory). **Avoid:** keep cargo runs minimal; use `timeout` and run `cargo clean` before suspecting a hang.

## Code Examples

### Splitter skeleton (shape only; the planner writes the final code)
```rust
// One rule for build and apply. State is only the bytes and the entry count since the last cut.
pub struct Splitter { size: usize, count: usize }
impl Splitter {
    pub fn new() -> Self { Self { size: NODE_HEADER, count: 0 } }
    /// `entry_len` is the encoded entry size; returns true if the node ends after this entry.
    pub fn push(&mut self, level: u8, key: &[u8], entry_len: usize) -> bool {
        let before = self.size;
        self.size += entry_len;
        self.count += 1;
        if level > 0 && self.count < 2 { return false; }          // no one-entry internal node
        if self.size > FORCE_AT { return true; }                  // NODE_MAX - MAX_ENTRY
        if self.size < NODE_MIN { return false; }
        let h = boundary_hash(level, key);                        // u32 from blake3(level+1 ++ key)
        (h as f64 / 4_294_967_295.0) < cut_probability(before, self.size)
    }
}
```
`cut_probability(start, end)` returns `1 - exp(-delta)` with `delta = (end^4 - start^4) as f64 / L^4` (u64 integer difference, basic-ops `exp`); it returns 1.0 when `start^4 / L^4` makes `1 - F(start)` zero (Dolt's `d <= 0` case, `start` at least about 6 x L, unreachable under `FORCE_AT`).

### Phase 22 smoke driver call (for the Docker target)
```sh
SMOKE=1 RUNS=1 WORK=/tmp/pp BIN=/target/release/prolly-bench timeout 600 bash /src/scripts/prolly-bench/bench.sh
```

## State of the Art

| Old Approach (issue #19 text) | Current finding | Impact |
|-------------------------------|-----------------|--------|
| "Today `commit()` serializes each map in full" and implied "stores each map in full" | It serializes and hashes each map in full, but `store_io` FastCDC-cuts every map, so new stored bytes are already small | The decision rests on CPU, RAM and scale, not on bytes stored (D-24) |
| "File manifests: a file is a prolly tree over its chunk hashes, with subtree lengths" | `file.rs` already stores manifests as content-cut trees with subtree byte lengths and splice-based edits | PROLLY-03 text: manifests need nothing |
| Dolt boundary rule "dynamic by node size" | Pinned: Weibull K=4, L=4096, min 512, max 16,384, hash over the key, level salt | Section "Boundary rule" |

**Manifest-tree facts for the written decision (D-25):** [VERIFIED: crates/chunk-store/src/file.rs:1-25, 32-47] the node header says "Node format: magic `SFM1`, level `u8`, entry count `u32 LE`, then entries of `ObjectId` (`ID_LEN` = 33 bytes: tag + digest) + byte length `u64 LE`"; "The nodes are cut by content, as the chunks are: a node ends after an entry whose ID matches a bit pattern (1 in `NODE_CUT_ONE_IN`), with at least `NODE_MIN` and at most `LEAF_ENTRIES` entries"; "So a chunk added or removed near the start of a file changes only the nodes up to the next cut (and their parents)"; `splice` makes the key of an edited content without a full re-chunk. Entries carry subtree byte lengths, so offset lookup is O(log n) and an edit rewrites one path. The cut there is by ID bits only (not size-aware), which is fine for 41-byte entries of nearly equal size. This is the statement PROLLY-03 requires.

**Deprecated/outdated:** the older assumption that each commit stores the full maps (issue #18/#19 era) no longer holds.

## Assumptions Log

| # | Claim | Section | Risk if Wrong |
|---|-------|---------|---------------|
| A1 | BLAKE3 of a short key costs about 50-100 ns | Boundary rule (key hash) | Bulk-build time estimate off; the bench prints the measured ns/key |
| A2 | Baseline new stored bytes for K changes (about 25 KB per changed map at K = 1; whole map at K = 1000 scatter) | Baseline | The report must quote the measured numbers; planning is not affected |
| A3 | `begin_commit` clones are about 4 MB per 84k-entry map | Baseline | RSS expectations only |
| A4 | Seed of the full Boost 1_84_0 takes minutes (not measured here) | Benchmark design | Plan time limits for the run plan; give each step a `timeout` and use the seed-once-per-WORK copy |
| A5 | Phase 23 (parallel track) may also change `Cargo.lock`, so a rebase conflict in `Cargo.lock` is possible | Wave constraints | Regenerate the lock with cargo after the rebase |
| A6 | The exact FastCDC resync after an 8-byte prefix shift dedups most chunks of `inode_map` against `inode_data` | Baseline Finding 1 | If it does not, the real-store `stats()` delta for `inode_data` is not near 0; the scratch-store design is right either way |
| A7 | Expected outcome of the benchmark (CPU and RSS gain per commit, small stored-byte difference, scale effect) | Summary | Not a recommendation; the owner decision rests on the measured report |

## Open Questions

1. **Which edit kinds and K share to use in the final script?**
   - What we know: the four kinds and the two patterns above are all expressible with the public API.
   - What's unclear: whether delete needs directory removal (only files created by the script are deleted, so no).
   - Recommendation: keep the recommended script; the planner may drop `content change` if the file-content put makes the run too slow.
2. **Whether the version step should be an in-place sync or a fresh seed of 1_85_0.**
   - Recommendation: in-place sync (it is the real "large commit"); the fresh-seed bulk row is a cheap extra (`flat` intern versus `prolly` build of 85,080 entries).
3. **Does the float-free `exp` need a table before the first release of the prototype?** No; golden test now, fixed-point only if adoption is chosen (Q-3).

## Environment Availability

| Dependency | Required By | Available | Version | Fallback |
|------------|------------|-----------|---------|----------|
| rustc / cargo | build and test | yes | 1.95.0-nightly | clippy gate uses the pinned stable in CI |
| python3 | median rows in `bench.sh` | yes | 3.9.6 | none needed |
| `/usr/bin/time` | peak RSS (`-l` on macOS) | yes | present | `NA` column |
| Boost 1_84_0 and 1_85_0 trees | the full run | yes, in the Phase 22 measure root outside the repo (both `x/boost_1_84_0` and `x/boost_1_85_0`) | 76,801 / 77,655 files | `scripts/macos-mount-test/fetch-boost.sh DIR 1_84_0 1_85_0`, else `SMOKE=1` (D-29) |
| Docker (OrbStack) | Linux smoke target | yes | server 29.8.2, 16 GB VM | skip the Linux smoke |
| Free disk | seeded store copy per run (about 1 GB) | yes | 519 GB free | — |
| macOS mount | not needed (D-07) | — | — | — |

**Missing dependencies with no fallback:** none.

## Wave and worktree constraints (D-05)

Files per plan (suggested split; no file is in two plans of one wave):

| Plan | Wave | Files it owns |
|------|------|---------------|
| 01 prolly crate | 1 | `Cargo.toml` (one members line `crates/prolly`), `Cargo.lock`, `crates/prolly/**` |
| 02 bench binary, driver, README, smoke | 2 | `Cargo.toml` (one members line `benchmarks/prolly-bench`), `Cargo.lock`, `benchmarks/prolly-bench/**`, `scripts/prolly-bench/**`, `docs/design/unsafe-review.md` (entry and count line), optionally `scripts/docker-linux-test/run.sh` (new `prolly-bench-smoke` target and its usage comment line) |
| 03 run, report, decision | 3 | `docs/research/prolly-trees.md`, `docs/design/metadata-maps.md`, `.planning/phases/24-prolly-evaluation/24-ISSUE-19-COMMENT.md`, `.planning/phases/24-prolly-evaluation/runs/**`; no code and no `Cargo` files |

A workspace member directory must have its `Cargo.toml` before the member line is added (cargo fails to load the workspace otherwise), so each plan adds its own member and its own directory together; two plans in one wave cannot both edit the members list. Plan 02 needs `prolly` to exist (depends on plan 01). `scripts/docker-linux-test/run.sh` and `docs/design/unsafe-review.md` are touched by no other plan of this phase (other phases may; rebase conflicts there are mechanical). Nothing in this phase changes `crates/metadata`, `crates/slicefs-cli`, `crates/chunk-store` or `docs/design/store-format.md`: a verify step is `git diff --stat <base>..HEAD -- crates/metadata crates/slicefs-cli crates/chunk-store docs/design/store-format.md` that must print nothing, plus `grep -n prolly crates/metadata/Cargo.toml crates/slicefs-cli/Cargo.toml` that must find nothing (D-19).

## Validation Architecture

> `workflow.nyquist_validation` is `true` in `.planning/config.json` [VERIFIED: file read this session].

### Test Framework
| Property | Value |
|----------|-------|
| Framework | Rust built-in test harness; `proptest` (workspace dep) for one property test |
| Config file | none; crate `Cargo.toml` only |
| Quick run command | `export PATH="$HOME/.cargo/bin:$PATH"; timeout 900 cargo test -p prolly </dev/null` |
| Full suite command | `export PATH="$HOME/.cargo/bin:$PATH"; timeout 900 cargo test -p prolly -p prolly-bench </dev/null` |
| Benchmark smoke (by hand, not in `cargo test`) | `SMOKE=1 RUNS=1 timeout 600 scripts/prolly-bench/bench.sh </dev/null` |
| Lint gate (CI) | `scripts/safety/clippy.sh` (clippy `-D warnings -D clippy::allow_attributes_without_reason`, allow-reason grep, `unsafe-count.sh`) |

### Phase Requirements to Test Map
| Req ID | Behavior | Test Type | Automated Command | File Exists? |
|--------|----------|-----------|-------------------|--------------|
| PROLLY-01 | Same set built by sorted bulk, by shuffled single-key `apply`, and by batches in two orders gives one root ObjectId | unit (MemIo, small config) | `cargo test -p prolly order_independence` | Wave 0 |
| PROLLY-01 | Value-independence: equal-length different value bytes give the same boundary keys per level (and different root ID) | unit | `cargo test -p prolly boundaries_ignore_value_bytes` | Wave 0 |
| PROLLY-01 | Every node is `<= NODE_MAX`, and every node but the last of a level is `>= NODE_MIN`; no internal node (except possibly the last of a level) has one entry; root is canonical | unit with an invariant walker | `cargo test -p prolly node_size_limits` | Wave 0 |
| PROLLY-01 | Variable-length keys and values: `(parent u64 BE, name)` keys, values 0..200 B; get, ordered iteration and `apply` equal a `BTreeMap` model; `apply` root equals bulk root | proptest (32-64 cases, small config) | `cargo test -p prolly variable_length_model` | Wave 0 |
| PROLLY-01 | Fixed-width: a same-width value change writes exactly `depth` new objects (`ChunkStore::stats()` delta) | unit | `cargo test -p prolly one_change_writes_one_path` | Wave 0 |
| PROLLY-01 | Bounded memory: build 2,000 and 40,000 entries with the small config; `high_water_bytes` is `<= depth_bound x NODE_MAX` and equal in both runs; cursor holds `<= depth` nodes; `apply` of 10 edits reads `<= 10 x depth` nodes | unit with counters | `cargo test -p prolly memory_is_bounded` | Wave 0 |
| PROLLY-01 | Golden vector: fixed key set gives a fixed root hex and a fixed table of `cut_probability` outputs (guards platform float drift) | unit | `cargo test -p prolly golden_root` | Wave 0 |
| PROLLY-01 | Canonical root: empty map, one key, delete back to the same as bulk | unit | `cargo test -p prolly canonical_root` | Wave 0 |
| PROLLY-01 | Corrupt or crafted node bytes return `Err`, never panic | unit | `cargo test -p prolly malformed_node_is_an_error` | Wave 0 |
| PROLLY-01 | The crate is not wired into the product | verify step | `grep -n prolly crates/metadata/Cargo.toml crates/slicefs-cli/Cargo.toml crates/chunk-store/Cargo.toml` finds nothing; `git diff --stat` of the three crates and `docs/design/store-format.md` is empty | n/a |
| PROLLY-02 | Bench builds in release; argument parsing and one tiny end-to-end (`gen` 200 files, `seed`, `whole` with 2 commits, `batches`, `flat`, `prolly`) in a tempdir | unit + one smoke test (a few seconds) | `cargo test -p prolly-bench` | Wave 0 |
| PROLLY-02 | Driver script end to end with generated data; rows and medians present | by-hand smoke | `SMOKE=1 RUNS=1 timeout 600 scripts/prolly-bench/bench.sh` then check the number of CSV rows and a `run=median` row per mode | Wave 0 |
| PROLLY-02 | Linux smoke | by-hand | `timeout 2400 scripts/docker-linux-test/run.sh prolly-bench-smoke` | Wave 0 (optional) |
| PROLLY-02 | Full run with Boost data, 3 runs | by-hand, time-limited | `WORK=... timeout 5400 scripts/prolly-bench/bench.sh DATA 3 > results.csv` | Plan 03 |
| PROLLY-02 | Bench never runs in `cargo test` | review check | `cargo test -p prolly-bench` finishes in seconds and reads no Boost data | n/a |
| PROLLY-03 | Research doc exists with method, tables, rule, crate survey, recommendation (adopt, later or drop) and the manifest statement | doc check | `grep -c -i "content-cut" docs/research/prolly-trees.md` is at least 1; `grep -E -c "adopt|later|drop" ...` at least 1 | Plan 03 |
| PROLLY-03 | Design-doc section exists and `store-format.md` is unchanged | doc check | `grep -n "prolly" docs/design/metadata-maps.md`; `git diff --stat -- docs/design/store-format.md` empty | Plan 03 |
| PROLLY-03 | #19 comment text file exists; no banned wording (D-27) in any new file | doc check | `test -s .planning/phases/24-prolly-evaluation/24-ISSUE-19-COMMENT.md`; the banned wording of D-27 (see CONTEXT) is absent from every new file | Plan 03 |
| (gate) | clippy, allow reasons, unsafe count | CI gate | `scripts/safety/clippy.sh` (long; or `cargo clippy -p prolly -p prolly-bench --all-targets -- -D warnings -D clippy::allow_attributes_without_reason` plus `scripts/safety/unsafe-count.sh`) | existing |

### Sampling Rate
- **Per task commit:** `cargo test -p prolly </dev/null` (or `-p prolly-bench`), under `timeout 900`.
- **Per wave merge:** `cargo test -p prolly -p prolly-bench` plus `scripts/safety/unsafe-count.sh` and the D-19/D-01 verify greps.
- **Phase gate:** the above, the `SMOKE=1` driver run, and a successful full run (or SMOKE plus a human-needed item per D-29) before `/gsd-verify-work`.

### Wave 0 Gaps
- [ ] `crates/prolly` (new crate; tests in `src/tests.rs` or `tests/`), covers PROLLY-01
- [ ] `benchmarks/prolly-bench` (new crate), `scripts/prolly-bench/bench.sh` and `README.md`, covers PROLLY-02
- [ ] `docs/research/prolly-trees.md`, `docs/design/metadata-maps.md`, `24-ISSUE-19-COMMENT.md`, covers PROLLY-03
- [ ] `docs/design/unsafe-review.md` entry for the bench's `getrusage` helpers
- Framework install: none (workspace `proptest` and `tempfile`).

Time budget for the tests: every prolly test uses `MemIo` and either the small config (L=256, min 32, max 1024, max entry 256; constants scaled together so trees get 3 or more levels with a few hundred entries) or at most a few thousand default-config entries, so the crate suite should run in a few seconds.

## Security Domain

> `security_enforcement` is not set in config, so it is enabled by default. This phase adds a library and a benchmark, with no network, auth, session or product path.

### Applicable ASVS Categories

| ASVS Category | Applies | Standard Control |
|---------------|---------|-----------------|
| V2 Authentication | no | no authentication in scope |
| V3 Session Management | no | none |
| V4 Access Control | no | none |
| V5 Input Validation | yes | node decode validates magic, count, lengths, key order, level; entry size limits; no panic on crafted bytes |
| V6 Cryptography | yes (use only) | `blake3` for the boundary hash (not a security use) and the store hasher for node IDs; nothing hand-rolled |
| V12 Files and resources | yes | the bench makes only its own `mktemp -d` dirs under `WORK`, refuses `/` and empty `WORK`, writes nothing in the data dir (Phase 22 rules) |

### Known Threat Patterns

| Pattern | STRIDE | Standard Mitigation |
|---------|--------|---------------------|
| Crafted node (from a damaged store) with huge counts or lengths | Tampering, DoS | Check lengths against the buffer before allocating; reject counts over a bound; return `Err` (as `file.rs` `decode_node` does) |
| Adversarial keys that force many cuts or none (boundary grinding) | DoS | The size-aware rule bounds node size (`FORCE_AT`, `MAX_ENTRY`); the key hash is not secret, so tree shape can be chosen by an attacker who chooses keys; note it in the decision doc under adoption risks (keys here are inode numbers and names chosen by the file-system user) |
| Bench script path handling | Tampering | Quote every path; no `rm -rf` of a path the script did not make (Phase 22 pattern) |

## Sources

### Primary (HIGH confidence)
- `dolthub/dolt` raw source at commit `d5a833f8df56e35c8fffccb48cfa5d790726aa57`: `go/store/prolly/tree/node_splitter.go`, `go/store/prolly/tree/chunker.go` (read as raw files this session)
- This repo, read this session: `crates/metadata/src/store.rs` (lines 36-57, 262-310, 330-413, 852-877, 1049-1149, 1403-1560, 1567-1676, 1761-1769), `crates/metadata/src/store_io.rs`, `crates/metadata/src/inode_map.rs`, `crates/metadata/src/directory.rs`, `crates/chunk-store/src/file.rs` (1-120, 740-775), `crates/chunk-store/src/lib.rs` (ObjectId, Options, Stats, `put`, `read`, `stats`, constants), `crates/slicefs-cli/src/seed.rs`, `snapshot.rs`, `content.rs`, `lib.rs`, `benchmarks/index-bench/src/main.rs`, `crates/chunk-store/examples/chunk_profile_bench.rs`, `scripts/chunk-profiles/bench.sh` and `README.md`, `scripts/docker-linux-test/run.sh`, `scripts/safety/clippy.sh`, `scripts/safety/unsafe-count.sh`, `docs/design/unsafe-review.md`, `docs/research/chunk-profiles.md`
- crates.io API (versions, dates, downloads) and gsd-tools `package-legitimacy check`
- doc.rust-lang.org/std/primitive.f64.html (`exp_m1`: "The precision of this function is non-deterministic")
- GitHub issue #19 (read only)

### Secondary (MEDIUM confidence)
- docs.rs pages of `prolly-map` 0.7.2 (`trait.Store.html`, `docs/architecture.md`) and `prollytree` 0.4.1 (`trait.NodeStorage.html`, README), fetched through a summarizing tool: fine for the trait signatures; the pages are silent on the boundary rule and streaming build, which is not used as evidence

### Tertiary (LOW confidence)
- None used for a recommendation.

## Metadata

**Confidence breakdown:**
- Standard stack: HIGH, no new package; the three surveyed crates are rejected for a structural reason (node identity)
- Boundary rule: HIGH for Dolt's constants and algorithm (raw source), HIGH for the model numbers (own simulation, tagged), MEDIUM for the BLAKE3 cost (assumed, measured by the bench)
- Baseline bytes: HIGH for serialized bytes (code and file counts), MEDIUM-LOW for new-stored-bytes estimates (assumed FastCDC behavior; measured by the bench)
- Pitfalls: HIGH (each traced to code or the gate scripts)

**Research date:** 2026-11-26
**Valid until:** 2026-12-26 (the repo changes; the Dolt commit is pinned)
