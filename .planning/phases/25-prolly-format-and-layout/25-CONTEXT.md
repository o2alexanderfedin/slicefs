# Phase 25: Prolly format and layout - Context

**Gathered:** 2026-11-26
**Status:** Ready for planning

<domain>
## Phase Boundary

Phase 25 fixes the on-disk format of the prolly metadata maps (#215), and nothing more:

- the map layout (Q-2 of #19), decided by a test and written down;
- one inode map in place of `inode_map` + `inode_data`;
- a fixed-point boundary rule with a version byte, with retuned constants;
- an optional boundary salt set at seed;
- store format 8.

The commit, read and GC paths on prolly maps are Phase 26 (PMAP-03, 06, 07, 08). Phase 25 writes the format and the docs, and makes the new build write and mount format 8 stores. A store of format 8 written at the end of Phase 25 can have a format that changes again in Phase 26, because no release comes between the two phases.

</domain>

<decisions>
## Implementation Decisions

### Design target
- **D-01:** Design for 1,000,000,000 inodes (files and directories) or more in one store. The goal is a global, decentralized file system. Hard rules for Phases 25 and 26: no operation loads a whole map into RAM (not mount, `stats`, GC or scrub). Memory stays bounded at any store size. Inode and byte counts come from the counts in the internal entries, or from the root record, never from a full walk. Write this target in `docs/design/store-format.md`. — **Reversibility:** costly — later code that loads a whole map breaks the rule at scale.

### Map layout (Q-2)
- **D-02:** Q-2 is decided by a test at the start of Phase 25, not by a guess. Wire both layouts into `benchmarks/prolly-bench`: (a) one prolly map for each metadata map, (b) one combined map with keys such as `(ino, kind, …)`. Measure commit time, CPU, nodes read and written, and bytes per commit on Boost, on 1M and on 10M synthetic inodes. Use mixed commits (one change, K=100, K=1,000, insert/delete heavy).
- **D-03:** Claude picks the layout from the test, writes the numbers and the reason in `docs/design/store-format.md` (with the effect on the root record and on the GC mark), and reports to the owner. The owner delegated this choice ("Test both, then decide"). — **Reversibility:** one-way after the format freeze (#156); before the freeze it is costly (a new format and a copy migration via #155).
- **D-04:** `refcounts` is a prolly map too, like all the other metadata maps. Then a one-change commit is O(change). #165 (refcount removal) can drop the map later.
- **D-05:** Extrapolate the test to 1B entries by tree height and node count, and write the method and the result in the doc. A 100M or 1B run is not part of this phase.

### One inode map (PMAP-02)
- **D-06:** `inode_map` and `inode_data` both map an inode number to an `ObjectKey`. Only one stays. Claude picks which one. `next_ino` moves to the root record (if the kept map does not carry it).

### Boundary rule (PMAP-04)
- **D-07:** Replace the float test (`exp_neg`, `cut_probability` in `crates/prolly/src/rule.rs`) with a fixed-point table and integer math. The rule carries a version byte, kept in the node and/or the root record (Claude decides where). A golden test with fixed input gives the same cut points and root ID on Linux and macOS.
- **D-08:** Retune the constants in the Q-2 test. Run the Dolt values (L=4096, K=4, `NODE_MIN` 512 B, `NODE_MAX` 16 KiB) against at least one larger L (for example 8192, a mean node of about 7.4 KB, near the 8 KiB chunk average). Weigh node count at 1B entries: each node is one chunk-index object. Pick by the bench, and write the chosen constants and the reason in `docs/design/store-format.md`. — **Reversibility:** one-way after the freeze — the constants decide every node ID, so a change rewrites every map.
- **D-09:** The prolly node format (magic, level, count, entries) becomes part of the store format and is documented in `docs/design/store-format.md`. This reverses Phase 24 D-03/D-14/D-15, where the node bytes were private to the crate.

### Boundary salt
- **D-10:** Add an optional per-store boundary salt. The default is no salt, so node IDs are equal across stores (for global dedup). `slicefs seed` gets an option (for example `--boundary-salt random`) that writes a salt setting line into the header. By the format rule ("a new setting line always comes with a flag"), the line comes with its own flag. Claude picks the class: a build that does not know the salt can read the trees but must not write them, so ro_compat fits. The salt is part of the boundary hash input. Record grinding as an accepted risk for unsalted stores (the harm has a limit: no node above `NODE_MAX`, the tree never breaks). — **Reversibility:** one-way per store — a salt change changes every node ID.
- **D-11:** A later change of the salt (a rebuild of all maps with a new salt, similar to `rehash`) is wanted by the owner. It is a new command, so it is deferred (see Deferred Ideas). Phase 25 sets the salt at seed only.

### Format mark (PMAP-05)
- **D-12:** Store format 8. The owner chose this over an incompat flag on format 7. The new build refuses format 7 stores with a clear message that points to `docs/user/upgrade.md` (#155). Release `v2.3.0` refuses format 8 with its existing "other format number" message. A test covers both directions. — **Reversibility:** one-way — a published format number.
- **D-13:** Update the format policy in `docs/design/store-format.md`. Today it says the number changes only when an older SliceFS cannot parse the header. Add that the number also changes for a full change of the metadata layout (format 8, prolly maps), and record that the owner decided this. Reword PMAP-05 in `.planning/REQUIREMENTS.md` and success criterion 4 of Phase 25 in `.planning/ROADMAP.md` from "new incompat flag" to "format 8" (done in the commit of this context).

### Claude's Discretion
- The resolution of the fixed-point table and the integer math (D-07). It must match the float rule closely (a test measures the error), be documented, and be pinned by a golden test.
- Which of `inode_map` / `inode_data` stays (D-06).
- The final layout after the test (D-03), and the final constants (D-08). Report both.
- Where the rule version byte lives (D-07), and the class of the salt flag (D-10).

</decisions>

<canonical_refs>
## Canonical References

**Downstream agents MUST read these before planning or implementing.**

### Prolly evaluation and prototype
- `docs/research/prolly-trees.md` — Phase 24 results, boundary rule (§Boundary rule), node storage, adoption changes (§What adoption would change), risks, owner questions Q-1..Q-3, rubric
- `crates/prolly/src/lib.rs` — constants (`NODE_MIN`, `NODE_MAX`, `MAX_ENTRY`, `MAX_KEY`, `WEIBULL_L`, `WEIBULL_K`, `NODE_MAGIC`), `Config`
- `crates/prolly/src/rule.rs` — `boundary_hash`, `exp_neg`, `cut_probability` (the float rule to replace)
- `crates/prolly/src/node.rs`, `tree.rs`, `apply.rs`, `tests.rs` — node format, build, apply, golden tests
- `benchmarks/prolly-bench/src/` and `scripts/prolly-bench/bench.sh` — the bench to extend for the Q-2 and constants test

### Store format
- `docs/design/store-format.md` — header, flag classes, flag list, format policy (§Format policy), messages
- `crates/slicefs-cli/src/header.rs` — header parse, `KNOWN_FLAGS`, format number

### Issues
- GitHub #215 — scope and "done when" of the prolly adoption
- GitHub #19 — the evaluation, Q-1..Q-3
- GitHub #155 — the copy migration (format 7 → 8), Phase 27
- GitHub #165 — refcount removal (later milestone)

### Planning
- `.planning/REQUIREMENTS.md` — PMAP-01, 02, 04, 05
- `.planning/ROADMAP.md` — Phase 25 success criteria

</canonical_refs>

<code_context>
## Existing Code Insights

### Reusable Assets
- `crates/prolly`: generic prolly map on `ChunkStore` (put/read by `ObjectId`), with order-independence and size-bound tests. It is the base for the product maps.
- `benchmarks/prolly-bench`: flat vs prolly bench with Boost and scale rows. Extend it for layout (a)/(b) and the L values.
- `crates/metadata/src/store_io.rs`: `NodeRead` / `NodeWrite`, `put_value` (today map nodes go through a FastCDC `FileWriter`).

### Established Patterns
- Header settings come with a flag; flag lines are `<class> <name> <release>` (`store-format.md`).
- Messages M5/M9 and the refuse paths in `header.rs`. The format 7 → 8 refusal goes there too.
- Golden tests pin root IDs and cut tables, and run on macOS and Linux (plan 24-04).

### Integration Points
- `crates/metadata/src/store.rs`: `inode_data` and `inode_map` (lines ~290, ~346, ~449), the `inode_live` shards, and the root record write and load.
- `crates/metadata/src/inode_map.rs`: `InodeMap`, `next_ino`, `intern_inode_map` / `load_inode_map`.
- `crates/metadata/src/gc/`, `crates/metadata/src/rehash/`: these walk the maps. They change in Phase 26, but the layout choice must keep a bounded walk possible.
- `crates/metadata/src/snapshot.rs`: snapshots hold map roots.

</code_context>

<specifics>
## Specific Ideas

- Owner's frame: "global decentralized file system and potentially billions of files/dirs stored". Scale is a first-class input to the layout and constants choice.
- The owner asked whether the format mark can be a SliceFS version. Answer given: flag lines already carry the release word, but the format number stays separate from the release number. The owner then chose format 8.
- Bench runs go in Docker with disk-backed storage (no tmpfs), with time limits on all waits.

</specifics>

<deferred>
## Deferred Ideas

- **Salt rebuild command:** rebuild all prolly maps of a store with a new boundary salt (similar to `rehash`). The owner wants this. It needs a new phase or an issue.
- **100M / 1B measured runs:** a real large-scale run of the prolly store. Separate task, hours to days.
- **Global namespace identity:** inode numbers and `(dir ino, name)` keys are local to one store, so cross-store sharing of metadata nodes works only for identical subtrees. A global identity design is its own topic.
- **Snapshot diff by equal subtrees:** a later gain of prolly roots (from `prolly-trees.md`).

</deferred>

---

*Phase: 25-prolly-format-and-layout*
*Context gathered: 2026-11-26*
