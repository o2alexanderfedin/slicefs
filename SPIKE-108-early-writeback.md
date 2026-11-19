# Spike #108: start pack writeback early (dropped)

Result: not kept. Starting kernel writeback early did not clearly cut the slowest write.

## Change

On Linux, after every 1 MiB of contiguous writes to a pack, `sync_file_range(SYNC_FILE_RANGE_WRITE)` on that range (`crates/chunk-store/src/pack_io.rs`). The periodic fsync stays for durability. Nothing on macOS. A second commit logs kicks of 5 ms or more as `[kick]`.

## Bench note

`bigwrite` keeps its store under `/tmp/bw`, which is overlayfs in the container. There `sync_file_range` returns 0 and starts no writeback (256 MiB dirty: 0.2 ms kick, dirty pages unchanged, fsync 362 ms after). On a btrfs Docker volume the kick starts writeback (252 ms kick, dirty 283 to 51 MiB, fsync 2 ms after). So all four runs below mounted a btrfs volume at `/tmp/bw`, the same for base and change.

## Results (Docker `run.sh bigwrite 1024 4096`, base 3f3ff34, runs interleaved)

| | base1 | base2 | chg1 | chg2 |
|---|---|---|---|---|
| 4 GiB max write (ms) | 462 | 766 | 4941 | 459 |
| 4 GiB max pack wait (ms) | 523 | 720 | 4857 | 138 |
| 4 GiB pack syncs: sum s / max ms | 9.6 / 858 | 7.1 / 1201 | 14.1 / 5080 | 6.2 / 824 |
| 1 GiB max write (ms) | 127 | 1427 | 401 | 347 |
| 1 GiB max pack wait (ms) | 139 | 1357 | 783 | 51 |
| VmHWM after cmp, 1 / 4 GiB (MiB) | 115 / 177 | 130 / 164 | 135 / 167 | 107 / 161 |

The kick itself blocked writes for up to 305 ms. Single runs vary much (1 GiB base: 127 to 1427 ms). The decision in #108 was to keep the RAM cap (`KEEP_CAP`): a disk that is slower than the writes stalls the writer.
