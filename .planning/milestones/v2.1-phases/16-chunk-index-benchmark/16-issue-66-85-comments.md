# Comments for #66 and #85 (Phase 16 close-out)

## #66

Results of the chunk index benchmark.

Tool: `benchmarks/index-bench` (commit 321a443). It makes stores of small synthetic objects (8 bytes each, so zstd does not run) with the default options (64 MiB packs), and measures each step in a new process. Page cache: warm (one warm-up open before the measured opens; median of 3). Machine: Xeon W-3245, 16 cores (32 threads), 192 GB RAM, macOS 26.2, APFS on an internal SSD. Linux: Docker (OrbStack) on the same host, kernel 7.2.12, btrfs. Other work kept the host load high during the runs (load average 80 to 140 on macOS), so the times are an indication; the sizes and counts are exact.

| OS | Objects | Open s | Peak RSS B/object | Steady RSS B/object | contains hit ns | contains miss ns |
|---|---|---|---|---|---|---|
| macOS | 1M | 0.96 | 390 | 390 | 1305 | 727 |
| macOS | 10M | 12.8 | 256 | 196 | 701 | 324 |
| macOS | 100M | 141 | 154 | 106 | 820 | 420 |
| Linux | 1M | 0.60 | 302 | 153 | 695 | 266 |
| Linux | 10M | 10.4 | 193 | 123 | 887 | 305 |

- RAM per object at 100M: about 106 B (steady) and about 154 B (peak, while the map grows). The old estimate in the design note was wrong; the note now has the measured values. On macOS the value at small sizes is higher because the map is not sized before the fill.
- Open time grows linearly with the object count: 0.96 s at 1M, 12.8 s at 10M, 141 s at 100M.
- A sealed pack without an index is checked record by record (CRC and BLAKE3) at about 46 to 48 MB/s of CPU time, and gets a new index. With all indexes missing, the 100M read-write open takes 292 s. Because of the load, the scan rate is computed from CPU time, not wall time.
- The 1M store has one pack, so its "one pack" recovery is the active-pack path (CRC check only, no new index).
- 100M was not run on Linux: the Docker VM had 12.6 GB of memory free, and the run needs about 25 GB.
- Write throughput of the create step (for #85): about 68,000, 64,000 and 72,000 puts/s on macOS at 1M, 10M and 100M. The creator process used 16.3 GB of peak RSS at 100M.

All tables and the method: `docs/design/chunk-store.md`, section "Index baseline (#66)", and `benchmarks/index-bench/README.md`.

These numbers are the baseline for #85. The acceptance items of this issue (cold-open time, peak RSS and missing-index recovery at 1M, 10M and 100M objects; RAM per object and scan throughput) are measured and documented, so this issue is closed. The work on a smaller index continues in #85.

## #85

Baseline for this issue: {link to the #66 comment}, and `docs/design/chunk-store.md`, section "Index baseline (#66)". Today, at 100M objects, the open takes 141 s and the index uses about 106 B per object (steady) and 154 B per object (peak). The same tool (`benchmarks/index-bench`) runs against the new index in this issue, and the results are compared with these tables. Compute the scan rate from CPU time, as the baseline does.
