storage: locality cache for the on-disk chunk index

Deferred from #85 (on-disk chunk index, PR #109).

## Problem

The dedup check of `put` looks up each chunk ID in the on-disk index. A hit checks the filter of each run and reads one 4 KiB page of a run file. The runs are sorted by digest, so the entries of the chunks of one file are spread over all pages of a run. A cache of recent run pages thus gives almost no hits.

When a file is written again (for example a copy of a tree that is already in the store), its chunks are often in the same pack, in the same order as before. The `.idx` file of that pack holds their entries together.

## To examine

- A small cache in RAM of the entries of recently hit packs. After a hit in pack P, the store reads the `.idx` of P and keeps its entries, so the next dedup checks of the same file are found in RAM.
- A bound on the cache (for example a fixed number of packs), so the RAM of the process stays bounded.
- GC: the cache must never give an object that GC removed. For example, the store drops the cache at each GC install, or checks the index generation counter.

## Measure first

- The hit rate of the cache and the run page reads for each put, on a copy of a Boost source tree that is already in the store.
- The put dedup-hit time with and without the cache (`benchmarks/index-bench`, 10M and 100M objects).
- The RAM of the cache.

If the measurements show no gain, close this issue with the numbers.
