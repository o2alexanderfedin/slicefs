# Phase 14: deferred items

## From plan 14-04

### Chunk store keeps unsynced pack bytes in RAM (up to one pack, 64 MiB)

- **Status:** resolved

**Resolved by 0285b2a** ("fix(chunk-store): bound the RAM copy of unsynced pack writes (#98)"): `RetryingPack` keeps at most `KEEP_LIMIT` unsynced bytes and syncs when it passes that limit (test in `crates/chunk-store/tests/retry_tests.rs`). The text below is the original record.

- **Where:** `crates/chunk-store/src/retry.rs`, `RetryingPack::write_at`: `self.unsynced.push((offset, data.to_vec()))`. `ChunkStore::open` wraps every pack in this retry layer (`crates/chunk-store/src/lib.rs:452`). The list is cleared only by a sync: a commit (`pre_root_sync` -> `ChunkStore::sync`) or a pack seal (`lib.rs:899`, at `pack_limit` = 64 MiB).
- **What:** every chunk byte written since the last store sync stays in RAM, so that a failed sync can write it again. A file of less than 64 MiB that is streamed in sits fully in RAM until the next commit (default interval 30 s). A rebase adds the bytes of its new chunks.
- **Evidence:** counting-allocator test (`memory_bound_tests.rs`). Without a store sync after setup, the baseline was 17000 KiB (16 MiB random file) and 51137 KiB (48 MiB random file), against 215 KiB for the repeating file (it dedups). The commit in the middle of the workload freed these bytes.
- **Not in the 11 sites:** this buffer existed before phase 14, and it is outside the files of plan 14-04. Its size has a fixed limit (one pack), and it does not grow with the file size. But it touches the hard rule ("never a whole file in RAM") for files below 64 MiB, and it sets a floor for the Docker VmHWM cap of plan 14-07.
- **Test choice:** the memory test syncs the store after setup on purpose (comment in the test). Its window still holds the bytes that the workload's rebases retain (about 2.8 MiB per rebase in the test).
- **Options for the orchestrator:** a sync when the retained bytes pass a smaller limit (for example 8 MiB), or a sync at the end of each rebase. Reading the bytes back from the file is not an option: after a failed sync, Linux can mark the pages clean.
