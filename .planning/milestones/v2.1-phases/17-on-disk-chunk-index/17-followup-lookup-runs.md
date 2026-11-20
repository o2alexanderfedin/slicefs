storage: fewer index runs for each lookup of the on-disk chunk index

From #85 (on-disk chunk index, PR #109).

## Problem

Lookups with the on-disk index are slower than the #66 baseline (the map in RAM). A lookup checks the memtable, then each run from the newest to the oldest: a filter check for each run, and for a hit one page read. The size-tiered merges (fan-in 4) keep 3 runs at 1M objects, 5 at 10M and 9 at 100M (`docs/design/chunk-store.md`, section "Index results (#85)", Open table).

A separate measurement, not in the doc tables: 10M objects, macOS, one process, the second lookup pass after a first pass.

| Index | Runs | Hit µs | Miss µs |
|---|---|---|---|
| As created | 5 | 1.03 | 0.50 |
| After `merge_index_all` | 1 | 0.80 | 0.31 |
| #66 baseline (map in RAM) | - | 0.70 | 0.32 |

The first pass in a new process was about 0.25 µs slower, because of page faults on the mapped run pages.

At 100M, the lookup step of `index-bench` gave a hit of 3.2 µs and a miss of 1.2 µs with 9 runs (#66: 0.82 µs and 0.42 µs). That step is one pass in a new process, so page faults are part of these values. At 100M, the share of the run count and the share of the page faults are not measured.

## To examine

- A merge policy that keeps fewer runs. Examples: a full merge when the store is idle, or a leveled policy.
- The cost of each policy in rewritten bytes. The index is about 60 B for each object, thus about 6 GB at 100M objects, and a full merge rewrites all of it.
- A bench change: `index-bench lookup` reports a second, warm lookup pass in the same process, next to the first pass. Then the effect of the run count and the effect of the page faults can be separated.

## Measure

- Hit, miss and put dedup-hit times at 10M and 100M, first and second pass, for each policy.
- The run count after create and after a long write load.
- The bytes that the merges write for each byte of index data.

No policy is chosen yet. The measurements decide.
