# Phase 25: Prolly format and layout - Discussion Log

> **Audit trail only.** Do not use as input to planning, research, or execution agents.
> Decisions are captured in CONTEXT.md — this log preserves the alternatives considered.

**Date:** 2026-11-26
**Phase:** 25-prolly-format-and-layout
**Areas discussed:** Map layout (Q-2), Boundary hash salt, Flag vs format number, Rule constants freeze, Scale target

---

## Map layout (Q-2)

| Option | Description | Selected |
|--------|-------------|----------|
| Test both, then decide | Wire both layouts into the bench; Claude picks by commit time and nodes touched, documents, reports | ✓ |
| Per-map now | One tree for each map, with no test | |
| Combined now | One tree keyed by inode, with no test | |

**User's choice:** Test both, then decide.

| Option | Description | Selected |
|--------|-------------|----------|
| Yes, prolly too | Refcounts also become a prolly map | ✓ |
| Keep flat for now | Commit stays O(store) for refcounts | |

---

## Boundary hash salt

| Option | Description | Selected |
|--------|-------------|----------|
| No salt | Equal node IDs across stores; grinding is an accepted risk | |
| Optional salt setting | Default no salt; header setting with its own flag | ✓ |
| Always salt | Stops grinding; no cross-store node sharing | |

| Option | Description | Selected |
|--------|-------------|----------|
| At seed only | Salt fixed at seed | |
| Seed, and a later rebuild | Also a command that rebuilds all maps with a new salt | ✓ |

**Notes:** The rebuild command is a new capability, so it is deferred. Phase 25 does the salt at seed only.

---

## Flag vs format number

| Option | Description | Selected |
|--------|-------------|----------|
| Incompat flag, required | Format 7 + `incompat prolly-meta v2.4` | |
| Version replaces number | Header carries the SliceFS release; policy rewritten | |
| Format 8 | Increase the format number | ✓ |

**Notes:** The user first asked: "Can that be a slicefs version?" Claude explained that flag lines already carry the release word, and that the format number is separate from the release number by policy. The user then chose format 8.

---

## Rule constants freeze

| Option | Description | Selected |
|--------|-------------|----------|
| Keep Dolt values | 512 B / 16 KiB, L=4096, K=4 | |
| Retune in the Q-2 test | Also try a larger L (for example 8192), pick by bench | ✓ |

| Option | Description | Selected |
|--------|-------------|----------|
| You decide | Claude picks the table resolution, documents it, golden test | ✓ |
| Exact match to float | Same cut points as the Phase 24 float rule | |

---

## Scale target

**User's input:** "For global decentralized file system and potentially billions of files/dirs stored..."

| Option | Description | Selected |
|--------|-------------|----------|
| 1M + 10M, extrapolate | Measure Boost, 1M, 10M; extrapolate to 1B | ✓ |
| Up to 100M measured | Hours, tens of GiB | |
| 1B measured | Days, hundreds of GiB | |

| Option | Description | Selected |
|--------|-------------|----------|
| Yes, design for 1B+ | Bounded memory at any size, written as a rule | ✓ |
| Note only | Background only | |

## Claude's Discretion

- Fixed-point table resolution and integer math
- Which of `inode_map` / `inode_data` stays
- Final layout and constants after the test (report to owner)
- Location of the rule version byte; class of the salt flag

## Deferred Ideas

- Salt rebuild command
- 100M / 1B measured runs
- Global namespace identity
- Snapshot diff by equal subtrees
