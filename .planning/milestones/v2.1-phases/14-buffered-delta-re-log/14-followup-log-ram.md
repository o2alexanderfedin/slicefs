Title: Replay at mount loads all write-log records after the last commit into RAM

At mount, SliceFS reads every write-log record after the last root into one list in RAM. Only then does it apply the records (`load_store_with_log` in `crates/metadata/src/segment/mod.rs`, called from `crates/slicefs-cli/src/mount.rs`). This RAM grows with the bytes written since the last commit. It does not grow with the file size. The default commit interval is 30 s. With `--commit-interval 0` there is no timed commit, so the log can grow until unmount.

What is already bounded (#98):
- While the daemon runs, the write log uses little RAM. The per-op mode writes each record to the segment file at once. The flush-on-fsync and periodic modes keep at most 4 MiB of records (`BUFFER_CAP_BYTES`).
- When replay applies the records, each file with random writes uses an overlay with the dirty limit.

Only the load of the records before replay is not bounded.

Proposal: read the segment files twice. The first pass finds the position of the last root. The second pass reads the records after that position one at a time and applies each record before it reads the next one.

Acceptance: a test logs much more than a fixed cap (for example 256 MiB of writes with no commit), then crashes and replays. The peak memory of the replay stays below the cap, and the content is the same.
