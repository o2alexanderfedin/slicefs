---
phase: 25-prolly-format-and-layout
review: 25-REVIEW.md
critical: 0
warning: 0
info: 6
---

# Phase 25 — Code Review Disposition

The review found 0 Critical and 0 Warning findings, so the fix loop did not run. The table records what happens to each Info finding.

| ID | Finding | Disposition | Reason |
|----|---------|-------------|--------|
| IN-01 | `begin_commit` reads `next_ino` before `take_all()`; safe only under the commit gate | deferred to Phase 26 | Same order as before this phase; Phase 26 rewrites the commit path for prolly maps |
| IN-02 | Format 8 header says prolly maps, but this build still writes flat maps | deferred to Phase 26 | No deployed stores (no backward compatibility); Phase 26 puts the prolly maps in before release `v2.4.0`, and its root record check must refuse stores made by this interim build cleanly |
| IN-03 | `--boundary-salt random` on an existing store gives the text "is another salt" | open | Small message fix; take it with the next touch of `content.rs` |
| IN-04 | `Config` derives `Debug`, so `{:?}` prints the salt | open | The salt is not a secret (it is plain text in `FORMAT`), low value |
| IN-05 | Bench defaults `--l` to 4096; product is now 8192 | open | The matrix script passes both values; change the default with the Phase 26 bench work |
| IN-06 | `InodeAlloc` derives `Clone` but nothing uses it | open | Dead derive, no effect |
