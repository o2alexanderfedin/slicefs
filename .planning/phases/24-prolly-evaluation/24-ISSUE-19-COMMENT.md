Phase 24 measured prolly trees for the metadata maps. Report: `docs/research/prolly-trees.md`.

What was built: a prototype prolly map (`crates/prolly`, not used by the product) with Dolt's size-aware key splitter, one chunk-store object per node, and a benchmark (`benchmarks/prolly-bench`, `scripts/prolly-bench/bench.sh`). The benchmark seeds Boost 1_84_0, runs 80 small commits (1 to 1,000 changed files, scattered and clustered), the 1_85_0 tree as one commit, and 40 commits of 1 to 1,000 real inserts and deletes in the middle of the map. It compares the real commit with a flat `inode_data` map and a prolly `inode_data` map. 3 runs, macOS; a Linux Docker run gives the same answer, except one row: with 100 inserts and deletes per commit, prolly is not better there, so a Linux-only reading of the rubric would give drop. These are the numbers of a second run, after a code review of the benchmark.

Headline numbers (median, one changed file, 84,104 inodes):

- Real commit (all six maps): 63.9 ms, 42.9 CPU ms; it serializes and hashes 10.3 MB, but stores only 36.3 KB.
- `inode_data` flat: 6.6 CPU ms, 3,448.3 KB serialized, 15.4 KB new stored; process peak RSS 175.4 MiB.
- `inode_data` prolly: 0.6 CPU ms, 3.3 KB serialized, 2.6 KB new stored (3 nodes written, 6 read); process peak RSS 33.7 MiB.
- Estimated real commit with a prolly `inode_data`: 58.4 ms (91.4 %). About 4 to 8 ms less per small commit. For 1,000 changes and for the version step, prolly is slower.
- Real inserts and deletes in the middle of the map (they move node boundaries): at 100 per commit, prolly 10.1 CPU ms against flat 12.5 on macOS, and 13.4 against 12.7 on Linux (prolly a little slower there). At 1,000, prolly 46.3 against flat 24.2 CPU ms.
- Scale: with 4,000,000 entries, one change costs flat 242.1 ms and prolly 22.0 ms. One flat map passes 100 ms per commit at about 1.6 million entries.
- Disk over 121 commits (before GC, pack index included): the flat store grew by 64.4 MiB, the prolly store by 38.4 MiB.

Two facts that change the question of this issue:

- File manifests are already content-cut trees (`crates/chunk-store/src/file.rs`): nodes cut by chunk ID, entries with subtree byte lengths, an edit rewrites one path. No prolly tree is needed for file content.
- Metadata map nodes are already FastCDC-cut values (`store_io::put_value`). A commit does not store the full maps. It serializes and hashes them in full, and that is the cost that is left.

Recommendation: **later** (the same as after the first run). At the Boost scale the gain is about 4 to 8 ms per commit: below the 10 ms limit of the rubric (and below 20 % on macOS). With real inserts and deletes the gain is smaller, and at 100 of them per commit it is gone on Linux. The gain becomes large at about a million inodes and more. A smaller first step: `inode_map` and `inode_data` hold the same entries, so one of them can go (that also changes the root record, so it is an owner decision too).

Owner questions (open, not decided by this phase):

- Q-1: adopt prolly maps (a store format change), and in which milestone? Recommendation: later, when stores of about a million inodes or more are a target.
- Q-2: one prolly map per metadata map, or one combined namespace map? Suggestion: decide it with Q-1.
- Q-3: do the boundary rule and its constants become part of the frozen store format? Suggestion: yes if adopted, with a fixed-point table in place of the float test and a version byte for the rule.
